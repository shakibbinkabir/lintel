// Agents other than Claude Code, as the island shows them. Ids, names and colours
// are PillCatalog.swift's (pill `agent_<id>`), so a pill looks the same on every
// platform.

import { Bridge, type Decision } from "../core/bridge";
import { sessionOwner } from "../core/pills";
import type { Answers } from "../core/question";
import { State, familyOf, type AgentTask } from "../core/state";

export interface KnownAgent {
  name: string;
  color: string;
}

export const KNOWN_AGENTS: Record<string, KnownAgent> = {
  cursor: { name: "Cursor", color: "#C0C4CC" },
  antigravity: { name: "Antigravity", color: "#E879F9" },
  codex: { name: "Codex", color: "#2DD4BF" },
  gemini: { name: "Gemini CLI", color: "#8AB4F8" },
  copilot: { name: "Copilot CLI", color: "#818CF8" },
  muse: { name: "Muse Code", color: "#38BDF8" },
  opencode: { name: "OpenCode", color: "#4ADE80" },
  amp: { name: "Amp", color: "#F59E0B" },
  hermes: { name: "Hermes", color: "#C084FC" },
  "claude-desktop": { name: "Claude Desktop", color: "#D97757" },
  kiro: { name: "Kiro", color: "#9046FF" },
};

/**
 * Agents whose permission requests get an Allow / Deny card, as on the Mac.
 * Must match `takes_decisions` in the relay (hook/src/reply.rs): any other
 * agent's request is handed straight back to its terminal. A session of the
 * Claude desktop app is Claude Code itself: the relay runs for it without
 * `--agent` and only tags the event, so the relay answers it as Claude Code's
 * (upstream pull requests 341 and 441).
 */
export const APPROVAL_AGENTS = new Set(["codex", "copilot", "muse", "claude-desktop", "cursor", "kiro", "antigravity", "opencode"]);

/** Tagged agents that ask questions the way Claude Code does (AskUserQuestion). */
export const QUESTION_AGENTS = new Set(["claude-desktop", "opencode"]);

/**
 * Claude Code under another name: `lintel-hook --agent claude-<something>` gives a
 * second Claude profile a pill of its own that still behaves as Claude Code's (cards
 * and questions from the island). Same rule as `is_claude_code` in the relay;
 * `claude-desktop` stays what it is, the tag of the Claude desktop app.
 */
export function isClaudeAlias(agent: string | null): boolean {
  return agent !== null && agent.startsWith("claude-") && agent !== "claude-desktop";
}

/** Same rule as HookServer.validateAgent on macOS. "claude" is reserved. */
export function validateAgent(raw: string | undefined): string | null {
  if (!raw || raw.length > 24 || raw === "claude") return null;
  if (!/^[a-z0-9-]+$/.test(raw)) return null;
  return raw;
}

const FALLBACK_COLORS = ["#22C55E", "#EAB308", "#60A5FA", "#E879F9"];

export function agentColor(id: string): string {
  const known = KNOWN_AGENTS[id];
  if (known) return known.color;
  let h = 0;
  for (let i = 0; i < id.length; i++) {
    h = (Math.imul(31, h) + id.charCodeAt(i)) | 0;
  }
  return FALLBACK_COLORS[Math.abs(h) % FALLBACK_COLORS.length];
}

/** The pill label: the agent's own name when Lintel knows it, else its id. */
export function agentName(id: string): string {
  if (KNOWN_AGENTS[id]) return KNOWN_AGENTS[id].name;
  // "claude-perso" → "Claude perso"
  return isClaudeAlias(id) ? `Claude ${id.slice("claude-".length).replace(/-+/g, " ").trim()}`.trim() : id;
}

/**
 * Brings a session's home forward: the app it lives in (Codex, Claude, Cursor,
 * Antigravity) or its terminal. Rust goes by the window it found for the
 * session and by its own list of programs: nothing an agent wrote is ever
 * started. False when there is no session to go to.
 */
export function jumpToSession(task: AgentTask | null): boolean {
  // The pill is gone (the Claude app's, or one that rested): the session that finished last stands in.
  const at = task && State.tasks.some((x) => x.id === task.id) ? { sessionId: task.sessionId, sessionCwd: task.sessionCwd, family: familyOf(task) } : lastFinished;
  if (!at) return false;
  void Bridge.openSession(at.sessionId ?? null, at.sessionCwd ?? null, sessionOwner(at.family)?.id ?? null);
  return true;
}

/** The session whose turn ended last, kept past its pill. */
let lastFinished: { sessionId?: string | null; sessionCwd?: string | null; family: string } | null = null;

export function noteFinished(task: AgentTask | undefined) {
  if (task) lastFinished = { sessionId: task.sessionId, sessionCwd: task.sessionCwd, family: familyOf(task) };
}

// ── Answering a card ──────────────────────────────────────────────────────────
//
// The only three places a waiting request is answered from the island. Each is
// called from a click on the card (a question also from its own keys, never a
// permission), and each names the request the card was drawn for: when that
// request is no longer the one on the card, nothing is sent and nothing leaves
// the queue. No timer and no step of the queue ever comes through here.

/** Allow or Deny, for the permission request on the card. True when it was taken. */
export function decideRequest(decision: Decision, requestId: string): boolean {
  const req = State.pendingApproval;
  if (!req || req.requestId !== requestId || req.questions) return false;
  void Bridge.approvalDecision(requestId, decision);
  State.endApproval(requestId, true);
  return true;
}

/** The answers to the question on the card: question text → label, or the words typed for those in `other`. */
export function answerRequest(answers: Answers, other: string[], requestId: string): boolean {
  const req = State.pendingApproval;
  if (!req || req.requestId !== requestId || !req.questions) return false;
  void Bridge.approvalAnswer(requestId, answers, other);
  State.endApproval(requestId, true);
  return true;
}

/** "Answer in terminal": the request on the card goes back to its agent undecided. */
export function returnRequest(requestId: string): boolean {
  const req = State.pendingApproval;
  if (!req || req.requestId !== requestId) return false;
  void Bridge.approvalDecline(requestId);
  State.endApproval(requestId);
  return true;
}
