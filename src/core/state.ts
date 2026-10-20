// App state — mirror of AppState.swift (the parts the island needs).

import type { BotEmoteName, BotStateName, IslandMode, IslandViewName } from "./layout";
import type { Face } from "../mascot/engine";
import {
  DEFAULT_MAIN_PILL, HOST_OS, availablePills, orderPills, pillDefinition, sanitizeDeclared,
  toggleDeclared, type HostOs, type PillDefinition,
} from "./pills";
import type { CodexPlanUsage, PlanUsage } from "./plan";
import type { ProviderId } from "./providers";
import type { FileDiff } from "./diff";
import type { Bindings } from "./shortcuts";
import { pillColor } from "./pill-colors";
import { ApprovalQueue, type ApprovalInfo } from "./approvals";
import { Sessions } from "./sessions";

export type { ApprovalInfo, AskedQuestion } from "./approvals";

export type AgentSource = "claudeCode" | "n8n" | "agent";
export type PillBadge = "approval" | "finished" | "error";

export interface AgentTask {
  id: string;
  name: string;
  color: string;
  state: BotStateName;
  stepIndex: number;
  steps: string[];
  /**
   * Position of the newest step in the whole session. `steps` is capped, so
   * `stepIndex` stops moving once it is full; this keeps counting.
   */
  stepSeq?: number;
  source: AgentSource;
  isIntegration: boolean;
  emote?: BotEmoteName | null;
  miniEye?: Face | null;
  pillBadge?: PillBadge | null;
  sessionCwd?: string | null;
  /** Claude Code's session, so "Open terminal" can find the window it runs in. */
  sessionId?: string | null;
  /** Claude's final message after Stop, one line; cleared when a new turn starts. */
  finalLine?: string | null;
  /** The same message whole, as the Markdown it was written in (the finished card). */
  finalText?: string | null;
  /**
   * The agent's own pill, for a pill made for one more session of it
   * (`session_<n>`): what it looks like and where "Open" leads. Read it with `familyOf`.
   */
  family?: string;
  /** The tool the session used last ("Bash", "Edit"). */
  lastTool?: string | null;
  /** The last shell command the session ran, for the code view. */
  lastCommand?: string | null;
  /** What the agent says about itself, when it does: the session chips. */
  model?: string | null;
  mode?: string | null;
  effort?: string | null;
}

/** The catalog pill a task belongs to: its own id, or its family's for a session's own pill. */
export function familyOf(task: { id: string; family?: string }): string {
  return task.family ?? task.id;
}

/** The pill is waiting for the user: a permission, a question. */
export function needsYou(task: AgentTask): boolean {
  return task.state === "approval" || task.state === "question" || task.pillBadge === "approval";
}

export interface ChatMessage {
  id: number;
  role: "user" | "assistant";
  content: string;
}

export type PromptContext =
  | { kind: "window"; appName: string; title: string; url?: string }
  | { kind: "file"; name: string; path?: string };

export interface ResultItem {
  label: string;
  detail: string;
  url?: string;
}

export interface SearchResult {
  title: string;
  items: ResultItem[];
  note?: string;
}

/** A fresh, idle task for a catalog pill, in the colour the user gave it if any. */
function taskFor(def: PillDefinition, colors: unknown, name = def.name): AgentTask {
  return {
    id: def.id, name, color: pillColor(def.id, def.color, colors), state: "idle", stepIndex: 0, steps: [],
    source: def.source, isIntegration: true,
  };
}

/** What an integration poller last reported. */
export interface IntegrationInfo {
  data: Record<string, unknown>;
  error: string | null;
  loaded: boolean;
  configured: boolean;
}

export interface Settings {
  soundEnabled: boolean;
  soundVolume: number;
  autoCloseInterval: number;
  /** Resting on the peek strip opens the island (off: only a click does). */
  openOnHover: boolean;
  /** Off: every mascot is a dot in the pill colour with a ring for its state. */
  showMascot: boolean;
  /** The tray's mascot does what the island's does; off: one still picture. Absent in an older file: on. */
  trayAnimate?: boolean;
  /** Lintel's own Desktop and Start menu shortcuts wear the VS Code pill's colour too. Absent: on. */
  iconShortcuts?: boolean;
  /** A 1.5 px outline around the island. */
  islandOutline: boolean;
  /** An active live activity keeps the peek strip on screen instead of showing it for 3 s. */
  keepActivities: boolean;
  /** The wheel on the peek strip: down opens the island, up puts it away. */
  wheelGesture: boolean;
  /** Ctrl+K for the gallery and Alt+letter for a section, in the open island. */
  keyJumps: boolean;
  /** The number beside a level's meter (volume, brightness): "60%", or no number at all. */
  levelPercent: boolean;
  /** The time and the day on the resting strip. */
  stripClock: boolean;
  /** The agents count and the plan limits on the resting strip. */
  stripStats: boolean;
  /** The section the island opens on when the strip shows nothing of its own (`<feature>.<id>`); empty: the home view. */
  openOn: string;
  /** A card under the pointer grows a little. */
  hoverEnlarge: boolean;
  /** The cards of the Dashboard: both, the player and the agents, the levels and the next event, or no Dashboard. */
  homeCards: "both" | "now" | "levels" | "off";
  /** "The small island stays": for a moment, while an agent session works or waits, or until the user puts it away. */
  peekStay: "moment" | "agent" | "always";
  absenceInterval: number;
  /** Declared pills next to the main one (at most 4), in the order they were added. */
  activeIntegrations: string[];
  /** The always-on workspace pill: VS Code, Cursor, Codex or Antigravity. */
  mainPill: string;
  /** "primary", "cursor", or `at:<x>,<y>` for one display (logical origin). */
  screen: string;
  /** "Let me drag the island": the strip and the open header can be dragged. */
  islandDrag: boolean;
  /** Where the island was dragged to; null is the top centre of the chosen display. */
  islandSpot: import("../island/place").Spot | null;
  autostart: boolean;
  hooksInstalled: boolean;
  /** Another session's finish or error opens its card for a moment (off: a badge only). */
  peekFinished: boolean;
  /** Claude model used by the chat. */
  model: string;
  /** Show the Claude plan pill (5 h and weekly limits) in the island's header. */
  showPlanInNotch: boolean;
  /** Lintel's status line relay is installed in Claude Code's settings. */
  planRelayInstalled: boolean;
  /** Show the Codex plan pill in the island's header. */
  showCodexPlanInNotch: boolean;
  /** Who the chat talks to (see core/providers.ts); picked in the chat view. */
  chatProvider: ProviderId;
  /** The model picked for each provider other than Anthropic, by provider id. */
  chatModels: Record<string, string>;
  /** Model server addresses once connected; empty means not connected. */
  ollamaUrl: string;
  lmstudioUrl: string;
  customUrl: string;
  /** Global shortcuts the user changed, by action id (see core/shortcuts.ts). */
  shortcuts: Bindings;
  /**
   * A colour of the user's own for a pill's Mascot, by pill ID ("#RRGGBB").
   * Empty means the catalog's colours; read it through core/pill-colors.ts.
   * Same key and values as the Mac's "pillColors".
   */
  pillColors: Record<string, string>;
  /**
   * Interface language: "" follows the system (when Lintel has its language,
   * else English), or one of src/i18n's ten codes ("fr", "pt-BR", "zh-Hans"…).
   */
  language: string;
  /**
   * Each feature's own settings and switch, by feature id. Rust owns both:
   * whatever the page sends back in `saveSettings` is ignored. Read and change
   * them through `FeatureCtx.settings` and `Bridge.featureSetEnabled`.
   */
  features?: Record<string, Record<string, unknown>>;
  enabled?: Record<string, boolean>;
}

export const DEFAULT_SETTINGS: Settings = {
  soundEnabled: true,
  soundVolume: 0.12,
  autoCloseInterval: 15,
  openOnHover: true,
  showMascot: true,
  trayAnimate: true,
  iconShortcuts: true,
  islandOutline: false,
  keepActivities: false,
  wheelGesture: true,
  keyJumps: true,
  levelPercent: true,
  stripClock: false,
  stripStats: false,
  openOn: "",
  hoverEnlarge: false,
  homeCards: "both",
  peekStay: "moment",
  absenceInterval: 180,
  activeIntegrations: [
    "integration_resend", "integration_n8n", "integration_vercel", "integration_github",
  ],
  mainPill: DEFAULT_MAIN_PILL,
  screen: "primary",
  islandDrag: true,
  islandSpot: null,
  autostart: false,
  hooksInstalled: false,
  peekFinished: false,
  model: "claude-opus-5",
  showPlanInNotch: false,
  planRelayInstalled: false,
  showCodexPlanInNotch: false,
  chatProvider: "anthropic",
  chatModels: {},
  ollamaUrl: "",
  lmstudioUrl: "",
  customUrl: "",
  shortcuts: {},
  pillColors: {},
  language: "",
};

type Listener = () => void;

/** Live diffs kept per pill (oldest dropped first) — same cap as macOS. */
export const MAX_DIFFS_PER_PILL = 50;
/** A pill's diffs are forgotten after an hour without a new one, as on macOS. */
export const DIFF_TTL_MS = 3_600_000;

class AppState {
  mode: IslandMode = "hidden";
  view: IslandViewName = "overview";

  tasks: AgentTask[] = [];
  focusId: string | null = null;

  stateOverride: BotStateName | null = null;

  /** Cursor in logical screen pixels, origin top-left (like AppState.mousePosition). */
  mouse = { x: 0, y: 0 };
  /** Cursor relative to the island's top-left corner. */
  mouseInIsland = { x: 0, y: 0 };

  isPinned = false;
  paused = false;

  uploadProgress = 0;
  uploadDuration = 2.4;
  fileDragOver = false;

  promptContext: PromptContext | null = null;
  droppedFile: { name: string; path: string } | null = null;
  noteMessage: string | null = null;
  searchResult: SearchResult | null = null;
  chatHistory: ChatMessage[] = [];
  /** The chat's model picker is open: the island takes its full chat height for the list. */
  pickingModel = false;
  /** The requests waiting for an answer, first come first served (core/approvals.ts). */
  approvals = new ApprovalQueue();
  /** The agent sessions behind the pills (core/sessions.ts). */
  sessions = new Sessions();
  /** The pill that was in front when the card came up; it comes back after. */
  focusBeforeApproval: string | null = null;

  integrations: Record<string, IntegrationInfo> = {};

  /** Claude's 5 h / weekly limits, from the status line (null until the first call). */
  planUsage: PlanUsage | null = null;
  /** Codex's limits, from `codex app-server` (null until it has answered). */
  codexPlanUsage: CodexPlanUsage | null = null;
  /** A plan card is open in place of the overview's left card. */
  showingPlanDetail = false;
  /** Which one: the Codex card rather than Claude's. */
  planDetailIsCodex = false;
  /** Per-pill file diffs, in order of reception. Steps carry their ids. */
  sessionDiffs = new Map<string, FileDiff[]>();
  private sessionDiffTimers = new Map<string, number>();
  /** Never reset, so an id can never point at a newer diff than the one tapped. */
  private nextDiffId = 0;
  /** Ctrl+↑ / Ctrl+↓: the highlighted row of the card's list (AppState.cardSelection). */
  cardSelection: number | null = null;
  /** Rows in the list on screen, 0 when there is none (AppState.cardItemCount). */
  cardItemCount = 0;
  lastActivity = performance.now();

  settings: Settings = { ...DEFAULT_SETTINGS };

  /** Which pills this build offers depends on it (Claude Desktop is Windows only). */
  os: HostOs = HOST_OS;

  /**
   * Whether a catalog pill may be loaded: the island page sets it so the pills
   * of a built-in feature that is off (agents, integrations) are not shown.
   */
  pillAllowed: (id: string) => boolean = () => true;

  private listeners = new Set<Listener>();

  subscribe(fn: Listener): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  /** Marks the UI dirty; the island re-renders on the next frame. */
  notify() {
    for (const fn of this.listeners) fn();
  }

  /** The request on the card: the oldest one waiting. */
  get pendingApproval(): ApprovalInfo | null {
    return this.approvals.head;
  }

  get focusTask(): AgentTask | null {
    return this.tasks.find((t) => t.id === this.focusId) ?? this.tasks[0] ?? null;
  }

  /**
   * The colour the island's mascot wears: its pill's. A card shown beside it (plan usage, a diff,
   * a service's data, the sessions, a question or a permission) changes the card, never this.
   */
  get mascotColor(): string | null {
    return this.focusTask?.color ?? null;
  }

  get effectiveState(): BotStateName {
    return this.stateOverride ?? this.focusTask?.state ?? "idle";
  }

  get otherTasks(): AgentTask[] {
    return this.tasks.filter((t) => t.id !== this.focusId);
  }

  setFocus(id: string) {
    const t = this.tasks.find((x) => x.id === id);
    if (!t) return;
    this.focusId = id;
    this.showingPlanDetail = false;
    t.pillBadge = null;
    this.notify();
  }

  /**
   * A permission request or a question arrives. The first one comes up: its
   * pill comes to the front, and the pill that was there is remembered
   * (HookServer.focusBeforeApproval). One that arrives behind it waits its turn.
   */
  beginApproval(info: ApprovalInfo): "queued" | "known" | "full" {
    const first = this.approvals.size === 0;
    const taken = this.approvals.push(info);
    if (taken !== "queued") return taken;
    this.isPinned = true;
    if (first) {
      if (this.focusBeforeApproval == null) this.focusBeforeApproval = this.focusId;
      this.setFocus(info.pillId);
    } else {
      this.notify();
    }
    return taken;
  }

  /**
   * One request leaves the queue (the one on the card unless another is named):
   * it has its answer, or it is withdrawn. `answered`: the user decided it here.
   * The next one comes up; after the last, the pill you were on comes back,
   * unless you moved to another one meanwhile. Nothing here answers anything.
   */
  endApproval(requestId = this.approvals.head?.requestId, answered = false) {
    const wasHead = requestId === this.approvals.head?.requestId;
    const req = requestId == null ? null : this.approvals.take(requestId, answered);
    if (!req) return;
    // Its session carries on, unless it has another request waiting.
    if (!this.approvals.all().some((a) => a.pillId === req.pillId)) {
      this.updateTask(req.pillId, "working");
      this.setPillBadge(req.pillId, null);
    }
    const next = this.approvals.head;
    if (next) {
      if (wasHead) this.setFocus(next.pillId);
    } else {
      this.isPinned = false;
      const previous = this.focusBeforeApproval;
      this.focusBeforeApproval = null;
      if (previous && this.focusId === req.pillId && this.tasks.some((t) => t.id === previous)) {
        this.focusId = previous;
      }
    }
    this.notify();
  }

  updateTask(id: string, state: BotStateName) {
    const t = this.tasks.find((x) => x.id === id);
    if (!t) return;
    t.state = state;
    this.notify();
  }

  appendStep(id: string, step: string) {
    const t = this.tasks.find((x) => x.id === id);
    if (!t) return;
    const newest = t.stepSeq ?? t.steps.length - 1;
    t.steps.push(step);
    if (t.steps.length > 20) t.steps.shift();
    t.stepIndex = t.steps.length - 1;
    t.stepSeq = newest + 1;
    this.notify();
  }

  setPillBadge(id: string, badge: PillBadge | null) {
    const t = this.tasks.find((x) => x.id === id);
    if (!t) return;
    t.pillBadge = badge;
    this.notify();
  }

  /** Stores a diff for a pill and returns its id (for the ticker step). */
  appendSessionDiff(pillId: string, diff: FileDiff): number {
    const id = this.nextDiffId++;
    const list = this.sessionDiffs.get(pillId) ?? [];
    list.push({ ...diff, id });
    while (list.length > MAX_DIFFS_PER_PILL) list.shift();
    this.sessionDiffs.set(pillId, list);
    // One timer per pill, re-armed on every diff — nothing polls.
    const prev = this.sessionDiffTimers.get(pillId);
    if (prev != null) window.clearTimeout(prev);
    this.sessionDiffTimers.set(
      pillId,
      window.setTimeout(() => this.clearSessionDiffs(pillId), DIFF_TTL_MS),
    );
    return id;
  }

  findDiff(pillId: string, id: number): FileDiff | null {
    return this.sessionDiffs.get(pillId)?.find((d) => d.id === id) ?? null;
  }

  clearSessionDiffs(pillId: string) {
    const timer = this.sessionDiffTimers.get(pillId);
    if (timer != null) window.clearTimeout(timer);
    this.sessionDiffTimers.delete(pillId);
    this.sessionDiffs.delete(pillId);
  }

  /** The always-on workspace pill, once the setting has been checked. */
  get mainPillId(): string {
    return sanitizeDeclared(this.settings, this.os).mainPill;
  }

  /**
   * True for a pill that stays when its session ends: the main pill and the
   * declared ones go back to idle instead of going away.
   */
  isKept(id: string): boolean {
    const d = sanitizeDeclared(this.settings, this.os);
    return id === d.mainPill || d.activeIntegrations.includes(id);
  }

  /**
   * Loads the catalog pills: the main pill always, the declared ones, and none
   * of the others — a pill that is mid-session stays until its session ends.
   * Safe to call any number of times. AppState.loadIntegrationTasks on macOS.
   */
  loadIntegrationTasks() {
    const d = sanitizeDeclared(this.settings, this.os);
    this.settings.mainPill = d.mainPill;
    this.settings.activeIntegrations = d.activeIntegrations;
    for (const def of availablePills(this.os)) {
      const shouldLoad = (def.id === d.mainPill || d.activeIntegrations.includes(def.id)) && this.pillAllowed(def.id);
      const idx = this.tasks.findIndex((t) => t.id === def.id);
      if (shouldLoad && idx < 0) this.tasks.push(taskFor(def, this.settings.pillColors));
      const busy = idx >= 0 && (this.tasks[idx].state !== "idle" || this.tasks[idx].steps.length > 0);
      if (!shouldLoad && idx >= 0 && !busy) this.tasks.splice(idx, 1);
    }
    // A colour picked in Settings reaches the pills that are already up.
    for (const t of this.tasks) {
      const def = pillDefinition(familyOf(t));
      if (def) t.color = pillColor(def.id, def.color, this.settings.pillColors);
    }
    this.tasks = orderPills(this.tasks, d.mainPill);
    if (!this.focusId || !this.tasks.some((t) => t.id === this.focusId)) this.focusId = d.mainPill;
    this.notify();
  }

  /**
   * A session is over. The main and declared pills are put back as they were;
   * any other pill goes away (AppState.removeTask on macOS).
   */
  removeTask(id: string) {
    const idx = this.tasks.findIndex((t) => t.id === id);
    if (idx < 0) return;
    if (this.isKept(id)) {
      const t = this.tasks[idx];
      t.state = "idle";
      t.steps = [];
      t.stepIndex = 0;
      delete t.stepSeq;
      t.pillBadge = null;
      t.finalLine = null;
      t.finalText = null;
      t.sessionId = null;
      t.lastTool = t.model = t.mode = t.effort = null;
      const def = pillDefinition(id);
      if (def) t.name = def.name;
      this.clearSessionDiffs(id);
      this.notify();
      return;
    }
    this.tasks.splice(idx, 1);
    this.clearSessionDiffs(id);
    if (this.focusId === id) this.focusId = this.tasks[0]?.id ?? this.mainPillId;
    this.notify();
  }

  /**
   * Creates the pill of a tagged agent on its first event; no-op if it exists.
   * Inserted right after the main pill so it is in the visible slice(0,4). A
   * catalog agent wears its catalog colour, as on macOS.
   */
  upsertExternalAgent(id: string, name: string, color: string) {
    if (this.tasks.some((t) => t.id === id)) return;
    const def = pillDefinition(id);
    this.insertAfterMain({
      id, name, color: def ? pillColor(def.id, def.color, this.settings.pillColors) : color,
      state: "idle", stepIndex: 0, steps: [],
      source: "agent", isIntegration: false,
    });
  }

  /**
   * The pill a Claude Code session belongs to (VS Code or Cursor). It is made
   * for the session when it is neither the main pill nor declared, as
   * upsertWorkspaceTask does on macOS.
   */
  upsertWorkspacePill(id: string, name: string, cwd: string): AgentTask | null {
    let t = this.tasks.find((x) => x.id === id);
    if (!t) {
      const def = pillDefinition(id);
      if (!def) return null;
      t = taskFor(def, this.settings.pillColors, name);
      this.insertAfterMain(t);
    }
    t.name = name;
    if (cwd) t.sessionCwd = cwd;
    return t;
  }

  /**
   * A pill of its own for one more session of an agent that already has one
   * running: it wears its family's colour, and goes with its session.
   */
  upsertSessionPill(id: string, family: string, name: string, color: string): AgentTask {
    let t = this.tasks.find((x) => x.id === id);
    if (!t) {
      const def = pillDefinition(family);
      t = {
        id, family, name, color: def ? pillColor(def.id, def.color, this.settings.pillColors) : color,
        state: "idle", stepIndex: 0, steps: [], source: def?.source ?? "agent", isIntegration: false,
      };
      this.insertAfterMain(t);
    }
    t.name = name;
    return t;
  }

  private insertAfterMain(t: AgentTask) {
    const at = this.tasks.findIndex((x) => x.id === this.mainPillId) + 1;
    this.tasks.splice(at, 0, t);
    if (!this.focusId) this.focusId = t.id;
    this.notify();
  }

  /** Declares or undeclares a pill (max 4 next to the main one). */
  toggleIntegration(id: string) {
    const next = toggleDeclared(sanitizeDeclared(this.settings, this.os), id, this.os);
    if (!next) return;
    this.settings.activeIntegrations = next;
    if (!next.includes(id) && this.focusId === id) this.focusId = this.mainPillId;
    this.loadIntegrationTasks();
  }

  /**
   * What the island opens on. A card waiting for an answer comes first, so
   * reopening a folded island shows it again (Mac #117, #290).
   */
  defaultView(): IslandViewName {
    if (this.pendingApproval) return this.pendingApproval.questions ? "question" : "approval";
    return this.tasks.length === 0 ? "empty" : "overview";
  }
}

export const State = new AppState();
