// Hook events from Claude Code and every other agent → island state.
// Port of HookServer.processEvent / processPermissionRequest from the macOS app.
// Difference from macOS: no terminal filter. On Windows the hook fires from any
// terminal (Windows Terminal, VS Code, PowerShell…) and all of them are handled.
// The relay has already mapped every agent's events and fields onto Claude
// Code's (hook/src/normalize.rs), so one handler serves them all.
//
// Which pill an event goes to and what it means for its turn is decided in
// core/sessions.ts; the requests that wait for an answer queue in
// core/approvals.ts. Nothing in this file answers a request: it only takes a
// card down (its turn ended, its time ran out, it was answered in the terminal)
// and, when it does, the relay is told "no decision".

import { Bridge, onEvent } from "../core/bridge";
import { fileName, makeDiffStep, toOneLine } from "../core/diff";
import { buildFileDiffs } from "./patch-diff";
import { isGitPush, setGitPush } from "../core/github";
import { Sound } from "../core/sound";
import { State, type AgentTask, type AskedQuestion } from "../core/state";
import { judge, mayQuit, modelLabel, promptStep, QUIT_AFTER_MS, SILENT_MS, type Session, type Verdict } from "../core/sessions";
import { APPROVAL_AGENTS, QUESTION_AGENTS, agentColor, agentName, isClaudeAlias, noteFinished, validateAgent } from "./agents";
import type { Island } from "./island";
import { parseClaudePlan, restorePlanUsage } from "../core/plan";
import { setClaudePlanUsage, storedClaudePlanUsage } from "../views/usage";
import { N_, t } from "../i18n/i18n";

const CLAUDE_ID = "integration_claude";
const CURSOR_ID = "agent_cursor";
const JETBRAINS_ID = "agent_jetbrains";

/** How long a request waits here before its terminal has taken over (the relay gives up at 108 s). */
const CARD_MS = 110_000;
/** Each waiting request is withdrawn when its own time is up, counted from its arrival. */
const timeouts = new Map<string, number>();

/** A `git push` of a session shows on the GitHub card, and the card refreshes when it ends. */
function noteGitPush(name: string, payload: HookPayload) {
  if (!isGitPush(payload.tool_name ?? "", payload.tool_input ?? {})) return;
  setGitPush(name === "PreToolUse" ? "pushing" : name === "PostToolUse" ? "pushed" : "failed");
  State.notify();
  if (name !== "PreToolUse" && State.tasks.some((x) => x.id === "integration_github")) void Bridge.refreshIntegration("integration_github");
}

/**
 * Takes one waiting request down, by its id: its turn ended, its time ran out,
 * or it was answered in the terminal. Never an answer: with `tellRelay` the
 * relay is released without a decision, and the agent asks in its terminal.
 * The next request in the queue, if any, comes up on the card.
 */
function withdraw(island: Island, requestId: string, tellRelay = false): void {
  const timer = timeouts.get(requestId);
  if (timer != null) window.clearTimeout(timer);
  timeouts.delete(requestId);
  if (!State.approvals.has(requestId)) return;
  if (tellRelay && requestId) void Bridge.approvalDecline(requestId);
  const wasHead = State.pendingApproval?.requestId === requestId;
  State.endApproval(requestId);
  const onCard = State.view === "approval" || State.view === "question";
  if (!State.pendingApproval) {
    island.dropPin();
    if (onCard) island.setView(State.defaultView());
  } else if (wasHead && onCard) {
    island.setView(State.defaultView());
  }
  State.notify();
}

/** The return to idle that Stop arms, per pill, so the next turn can cancel it. */
const stopTimers = new Map<string, number>();

function cancelStopTimer(id: string): boolean {
  const timer = stopTimers.get(id);
  if (timer == null) return false;
  window.clearTimeout(timer);
  stopTimers.delete(id);
  return true;
}

/** The pill rests `ms` from now, unless its session speaks first. */
function restAfter(id: string, ms: number) {
  cancelStopTimer(id);
  stopTimers.set(
    id,
    window.setTimeout(() => {
      stopTimers.delete(id);
      State.updateTask(id, "idle");
      State.setPillBadge(id, null);
    }, ms),
  );
}

interface HookPayload {
  hook_event_name?: string;
  request_id?: string;
  session_id?: string;
  cwd?: string;
  message?: string;
  /** UserPromptSubmit carries `prompt`; `message` belongs to Notification/Stop. */
  prompt?: string;
  /** Stop: the turn's final answer (Markdown) — Claude Code's, Hermes's, Codex's. */
  last_assistant_message?: string;
  tool_name?: string;
  tool_input?: Record<string, unknown>;
  /** Set by the relay when an edit was too big to forward whole (> 256 KB). */
  lintel_diff_truncated?: boolean;
  /** Optional agent tag: lowercase, digits and hyphens, ≤ 24 chars. */
  lintel_agent?: string;
  /** Hermes: where the session runs (telegram, discord…; "cli" in a terminal). */
  platform?: string;
  /** "cursor" or "jetbrains" when Claude Code runs in that editor's terminal (set by the relay). */
  term_editor?: string;
  /** The title of the session's terminal window, when the app found the window (Claude Code writes its one-line summary there). */
  lintel_window_title?: string;
  /** StatusLine (the plan usage relay): Claude Code's 5-hour and weekly limits. */
  rate_limits?: unknown;
  /** PermissionRequest: the rules Claude Code offers to remember (the Always button). */
  permission_suggestions?: unknown;
  /** The folder the session started in and its /rename title, from Claude Code's own record (the relay reads it). */
  session_root?: string;
  session_title?: string;
  /** Stop: the window the session runs in is the one in front (Rust adds it). */
  lintel_front?: boolean;
  /** Set on every event fired inside a subagent. */
  agent_id?: string;
  /** SubagentStop: empty for the CLI's own helpers (prompt suggestions), which are not the session's work. */
  agent_type?: string;
  /** Stop: what still runs in the background (subagents, shells, monitors). */
  background_tasks?: unknown;
  /** Notification: `permission_prompt`, `elicitation_dialog`, `idle_prompt`… */
  notification_type?: string;
  /** The agent's own turn id, when it numbers its turns (Codex). */
  turn_id?: string;
  /** SessionStart: `startup`, `resume`, `clear`, `compact`. */
  source?: string;
  /** What the session runs with, for its chips. */
  permission_mode?: string;
  model?: unknown;
  effort?: unknown;
}

function lastPathComponent(p: string): string {
  const cleaned = p.replace(/[\\/]+$/, "");
  const idx = Math.max(cleaned.lastIndexOf("\\"), cleaned.lastIndexOf("/"));
  return idx >= 0 ? cleaned.slice(idx + 1) : cleaned;
}

/**
 * localizedStep() — same labels as the macOS app, in English and shown in the
 * interface language (src/i18n) when the step is recorded.
 */
const TOOL_LABELS: Record<string, string> = {
  Bash: N_("Runs"),
  Read: N_("Reads"),
  Write: N_("Writes"),
  Edit: N_("Edits"),
  Glob: N_("Searches"),
  Grep: N_("Searches"),
  WebSearch: N_("Searches the web"),
  WebFetch: N_("Fetches"),
  TodoWrite: N_("Tasks"),
  Task: N_("Agent"),
  LS: N_("Lists"),
  MultiEdit: N_("Edits"),
  NotebookEdit: N_("Notebook"),
  PowerShell: N_("Runs"),
  // Antigravity's tools (#298).
  run_command: N_("Runs"),
  view_file: N_("Reads"),
  write_to_file: N_("Writes"),
  replace_file_content: N_("Edits"),
  read_url_content: N_("Fetches"),
  search_web: N_("Searches the web"),
  ask_question: N_("Needs input"),
  multi_replace_file_content: N_("Edits"),
  list_dir: N_("Lists"),
  // Cursor's shell, Codex's patch, Kiro CLI's tools.
  Shell: N_("Runs"),
  apply_patch: N_("Edits"),
  execute_bash: N_("Runs"),
  shell: N_("Runs"),
  fs_read: N_("Reads"),
  read: N_("Reads"),
  fs_write: N_("Edits"),
  write: N_("Edits"),
};

/** Kiro's file tools: their `command` names the operation ("str_replace"), it is not a shell line. */
const FILE_TOOLS = new Set(["fs_read", "read", "fs_write", "write"]);

/** Antigravity's question tool, bare or behind a server prefix. */
const isAsk = (tool: string) => tool === "ask_question" || tool.endsWith(":ask_question");

function stepLabel(tool: string, input: Record<string, unknown>): string {
  if (isAsk(tool)) tool = "ask_question";
  const label = TOOL_LABELS[tool] ? t(TOOL_LABELS[tool]) : tool;
  const str = (k: string) => (typeof input[k] === "string" ? (input[k] as string) : null);
  const asked = Array.isArray(input.questions) ? (input.questions[0] as { question?: unknown } | undefined)?.question : null;
  if (tool === "ask_question" && typeof asked === "string" && asked.trim()) return `${label} · ${toOneLine(asked).slice(0, 80)}`;
  // Codex's patch: the first file it touches.
  const patched = tool === "apply_patch" ? str("command")?.match(/^\*\*\* (?:Update|Add|Delete) File: (.+)$/m)?.[1] : null;
  if (patched) return `${label} · ${lastPathComponent(patched.trim())}`;
  const cmd = FILE_TOOLS.has(tool) ? null : str("command");
  if (cmd) return `${label} · ${cmd.slice(0, 40)}`;
  const path = str("path");
  if (path) return `${label} · ${lastPathComponent(path)}`;
  const file = str("file_path");
  if (file) return `${label} · ${lastPathComponent(file)}`;
  const query = str("query");
  if (query) return `${label} · ${query.slice(0, 40)}`;
  return label;
}

/**
 * What the Allow button actually authorises. Approving "Write" tells you nothing
 * — approving `Write · C:\…\.env` tells you everything, and the difference is
 * the whole point of approving from the island rather than blind.
 *
 * Ordered by how specific the field is, so an unfamiliar tool still shows
 * whatever identifying string it carries instead of falling back to its name.
 */
const APPROVAL_FIELDS = [
  "command", // Bash, PowerShell
  "file_path", // Write, Edit, MultiEdit, NotebookEdit
  "path", // Read, LS
  "url", // WebFetch
  "query", // WebSearch
  "pattern", // Glob, Grep
  "prompt", // Task
] as const;

function approvalTarget(tool: string, input: Record<string, unknown>): string {
  // A plan does not fit on the card: its title, the first line without its heading marks.
  if (tool === "ExitPlanMode" && typeof input.plan === "string") {
    return input.plan.split("\n").map((line) => line.replace(/^[\s#]+/, "").trim()).find(Boolean) ?? tool;
  }
  for (const field of APPROVAL_FIELDS) {
    const value = input[field];
    if (typeof value === "string" && value.trim()) {
      return `${tool} · ${value.trim()}`;
    }
  }
  return tool;
}

/**
 * The questions of an AskUserQuestion call, if the island can show all of them
 * as options to pick from. Anything it cannot is left to the terminal.
 */
function askedQuestions(tool: string, input: Record<string, unknown>): AskedQuestion[] | null {
  if (tool !== "AskUserQuestion" || !Array.isArray(input.questions)) return null;
  const out: AskedQuestion[] = [];
  for (const raw of input.questions as Record<string, unknown>[]) {
    const question = typeof raw?.question === "string" ? raw.question : "";
    const options = (Array.isArray(raw?.options) ? (raw.options as Record<string, unknown>[]) : [])
      .filter((o) => typeof o?.label === "string" && o.label)
      .map((o) => ({
        label: o.label as string,
        description: typeof o.description === "string" ? o.description : "",
      }));
    // A question cut short by the relay would be answered under the wrong text,
    // and two with the same text would share one answer.
    if (!question || question.endsWith("…") || options.length < 2) return null;
    if (out.some((q) => q.question === question)) return null;
    out.push({ question, options, multiSelect: raw.multiSelect === true });
  }
  // Claude Code asks one to four at a time; anything else is not ours to guess.
  return out.length > 0 && out.length <= 4 ? out : null;
}

/**
 * PostToolUse of Edit / MultiEdit / Write → a diff stored for the pill and a
 * ticker step with its +N −M counts. Nothing is kept for a pill that does not
 * exist, so a stray event cannot grow memory. An edit the relay had to cut
 * would give wrong counts: the PreToolUse step ("Edits · file") stands alone.
 */
function recordDiff(agentId: string, payload: HookPayload) {
  if (payload.lintel_diff_truncated) return;
  if (!State.tasks.some((t) => t.id === agentId)) return;
  // One diff per file: Codex's patch can change several.
  for (const diff of buildFileDiffs(payload.tool_name ?? "", payload.tool_input ?? {}, payload.cwd ?? "")) {
    const id = State.appendSessionDiff(agentId, diff);
    State.appendStep(agentId, makeDiffStep(fileName(diff.path), diff.added, diff.removed, id));
  }
}

/** The final message stays on the card until the next turn starts. */
function clearFinal(id: string) {
  const task = State.tasks.find((x) => x.id === id);
  if (!task) return;
  task.finalLine = null;
  task.finalText = null;
}

/** The conversation was cleared: the pill rests, with nothing left of the old turn. */
function restNow(id: string) {
  const task = State.tasks.find((x) => x.id === id);
  if (!task) return;
  task.state = "idle";
  task.steps = [];
  task.stepIndex = 0;
  delete task.stepSeq;
  task.pillBadge = null;
  task.finalLine = null;
  task.finalText = null;
}

/**
 * Option (Settings › Claude Code): another session's finish or error opens its card for a
 * moment, then the pill you were on comes back. Never over an approval or a question, an
 * island you are using, or a call. True when the card went up.
 */
function peek(island: Island, id: string, view: "finished" | "error"): boolean {
  const back = State.focusId;
  const asking = State.pendingApproval || State.tasks.some((x) => x.state === "approval" || x.state === "question");
  if (!State.settings.peekFinished || !back || back === id || asking || State.mode === "expanded" || Sound.hushed) return false;
  State.setFocus(id);
  island.alert(view);
  window.setTimeout(() => {
    if (State.focusId === id && !State.pendingApproval) State.setFocus(back);
  }, 6000);
  return true;
}

/** A session is gone (ended, silent for half an hour, or the thirteenth): its pill with it. */
function release(island: Island, session: Session) {
  for (const a of State.approvals.ofTurn(session.family, session.id, Infinity)) withdraw(island, a.requestId, true);
  cancelStopTimer(session.pillId);
  windowTitles.delete(session.id);
  State.clearSessionDiffs(session.pillId);
  // A kept pill (the main one, a declared one) goes back as it was; any other goes away.
  State.removeTask(session.pillId);
}

const text = (v: unknown): string => (typeof v === "string" ? v.trim() : "");

/** The last terminal title heard per session, so an event without one keeps the pill's name. Bounded. */
const windowTitles = new Map<string, string>();

/**
 * The terminal window's title as the pill shows it. Claude Code writes its summary
 * there behind a state glyph (✳ ⠂ …), which the pill already conveys; a title without
 * that glyph is the shell's own (or another tab's), and says nothing about the session.
 */
function cleanWindowTitle(raw: string | undefined): string | null {
  if (!raw || /^[\p{L}\p{N}]/u.test(raw)) return null;
  const title = raw.replace(/^[^\p{L}\p{N}]+/u, "").trim();
  if (!title || title === "Claude Code" || title.length > 120) return null;
  return title;
}

/** What the session says it runs with: its permission mode, effort and model (the session chips). */
function noteChips(pillId: string | undefined, payload: HookPayload): boolean {
  const task = pillId ? State.tasks.find((x) => x.id === pillId) : null;
  if (!task) return false;
  const before = `${task.mode}~${task.effort}~${task.model}`;
  const obj = (v: unknown) => (v && typeof v === "object" ? (v as Record<string, unknown>) : {});
  const mode = text(payload.permission_mode);
  const effort = text(payload.effort) || text(obj(payload.effort).level);
  const model = text(obj(payload.model).display_name) || modelLabel(text(payload.model) || text(obj(payload.model).id));
  if (mode) task.mode = mode;
  if (effort) task.effort = effort;
  if (model) task.model = model;
  return before !== `${task.mode}~${task.effort}~${task.model}`;
}

// ── Timers that follow the sessions ───────────────────────────────────────────

/** One timer for the moment every session has been silent for half an hour; an event before then prunes as it goes. */
let pruneTimer: number | null = null;

function armPrune(island: Island) {
  if (pruneTimer != null) window.clearTimeout(pruneTimer);
  pruneTimer = null;
  const heard = State.sessions.lastHeard();
  if (heard == null) return;
  pruneTimer = window.setTimeout(() => {
    pruneTimer = null;
    for (const gone of State.sessions.prune(Date.now())) release(island, gone);
    State.notify();
  }, Math.max(0, heard + SILENT_MS - Date.now()));
}

/** The relay started the app (the opt-in of Settings › Agents): it leaves once the agents are quiet. */
let autoLaunched = false;
let quitTimer: number | null = null;

function armQuit() {
  if (!autoLaunched) return;
  if (quitTimer != null) window.clearTimeout(quitTimer);
  quitTimer = window.setTimeout(() => {
    quitTimer = null;
    const quiet = mayQuit({
      autoLaunched,
      now: Date.now(),
      lastHeard: State.sessions.lastHeard(),
      lastUsed: Date.now() - (performance.now() - State.lastActivity),
      busy: State.tasks.some((x) => x.state === "working" || x.state === "thinking" || x.state === "searching"),
      waiting: State.approvals.size,
      islandShown: State.mode !== "hidden",
    });
    if (quiet) void Bridge.quit();
    else armQuit();
  }, QUIT_AFTER_MS);
}

export function registerHookHandlers(island: Island) {
  // The last plan numbers seen survive a restart, as on the Mac.
  State.planUsage ??= restorePlanUsage(storedClaudePlanUsage());
  void onEvent<HookPayload>("hook", (payload) => handleHook(island, payload));
  // The relay hung up while its card was up: the prompt was answered in the
  // terminal, so the card goes at once, with nobody left to release.
  void onEvent<string>("hook-resolved", (requestId) => withdraw(island, requestId));
  // Sessions already running get their pills at once, not at their next event.
  void Bridge.runningSessions().then((found) => {
    for (const [session_id, cwd] of found ?? []) handleHook(island, { hook_event_name: "SessionStart", source: "found", session_id, cwd });
  });
  void Bridge.autoLaunched().then((yes) => {
    autoLaunched = yes === true;
    armQuit();
  });
}

function handleHook(island: Island, payload: HookPayload) {
  // Account-wide numbers from the status line relay, not part of any session:
  // keep the latest, nothing else (no reveal, no sound), paused or not. The
  // same line says which model and effort its session runs with.
  if (payload.hook_event_name === "StatusLine") {
    const usage = parseClaudePlan(payload.rate_limits);
    if (usage) setClaudePlanUsage(usage);
    // The status line is drawn again and again: only a change redraws the island.
    if (noteChips(State.sessions.byId(payload.session_id ?? "")?.pillId, payload)) State.notify();
    return;
  }

  if (State.paused) {
    // Silence here used to cost Claude Code nearly two minutes: the relay waited
    // for a decision from an island that had already decided not to look. Say so,
    // and the terminal takes the question immediately.
    if (payload.request_id) void Bridge.approvalDecline(payload.request_id);
    return;
  }

  const name = payload.hook_event_name ?? "";
  // The CLI's own helpers (prompt suggestions) are not the session's work.
  if (name === "SubagentStop" && payload.agent_type === "") return;
  const cwd = payload.cwd ?? "";
  // The session's /rename title, else the folder it started in (not the one after a `cd`).
  // With the terminal's title in between: Claude Code keeps a one-line summary of the session there.
  const sessionKey = payload.session_id ?? "";
  const heard = cleanWindowTitle(payload.lintel_window_title);
  if (heard && sessionKey) windowTitles.set(sessionKey, heard);
  const windowTitle = heard ?? (windowTitles.get(sessionKey) || "");
  const raw = text(payload.session_title) || windowTitle || lastPathComponent(text(payload.session_root)) || lastPathComponent(cwd);
  const projectName = (raw || "Session");

  // Route to the right agent. Valid lintel_agent → that agent's "agent_<name>".
  // "claude" is reserved; absent or invalid → Claude Code's own pill: Cursor's
  // when it runs in Cursor's terminal (Mac #120), VS Code's otherwise.
  const validAgent = validateAgent(payload.lintel_agent);
  const workspaceId = payload.term_editor === "cursor" ? CURSOR_ID : payload.term_editor === "jetbrains" ? JETBRAINS_ID : CLAUDE_ID;
  const family = validAgent ? `agent_${validAgent}` : workspaceId;
  const isExternalAgent = validAgent !== null;
  // `--agent claude-<name>` is Claude Code with a pill of its own: its cards and questions as usual.
  const isClaudeCode = !isExternalAgent || isClaudeAlias(validAgent);
  const sessionId = payload.session_id ?? "";

  // Then to the session's pill: the agent's own for its first session, one of
  // its own for every other session running beside it (core/sessions.ts).
  const now = Date.now();
  for (const gone of State.sessions.prune(now)) release(island, gone);
  const { session, dropped } = State.sessions.touch(family, sessionId, now);
  for (const gone of dropped) release(island, gone);
  const agentId = session.pillId;
  const ownPill = agentId !== family;
  armPrune(island);
  armQuit();

  // What the event means for its turn. A late one (a turn that is over, an
  // older turn of an agent that numbers them) changes nothing at all.
  const turnBefore = session.turn;
  const backgroundBefore = session.background;
  const verdict: Verdict = name === "SessionEnd" ? "end" : judge(session, {
    name,
    turnId: text(payload.turn_id) || undefined,
    subagent: text(payload.agent_id) !== "",
    background: Array.isArray(payload.background_tasks) ? payload.background_tasks.length : 0,
    prompt: payload.prompt ?? payload.message,
  });
  if (verdict === "late") return;

  /** Alerts force the island open; work events only reveal the compact island. */
  const surface = (view: Parameters<Island["alert"]>[0], isAlert: boolean) => {
    if (State.mode === "expanded") {
      if (isAlert) island.setView(view);
    } else if (isAlert) {
      island.alert(view);
    } else if (State.mode === "hidden") {
      island.reveal();
    }
  };

  const task = (): AgentTask | undefined => State.tasks.find((x) => x.id === agentId);

  /** Ensure the session's pill exists, named after its folder: an event that names none leaves the name it has. */
  const ensurePill = () => {
    let made: AgentTask | null | undefined;
    if (ownPill) {
      const label = raw || task()?.name || (isExternalAgent ? agentName(validAgent!) : projectName);
      made = State.upsertSessionPill(agentId, family, label, isExternalAgent ? agentColor(validAgent!) : "");
    } else if (isExternalAgent) {
      State.upsertExternalAgent(agentId, agentName(validAgent!), agentColor(validAgent!));
      made = task();
    } else {
      made = State.upsertWorkspacePill(agentId, raw || task()?.name || projectName, cwd);
    }
    if (made && cwd) made.sessionCwd = cwd;
    if (made && sessionId) made.sessionId = sessionId;
  };

  /**
   * Called where a handler is about to replace `finished` with a newer state:
   * the timer Stop armed would otherwise put the pill back to idle over it. The
   * badge that timer was going to clear goes now.
   */
  const supersedeStop = () => {
    if (cancelStopTimer(agentId)) State.setPillBadge(agentId, null);
  };

  /** At work, unless one of its requests still waits: then the pill goes on saying so. */
  const atWork = () => {
    const waits = State.approvals.all().find((a) => a.pillId === agentId);
    State.updateTask(agentId, waits ? (waits.questions ? "question" : "approval") : "working");
  };

  // The turn that asked for a permission is over — answered in the terminal,
  // interrupted, or a new prompt — so its card would be lying. It goes, and the
  // relay is released without a decision. Matched by agent, session and turn:
  // another session's turn, or a turn that goes on in the background, takes
  // nothing down.
  const over =
    name === "SessionEnd" || verdict === "finish" || verdict === "quiet" || verdict === "end" || verdict === "error" ||
    (name === "UserPromptSubmit" && session.turn !== turnBefore);
  if (over) {
    const upTo = name === "SessionEnd" ? Infinity : turnBefore;
    for (const a of State.approvals.ofTurn(family, sessionId, upTo)) withdraw(island, a.requestId, true);
  }

  // Read after the cards above are dropped: their pill may have handed the
  // front back to the pill you were on.
  const focused = State.focusId === agentId;

  switch (name) {
    case "SessionStart": {
      ensurePill();
      clearFinal(agentId);
      // Found running at launch (src-tauri hooks.rs): a pill, no sound and no island.
      if (payload.source === "found") break;
      // `/clear` and `/compact` start the session again: the old turn is gone,
      // and there is nothing to announce.
      if (payload.source === "clear" || payload.source === "compact") {
        cancelStopTimer(agentId);
        restNow(agentId);
        break;
      }
      // Hermes through its gateway says where the session comes from.
      if (validAgent === "hermes" && payload.platform && payload.platform !== "cli") {
        State.appendStep(agentId, payload.platform.charAt(0).toUpperCase() + payload.platform.slice(1));
      }
      surface("overview", false);
      Sound.play("work");
      break;
    }

    case "UserPromptSubmit": {
      // `/clear` typed in the terminal: the conversation is gone and no turn follows.
      if (verdict === "end") {
        cancelStopTimer(agentId);
        restNow(agentId);
        break;
      }
      ensurePill();
      supersedeStop();
      clearFinal(agentId);
      // A finished or failed card on screen is about the turn before: back to the overview.
      if (focused && State.mode === "expanded" && (State.view === "finished" || State.view === "error")) island.setView("overview");
      State.updateTask(agentId, "thinking");
      // The field is `prompt`; reading `message` meant this step was always blank.
      // A prompt the CLI wrote itself (`<task-notification>`) is not the user's words.
      // A slash command shows as typed (`/compact`), not as the tags the CLI echoes it in.
      const asked = promptStep(payload.prompt ?? payload.message);
      if (asked) State.appendStep(agentId, asked);
      surface("overview", false);
      break;
    }

    case "PreToolUse": {
      ensurePill();
      supersedeStop();
      clearFinal(agentId);
      // The strip comes out when work starts, not for every tool: idle stays idle (§5.2).
      const started = task()?.state !== "working";
      atWork();
      noteGitPush(name, payload);
      const tool = payload.tool_name ?? "Tool";
      // A subagent's tool is shown as its own; the session's last tool stays the session's.
      State.appendStep(agentId, (verdict === "sub" ? "↳ " : "") + stepLabel(tool, payload.tool_input ?? {}));
      const made = task();
      if (made && verdict !== "sub") made.lastTool = tool;
      const cmd = payload.tool_input?.command;
      if (made && typeof cmd === "string" && cmd) made.lastCommand = cmd.slice(0, 300);
      // Antigravity asks a question in its terminal: the pill says it needs input, with the sound and the island out.
      if (isAsk(tool) && verdict !== "sub" && !State.approvals.all().some((a) => a.pillId === agentId)) {
        State.updateTask(agentId, "question");
        Sound.play("question");
        if (!State.pendingApproval && !Sound.hushed) surface("overview", true);
      } else if (started) surface("overview", false);
      break;
    }

    case "PostToolUse":
    case "PostToolUseFailure": {
      supersedeStop();
      noteGitPush(name, payload);
      // The tool ran, or was refused: a request for it that still waits here was
      // answered (or dismissed) in the terminal, and its card would be lying.
      const tool = payload.tool_name ?? "Tool";
      const settled = State.approvals.answeredBy({
        family, sessionId, tool, command: approvalTarget(tool, payload.tool_input ?? {}),
      });
      if (settled) withdraw(island, settled.requestId, true);
      atWork();
      if (name === "PostToolUse") recordDiff(agentId, payload);
      else State.appendStep(agentId, t("⚠ failed"));
      break;
    }

    case "Notification": {
      const message = payload.message ?? "";
      const lower = message.toLowerCase();
      const hasCard = State.approvals.all().some((a) => a.pillId === agentId);
      if (lower.includes("rate limit") || lower.includes("limite d")) {
        supersedeStop();
        State.updateTask(agentId, "ratelimit");
        Sound.play("rate");
      } else if (payload.notification_type === "permission_prompt") {
        // Asked in the terminal with no card here (it timed out, or this agent
        // gets none): the pill says it waits, and nothing is answered from here.
        if (hasCard) break;
        ensurePill();
        supersedeStop();
        State.updateTask(agentId, "approval");
        Sound.play("approval");
        waitingNotice(island, task(), focused);
      } else if (payload.notification_type === "elicitation_dialog" || message.endsWith("?")) {
        if (hasCard) break;
        supersedeStop();
        State.updateTask(agentId, "question");
        if (message) State.appendStep(agentId, message);
        waitingNotice(island, task(), focused);
      }
      break;
    }

    case "Stop": {
      if (verdict === "sub") break;
      if (verdict === "background") {
        // The agent said its piece but its background tasks run on: still
        // working, and what it said is not its last word.
        ensurePill();
        supersedeStop();
        State.updateTask(agentId, "working");
        State.appendStep(agentId, t("In background: {count}", { count: session.background }));
        break;
      }
      State.updateTask(agentId, "finished");
      // Claude Code puts the turn's answer in the Stop payload itself, so there is
      // no transcript to read (the relay does not even forward its path). Other
      // agents report their last words the same way (Hermes, Codex) or as
      // `message`.
      const whole = payload.last_assistant_message ?? payload.message ?? "";
      const finalText = toOneLine(whole);
      if (finalText) {
        State.appendStep(agentId, finalText);
        const made = task();
        if (made) {
          made.finalLine = finalText;
          made.finalText = whole;
        }
      }
      // One notice for a turn: a second stop of the same turn changes the pill
      // and stays silent.
      const announce = verdict === "finish";
      // Its terminal is the window in front: the user sees it finish, so a badge is all it gets.
      const watched = announce && payload.lintel_front === true;
      if (announce && !watched) Sound.play("finish");
      noteFinished(task());
      // A card waiting for an answer is never covered by another alert.
      if (announce && !watched && focused && !State.pendingApproval && !Sound.hushed) surface("finished", true);
      else if (announce && !watched && !focused && peek(island, agentId, "finished")) {
        // its card is up for a moment
      } else if (!focused || State.pendingApproval || watched || Sound.hushed) State.setPillBadge(agentId, "finished");
      // The pill stays for the next turn: it goes at the session's end, or
      // after half an hour of silence.
      restAfter(agentId, 5200);
      break;
    }

    case "Interrupt":
      // Codex: the user stopped the turn. Back to idle, nothing to celebrate.
      supersedeStop();
      State.updateTask(agentId, "idle");
      State.setPillBadge(agentId, null);
      break;

    case "StopFailure":
      supersedeStop();
      State.updateTask(agentId, "error");
      Sound.play("error");
      if (focused && !State.pendingApproval && !Sound.hushed) surface("error", true);
      else if (!focused && peek(island, agentId, "error")) {
        // its card is up for a moment
      } else State.setPillBadge(agentId, "error");
      break;

    case "SessionEnd":
      // Nothing left for the timer to do, and it must not outlive the session.
      State.sessions.end(family, sessionId);
      release(island, session);
      armPrune(island);
      break;

    case "SubagentStart":
      State.appendStep(agentId, t("+ subagent"));
      break;

    case "SubagentStop":
      State.appendStep(agentId, t("• subagent done"));
      // Every background agent has reported. The agent normally wakes and says
      // so; if it never does, the pill rests instead of working for ever.
      if (backgroundBefore > 0 && session.background === 0) restAfter(agentId, 60_000);
      break;

    case "PermissionRequest": {
      // Only Claude Code and the agents the relay can answer for (Codex, Copilot
      // CLI, Muse Code, and Claude Code run from the Claude desktop app) get a
      // card. Anyone else's request is declined at once, so the agent asks in
      // its own terminal.
      if (isExternalAgent && !isClaudeCode && !APPROVAL_AGENTS.has(validAgent!)) {
        if (payload.request_id) void Bridge.approvalDecline(payload.request_id);
        break;
      }

      const requestId = payload.request_id ?? "";
      const tool = payload.tool_name ?? "Tool";
      const input = payload.tool_input ?? {};
      // Claude Code asking a question is not a permission to grant: the island
      // shows the options and sends back the one that was picked. Only Claude
      // Code asks questions this way, from a terminal or from its desktop app.
      const asks = isClaudeCode || QUESTION_AGENTS.has(validAgent!);
      const questions = asks ? askedQuestions(tool, input) : null;
      const view = questions ? "question" : "approval";
      ensurePill();
      // First come, first served: a request that arrives while another is on
      // the card waits behind it, and never replaces it. Beyond five, the
      // newest one goes straight back to its terminal.
      const taken = State.beginApproval({
        requestId,
        family,
        sessionId,
        turn: session.turn,
        pillId: agentId,
        tool,
        command: approvalTarget(tool, input),
        ...(questions ? { questions } : {}),
        ...(Array.isArray(payload.permission_suggestions) && payload.permission_suggestions.length > 0 ? { suggested: true } : {}),
      });
      if (taken === "full") {
        if (requestId) void Bridge.approvalDecline(requestId);
        break;
      }
      // The relay's short ack window closes in 800 ms; everything below this
      // line is synchronous, so the card (or its place in the count on the
      // card) really is up by the time it lands.
      if (requestId) void Bridge.approvalAck(requestId);
      // The same request sent twice: acknowledged again, nothing else.
      if (taken === "known") break;
      // These agents report the tool only as this request (no PreToolUse of its own): it is the turn's step too.
      if (validAgent === "kiro" || validAgent === "antigravity") State.appendStep(agentId, stepLabel(tool, input));
      supersedeStop();
      State.updateTask(agentId, view);
      Sound.play(view);
      // Lintel answers within 108 s or not at all; after that the terminal has
      // taken over and the card would be lying. Each request has its own time.
      timeouts.set(requestId, window.setTimeout(() => withdraw(island, requestId), CARD_MS));
      // The card always comes up, even over another pill or an island that is
      // already open: its pill comes to the front, and the one you were on
      // comes back once you answer (Mac #117, #120). One that waits behind
      // another card only changes the count on it.
      if (State.pendingApproval?.requestId === requestId) island.alert(view);
      break;
    }

    default:
      break;
  }
  noteChips(agentId, payload);
  State.notify();
}

/**
 * A session waits for the user without a card here: a short notice says so,
 * unless its pill is what the open island already shows.
 */
function waitingNotice(island: Island, task: AgentTask | undefined, focused: boolean) {
  if (!task || (focused && State.mode === "expanded")) return;
  island.api("agents").notice({ kind: "agent", title: task.name, detail: t("Waiting for you"), icon: "bell", tone: "warn" });
}
