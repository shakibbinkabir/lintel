// The agent sessions behind the pills: which pill a session's events go to, and
// what an event means for the turn it belongs to. Behaviour taken from upstream
// pull requests (one pill per session, no false "finished", late events); no
// code of theirs. Pure: no DOM, no timers, the time is handed in.
//
// One pill per session. The first session of an agent keeps that agent's own
// pill (its "family": `integration_claude`, `agent_codex`…); every other
// session running beside it gets a pill of its own until it ends. Twelve
// sessions at most, and one that has said nothing for half an hour is gone.

export const MAX_SESSIONS = 12;
/** A terminal closed without a word: its session is dropped after this much silence. */
export const SILENT_MS = 30 * 60_000;
/** The agent's own turn ids that are remembered, to tell an old turn's event from a new one's. */
const TURN_IDS = 8;

export interface Session {
  /** The agent, as its catalog pill. */
  family: string;
  /** The agent's own id for the session ("" when it sends none). */
  id: string;
  /** The pill its events go to: the family's, or one of its own. */
  pillId: string;
  /** Turns seen: a submitted prompt opens one. */
  turn: number;
  /** The session opens its turns with a prompt (some agents only ever say they stopped). */
  prompted: boolean;
  /** A turn is running. */
  open: boolean;
  /** The end of this turn has been announced: once per turn. */
  notified: boolean;
  /** Background tasks still running when the turn last stopped. */
  background: number;
  /** The agent's own turn ids, oldest first (Codex sends one with every event). */
  turnIds: string[];
  /** When it last spoke, in milliseconds. */
  heard: number;
}

/** What an event says about its turn, read from the hook payload. */
export interface TurnEvent {
  name: string;
  /** The agent's own turn id, when it sends one. */
  turnId?: string;
  /** The event comes from inside a subagent (`agent_id`). */
  subagent?: boolean;
  /** Stop: tasks still running in the background (`background_tasks`). */
  background?: number;
  /** UserPromptSubmit: what was typed. */
  prompt?: string;
}

/**
 * What to do with an event:
 * - `late`: it belongs to a turn that is over. Nothing changes.
 * - `sub`: from a subagent of a running turn: a step to show, never the session's last words.
 * - `open`: the turn starts or goes on.
 * - `background`: the agent stopped talking but its background tasks run on: still working.
 * - `finish`: the turn ended: announce it.
 * - `quiet`: it ended again without a new prompt: show it, announce nothing.
 * - `end`: over with nothing to announce (interrupted, `/clear`).
 * - `error`: the turn failed.
 */
export type Verdict = "late" | "sub" | "open" | "background" | "finish" | "quiet" | "end" | "error";

/**
 * A slash command as the CLI echoes it (`<command-name>/compact</command-name>`,
 * its arguments in `<command-args>`), as typed: `/compact`. Null for anything else.
 * (After upstream pull request 735.)
 */
function slashCommand(prompt: string): string | null {
  const tag = (name: string) => new RegExp(`<${name}>([\\s\\S]*?)</${name}>`).exec(prompt)?.[1]?.trim() ?? "";
  const name = tag("command-name");
  if (!name) return null;
  const args = tag("command-args");
  return (name.startsWith("/") ? name : `/${name}`) + (args ? ` ${args}` : "");
}

/** Typed in the terminal and run by the CLI itself: it clears the conversation, no turn follows. */
export function isClear(prompt: string | undefined): boolean {
  const p = (prompt ?? "").trim();
  return /^\/clear(\s|$)/.test(slashCommand(p) ?? p);
}

/** A prompt the CLI wrote itself to wake the agent (a background task reported): the same turn goes on. */
export function isInjected(prompt: string | undefined): boolean {
  const p = (prompt ?? "").trimStart();
  return p.startsWith("<task-notification") || p.startsWith("<system-reminder");
}

/** A prompt as the session's step line: a slash command as typed, never its raw tags; nothing for one the CLI wrote itself. */
export function promptStep(prompt: string | undefined): string | null {
  if (!prompt || isInjected(prompt)) return null;
  return (slashCommand(prompt) ?? prompt).slice(0, 60) || null;
}

/** The main agent is at work again. Without prompts to count, that is the next turn. */
function reopen(s: Session) {
  if (s.open) return;
  s.open = true;
  if (!s.prompted) s.notified = false;
}

function close(s: Session, notified: boolean) {
  s.open = false;
  s.background = 0;
  s.notified = notified;
}

/**
 * Applies an event to its session's turn and says what it means. The one place
 * that decides "finished or still working" and "is this event late".
 */
export function judge(s: Session, e: TurnEvent): Verdict {
  // An agent that numbers its turns: an event of a turn before the newest one is late.
  if (e.turnId) {
    const at = s.turnIds.indexOf(e.turnId);
    if (at >= 0 && at < s.turnIds.length - 1) return "late";
    if (at < 0) {
      s.turnIds.push(e.turnId);
      if (s.turnIds.length > TURN_IDS) s.turnIds.shift();
    }
  }

  switch (e.name) {
    case "UserPromptSubmit":
      if (isClear(e.prompt)) {
        close(s, true);
        return "end";
      }
      if (isInjected(e.prompt)) {
        s.open = true;
        return "open";
      }
      s.turn += 1;
      s.prompted = true;
      s.open = true;
      s.notified = false;
      s.background = 0;
      return "open";

    case "SessionStart":
      // Started, resumed or cleared: no turn runs until the next prompt or tool.
      close(s, true);
      return "open";

    case "PreToolUse":
      // Inside a subagent: shown while its turn runs, ignored once it is over
      // (a background subagent writes on after the answer was given).
      if (e.subagent) return s.open ? "sub" : "late";
      reopen(s);
      return "open";

    case "PermissionRequest":
      // The agent is held until it is answered: never late, whoever asks.
      if (!e.subagent) reopen(s);
      return "open";

    case "PostToolUse":
    case "PostToolUseFailure":
    case "SubagentStart":
      if (!s.open) return "late";
      return e.subagent ? "sub" : "open";

    case "SubagentStop":
      if (!s.open) return "late";
      if (s.background > 0) s.background -= 1;
      return "sub";

    case "Stop":
      if (e.subagent) return s.open ? "sub" : "late";
      if ((e.background ?? 0) > 0) {
        s.open = true;
        s.background = e.background!;
        return "background";
      }
      if (!s.open) {
        // A second stop with no prompt and no work since the first: the same turn.
        if (s.prompted) return "late";
        s.notified = false;
      }
      if (s.notified) {
        close(s, true);
        return "quiet";
      }
      close(s, true);
      return "finish";

    case "StopFailure":
      close(s, true);
      return "error";

    case "Interrupt":
      close(s, true);
      return "end";

    default:
      return "open";
  }
}

/** "claude-opus-5-5" reads "Opus 5.5"; any other agent's model keeps the name it was given. */
export function modelLabel(id: string): string {
  const m = /^claude-([a-z]+)((?:-\d+)*?)(?:-\d{8})?$/.exec(id.trim());
  if (!m) return id.trim();
  const version = m[2].split("-").filter(Boolean).join(".");
  return `${m[1][0].toUpperCase()}${m[1].slice(1)}${version ? ` ${version}` : ""}`;
}

/** An app the relay started leaves again once the agents have been quiet this long. */
export const QUIT_AFTER_MS = 10 * 60_000;

/**
 * Whether an app the relay started (the opt-in of Settings › Agents) may leave:
 * never one the user started, never while an agent works or waits for an
 * answer, never while the island is in use.
 */
export function mayQuit(o: {
  autoLaunched: boolean; now: number; lastHeard: number | null; lastUsed: number;
  busy: boolean; waiting: number; islandShown: boolean;
}): boolean {
  if (!o.autoLaunched || o.busy || o.waiting > 0 || o.islandShown) return false;
  return o.now - Math.max(o.lastHeard ?? 0, o.lastUsed) >= QUIT_AFTER_MS;
}

export class Sessions {
  private rows: Session[] = [];
  private seq = 0;

  list(family?: string): Session[] {
    return family == null ? [...this.rows] : this.rows.filter((s) => s.family === family);
  }

  find(family: string, id: string): Session | null {
    return this.rows.find((s) => s.family === family && s.id === id) ?? null;
  }

  /** The session with this id, whichever agent it is (the status line names no agent). */
  byId(id: string): Session | null {
    return (id && this.rows.find((s) => s.id === id)) || null;
  }

  ofPill(pillId: string): Session | null {
    return this.rows.find((s) => s.pillId === pillId) ?? null;
  }

  /**
   * The session an event belongs to, made on its first event. `dropped` are the
   * sessions that had to go for it: silent ones, and the one heard longest ago
   * when twelve are already there.
   */
  touch(family: string, id: string, now: number): { session: Session; dropped: Session[] } {
    const known = this.find(family, id);
    if (known) {
      known.heard = now;
      return { session: known, dropped: [] };
    }
    const dropped = this.prune(now);
    if (this.rows.length >= MAX_SESSIONS) {
      const oldest = this.rows.reduce((a, b) => (b.heard < a.heard ? b : a));
      this.rows.splice(this.rows.indexOf(oldest), 1);
      dropped.push(oldest);
    }
    // The agent's own pill while no other session of it holds it.
    const taken = this.rows.some((s) => s.pillId === family);
    const session: Session = {
      family, id, pillId: taken ? `session_${++this.seq}` : family,
      // Heard for the first time, it is taken to be at work: only a turn seen to end is over.
      turn: 0, prompted: false, open: true, notified: false, background: 0, turnIds: [], heard: now,
    };
    this.rows.push(session);
    return { session, dropped };
  }

  /** The session is over. */
  end(family: string, id: string): Session | null {
    const at = this.rows.findIndex((s) => s.family === family && s.id === id);
    return at < 0 ? null : this.rows.splice(at, 1)[0];
  }

  /** Takes out the sessions that have said nothing for half an hour, and returns them. */
  prune(now: number): Session[] {
    const gone = this.rows.filter((s) => now - s.heard >= SILENT_MS);
    if (gone.length > 0) this.rows = this.rows.filter((s) => !gone.includes(s));
    return gone;
  }

  /** When the next silent session is due to go, or null with no session. */
  nextPrune(): number | null {
    return this.rows.length === 0 ? null : Math.min(...this.rows.map((s) => s.heard)) + SILENT_MS;
  }

  /** When any session last spoke, or null with no session. */
  lastHeard(): number | null {
    return this.rows.length === 0 ? null : Math.max(...this.rows.map((s) => s.heard));
  }
}
