// The island: DOM shell, sizing animation, mascot placement, pointer and keys,
// and the API features use (docs/ARCHITECTURE.md §4, §5).

import { Tracked, easeOut, scaleFrom } from "../core/anim";
import { Bridge, IS_TAURI, onDragDrop, onEvent } from "../core/bridge";
import type { IslandApi, IslandShape, ShapeAsk, VoiceFace } from "../core/feature";
import {
  COMPACT_W, EXPANDED_CORNER, EXPANDED_W, NOTCH_H, NOTCH_W, PANEL_H, PANEL_W,
  ROUNDED_CORNER, botPosition, chatPromptHeight, islandSize, pixelWidth,
  QUESTION_PICKER_H,
  type BotEmoteName, type BotStateName, type IslandMode, type IslandViewName,
} from "../core/layout";
import { featureOn, onFeaturesChanged } from "../core/registry";
import { Sound } from "../core/sound";
import { State, familyOf, type AgentTask } from "../core/state";
import { pillDefinition, sessionOwner } from "../core/pills";
import { onLanguageChange, tl } from "../i18n/i18n";
import { BotEngine, hexToRGB, setDanceBeat, wipe } from "../mascot/engine";
import { Greeting } from "../mascot/greeting";
import { Mood } from "../mascot/mood";
import { pruneMiniBots, syncMiniBotStates, tickMiniBots } from "../mascot/minibots";
import { dotMark, iconBtn, setDotMark, tab } from "../ui/components";
import { hideTip } from "../ui/tooltip";
import { UploadCanvas } from "../upload/canvas";
import { USC, UploadSeq } from "../upload/sequence";
import { closePlanCard } from "../views/usage";
import { buildHeader, buildViews, finishedIsTall, hasChips, type ViewActions, type ViewHost } from "../views/views";
import { h } from "../views/dom";
import { ACTIVITY_REVEAL_MS, Arbiter, ORDER, pickActivities } from "./arbiter";
import { IslandStateMachine, agentBusy } from "./fsm";
import { refreshHookPills } from "./integrations";
import { answerRequest, decideRequest, jumpToSession, returnRequest } from "./agents";
import { comboCodes, comboEvent } from "./keys";
import { FLOAT_TOP, PILL_H, PILL_W, dragStarted, resolve, settle, type Display } from "./place";
import { buildHome } from "./home";
import { HOME_KEY, Sections, activities, galleryHeight, headerKeys, sections, setBuilder, surfaceKey, type Activity } from "./sections";
import type { ViewCommand } from "./shortcuts";
import { Strip, type StripContent } from "./strip";
import { Travel } from "./travel";
import { TrayMascot } from "./tray-mascot";
import { EAR_HEARD_MS, VOICE, earLook, faceEmote, mascotFace, sameShape, stripWidth, voiceMay, type EarLook } from "./voice";
import { buildUtilities, runTool } from "./utilities";

/** The finished card with an answer that scrolls. */
const FINISHED_TALL_H = 300;
/** What the row of session chips adds to the overview. */
const CHIPS_H = 22;
/** Same margin as the Rust hit test (src-tauri/src/island.rs). */
const HIT_MARGIN = 14;
/** The mascot's place on the peek strip, and in the greeting that brings it there. */
const STRIP_BOT = botPosition("compact", "overview", NOTCH_H);
/** The mascot's canvas, in logical pixels: twice the largest mascot, room for a hop, a shake, a heart. Same number in style.css. */
const BOT_BOX = 128;
/** How long after the island opens its body is fully in: --d-settle + --d-stagger + --d-base (tokens.css), and a frame. */
const BODY_IN_MS = 200 + 40 + 180 + 20;
/** How long after the island folds its strip is fully back: --d-settle + --d-base, and a frame. */
const STRIP_IN_MS = 200 + 180 + 20;
/** A pointer on the wake strip brings the island out only if it is still there after this: one that crosses the top edge brings nothing. */
const WAKE_DELAY_MS = 50;
/** Between two frames while the strip rests (§4): the mascot still breathes, about fourteen times a second instead of sixty. */
const REST_FRAME_MS = 66;

/** How much a card grows under the pointer (Settings › Island "Enlarge cards on hover"), and in how long: `scale(1.02)` and --d-fast in style.css. */
const HOVER_GROW = 1.02;
const HOVER_GROW_MS = 120;
/** What grows there, as in style.css. */
const GROWS = ".card, .isl-pill, .tile";
const REDUCED = window.matchMedia?.("(prefers-reduced-motion: reduce)");

/** The three views the drop sequence owns; leaving them stops the engine. */
const UPLOAD_VIEWS: ReadonlySet<IslandViewName> = new Set(["upload", "uploading", "choose"]);

/** Seconds between the drop and the moment the progress bar starts filling. */
const PRE_PROGRESS = USC.T_PROG_START - USC.T_DROP;

/** Where a click means "I want to type here". */
const TEXT_ENTRY =
  "textarea, select, [contenteditable], input:not([type=range], [type=checkbox], [type=radio], [type=button], [type=color], [type=file])";

/**
 * Text a card may have had to cut short: a click on it, when it is cut, shows
 * all of it (the reader). A feature marks its own with `data-expand`.
 */
const READS = "[data-expand], .focus-line, .title, .detail";
/** A click here is that control's own, never a request to read. */
const CONTROLS = "button, a, input, textarea, select, [role=slider], .tab, .tile, .list-item, .link";

/** Where the section last opened is kept between runs (a convenience of this page, nothing more). */
const LAST_TOOL = "lintel.lastTool";

/** The states the mascot dances in while music plays; the rest (an alert, an error, sleep) win. */
const DANCE_STATES: ReadonlySet<BotStateName> = new Set(["idle", "working", "thinking", "searching", "finished"]);

/** A feature's `isActive` must never take the island down. */
const isActive = (a: Activity): boolean => {
  try {
    return a.def.isActive();
  } catch (err) {
    console.error(`[activity ${a.key}] isActive failed`, err);
    return false;
  }
};

export class Island {
  readonly fsm = new IslandStateMachine();
  readonly arbiter: Arbiter;

  private root: HTMLElement;
  private islandEl!: HTMLElement;
  private clipEl!: HTMLElement;
  private contentEl!: HTMLElement;
  private viewsEl!: HTMLElement;
  private botCanvas!: HTMLCanvasElement;
  /** The mascot when it is hidden (Settings › Mascot): a dot in its place. */
  private botDot = dotMark("", "idle");
  private greetingCanvas!: HTMLCanvasElement;
  private countdown!: HTMLElement;
  private cdDue: number | null = null;
  private wakeStrip!: HTMLElement;
  /** All of a text its card had to cut short, over the view; a click or Esc puts it away. */
  private reader = h("div", { id: "reader" });
  /**
   * The microphone indicator (docs/VOICE.md §7.5, `IslandApi.ear`): 28 × 3 at the top centre, over
   * whatever the island shows. Never a target of the pointer, and no part of the shape Rust tests
   * clicks against: the wake strip under it and the click-through are what they were.
   * debt: on the pill of a floating island it sits at the top edge, not in the middle (never looked at).
   */
  private earEl = h("div", {
    id: "ear",
    style:
      "position:absolute;top:0;left:50%;z-index:3;width:28px;height:3px;margin-left:-14px;border-radius:var(--r-pill);" +
      "background:var(--accent);opacity:0;pointer-events:none;" +
      "transition:opacity var(--d-fast) var(--ease-out),background-color var(--d-fast) var(--ease-out)",
  });
  private earTimer: number | undefined;
  /** What the voice row asked of the island's shape, and the box it was last told. */
  private shapeAsk: ShapeAsk | null = null;
  private shapeSaid: IslandShape | null = null;
  /** The face voice asked for with its level, and what the mascot wears for it (its own while a card waits). */
  private voiceFace: { face: VoiceFace; level: number } | null = null;
  private worn = mascotFace(null, 0, false);

  private header!: ViewHost;
  /** Folds the open island; not there while a card waits. */
  private foldBtn!: HTMLElement;
  /** Puts away a strip that stays (Settings › Island): at its right end, while the pointer is on the strip. */
  private foldMark!: HTMLElement;
  private foldShown = false;
  private views!: Map<IslandViewName, ViewHost>;
  private uploadCanvas!: UploadCanvas;
  private strip = new Strip();
  private sections!: Sections;

  private width = new Tracked(NOTCH_W);
  private height = new Tracked(0);
  private radius = new Tracked(ROUNDED_CORNER);
  private target = { w: NOTCH_W, h: 0 };
  // The mascot, from the island's centre line and top edge: the strip and the
  // open island are both centred, so it keeps its place whatever the width does.
  private botXc = new Tracked(STRIP_BOT.cx - COMPACT_W / 2);
  private botCy = new Tracked(STRIP_BOT.cy);
  private botSize = new Tracked(STRIP_BOT.diameter);
  private botTarget = { x: STRIP_BOT.cx - COMPACT_W / 2, y: STRIP_BOT.cy, d: STRIP_BOT.diameter };
  /** The mascot was on screen at the last frame: it travels to its next place instead of appearing there. */
  private botSeen = false;
  /** The card the mascot sits in while it is grown under the pointer, and its centre: the mascot is grown with it. */
  private grown: { card: Element; x: number; y: number } | null = null;
  /** When the mascot last perked up for a card that grew, on the frame clock. */
  private perkedAt = -1e9;
  /** What the strip and the open island both show, on its way between them (§4). */
  private travel = new Travel();
  /** The view the open island was last on: where a fold's journeys start. */
  private openView: HTMLElement | null = null;
  /** When the island last opened, on the frame clock. */
  private openedAt = -1e9;

  private engine = new BotEngine();
  /** Sleep after a quiet spell, and the PC's mood (src/mascot/mood.ts). */
  private mood = new Mood(() => this.invalidate());
  private greeting = new Greeting();
  /** The mascot in the notification area, on the windows' taskbar buttons and on the shortcuts. */
  private tray = new TrayMascot(() => this.arbiter.notice != null);

  private running = false;
  private lastFrame = 0;
  private dirty = true;
  /**
   * The strip rests: the setting keeps it out, the pointer is away and nothing
   * moves. Frames come slowly (REST_FRAME_MS, or not at all when no mascot is
   * drawn) and Rust's cursor poll only looks for the pointer coming back (§4).
   */
  private rest = false;
  private restTimer: number | null = null;

  // Rust starts the window at full size so the launch greeting has room.
  private collapsed = false;
  private collapseTimer: number | null = null;
  private wasInIsland = false;
  /**
   * The island was put away under a pointer that rests on the wake strip (Esc, the wheel, the shortcut, a jump to
   * a terminal): the strip brings nothing out until that pointer has left it, or the island would come straight back.
   */
  private wakeSpent = false;
  /** Dragged off the top edge (§4): all four corners round, and FLOAT_TOP lower in its window, for the shadow. */
  private floating = false;
  /** A press on the strip or on the empty part of the header that is not yet a click or a drag: where, and what the click does. */
  private press: { x: number; y: number; click: () => void } | null = null;
  /** Ends the hold of a drag in progress. */
  private dragHold: (() => void) | null = null;
  /** The spot the window was last placed for, as text. */
  private placed = "null";
  /** The cursor poll has said where the pointer is since the window last came back from the wake strip. */
  private cursorKnown = false;
  /** Last shape handed to Rust for the click-through test. */
  private pushedRect = { x: -1, y: -1, w: -1, h: -1 };

  // Resting on the mascot → love; three clicks → dizzy.
  private botHovering = false;
  private botHoverTimer: number | null = null;
  private lastLoveTime = 0;
  private botHoverStart = { x: 0, y: 0 };
  private slaps: number[] = [];

  private confusedRecovery: number | null = null;
  private prevViewBeforeConfused: IslandViewName = "overview";
  /** What had the text cursor at the last sync: a view, or a section's key. */
  private lastTyping: string | null = null;
  /** The island holds the keyboard: it is the window in front (§4, "Keyboard"). */
  private keyboard = false;
  /** The keyboard was given by a click inside the island (not by a shortcut): what the question card's keys ask. */
  private keyboardByClick = false;
  /**
   * The pointer has been on the island since it took the keyboard: when it
   * leaves, the keyboard goes back (unless text is being typed). An island
   * opened by a shortcut with the pointer elsewhere keeps it until it folds.
   */
  private pointerSeen = false;
  /** Takes and hand-backs reach Rust in the order they were asked for. */
  private keyboardQueue: Promise<unknown> = Promise.resolve();
  private leaveTimer: number | null = null;
  /** The left button is held, as the cursor poll last said: a drag may be going on. */
  private buttonDown = false;
  /** The keys last given to Rust to read under the pointer. */
  private pushedKeys = "";
  /** Rust was last told to report a press outside the island. */
  private pushedWatch = false;
  private wakeTimer: number | undefined;
  private clockTimer: number | null = null;
  /** The section last opened, other than the Dashboard: its row offers it again. */
  private lastTool: string | null = null;
  /** The view an open asked for by code (an alert, a tab, a shortcut) is for; null when the pointer opened the strip. */
  private forced: IslandViewName | null = null;
  private lastWheel = 0;

  /** The launch greeting ended, or the island came out of hidden — two of the
   *  moments the Monday recap may open (see src/recap/recap.ts). */
  onGreetingDone: (() => void) | null = null;
  onWake: (() => void) | null = null;

  /** Drop sequence bookkeeping: last tick played, and whether the ✓ has fired. */
  private uploadTens = 0;
  private uploadDone = false;

  // Live activities (§5.2).
  private activityRev = 0;
  private tickTimer: number | null = null;
  private activityHold: (() => void) | null = null;
  private songPlays = false;
  private modeWatchers = new Set<() => void>();
  /** What each feature holds the island with, so a feature that stops lets go. */
  private holds = new Map<string, Set<() => void>>();
  /** The agents at work are a live activity like any other: the pills strip, at its place in the order. */
  private agents: Activity = {
    key: "agents",
    feature: "agents",
    def: {
      id: "agents",
      order: ORDER.agents,
      isActive: () => State.tasks.some((t) => t.state !== "idle" && t.state !== "sleeping"),
      render: () => h("div"),
      mark: () => dotMark(State.focusTask?.color ?? "", State.effectiveState),
    },
  };

  constructor(root: HTMLElement) {
    this.root = root;
    this.arbiter = new Arbiter({
      blocked: () => State.pendingApproval != null,
      open: () => State.mode === "expanded",
      hovered: () => this.wasInIsland && State.mode === "compact",
      reveal: (ms, exact) => this.reveal(ms, exact),
      changed: () => this.invalidate(),
    });
    this.build();
    this.wireFsm();
    this.wireInput();
    this.greeting.onComplete = () => {
      this.fsm.greetComplete();
      this.onGreetingDone?.();
    };
    State.subscribe(() => this.invalidate());
    onFeaturesChanged(() => this.featuresChanged());
    // The base views relabel themselves; a feature's section is built again in the new language.
    onLanguageChange(() => {
      this.sections.rebuild();
      this.invalidate();
    });
  }

  private invalidate() {
    this.dirty = true;
    this.ensureRunning();
  }

  // ── What features get (FeatureCtx.island) ───────────────────────────────────

  /** The island as one feature sees it: its own sections and activities, by their ids. */
  api(feature: string): IslandApi {
    return {
      notice: (notice) => {
        if (!State.paused) this.arbiter.post(notice, feature);
      },
      reveal: (ms) => {
        if (!State.pendingApproval) this.reveal(ms);
      },
      // A card that waits is never covered: a feature cannot take the island off it.
      // Nor off the Drop tab, when it comes for what is being dragged: the user
      // put the island there to drop a file on it (§5).
      open: (section, opts) => {
        if (State.pendingApproval) return;
        if (opts?.dragged && State.mode === "expanded" && UPLOAD_VIEWS.has(State.view)) return;
        this.openSection(surfaceKey(feature, section));
      },
      activityChanged: (id) => {
        this.activityRev++;
        const go = () => {
          // Only the activity the strip shows brings it out: music pausing behind
          // a timer or the agents at work changes nothing anyone would see.
          const owner = this.pickStrip().primary?.a;
          this.arbiter.activityChanged(owner?.key === surfaceKey(feature, id) ? owner.def : undefined);
          this.syncActivityHold();
        };
        // About to come out of hiding (playback starting): the full-screen rule
        // is asked again first. A video that went full screen after it came to
        // the front may not have said so (§4).
        if (State.mode !== "hidden" || !featureOn("huds-power")) return go();
        void Bridge.featureCall<boolean>("huds-power", "fullscreen")
          .then((on) => {
            if (typeof on === "boolean" && on !== this.arbiter.quiet && !Sound.hushed) this.setFullscreen(on);
          })
          .catch(() => {})
          .then(go);
      },
      redraw: () => {
        this.activityRev++;
        this.invalidate();
      },
      fullscreen: (on, call) => this.setFullscreen(on, call),
      hold: () => {
        const mine = this.holds.get(feature) ?? new Set();
        this.holds.set(feature, mine);
        const release = this.fsm.hold();
        mine.add(release);
        return () => {
          mine.delete(release);
          release();
        };
      },
      inHeader: (section) => headerKeys().includes(surfaceKey(feature, section)),
      mode: () => (this.islandEl.dataset.mode ?? "hidden") as "hidden" | "strip" | "open",
      onMode: (fn) => {
        this.modeWatchers.add(fn);
        return () => void this.modeWatchers.delete(fn);
      },
      // A card that waits is never folded by a feature.
      close: () => {
        if (State.mode === "expanded" && !State.pendingApproval && !State.isPinned) this.collapse();
      },
      // The voice assistant's three (island/voice.ts): nothing for any other feature, nothing while voice is off.
      ear: (on, heard) => {
        if (voiceMay(feature, featureOn)) this.setEar(earLook(on, heard));
      },
      shape: (ask) => {
        if (ask !== undefined && voiceMay(feature, featureOn)) {
          this.shapeAsk = ask;
          this.shapeSaid = this.shapeNow();
          this.invalidate();
        }
        return this.shapeNow();
      },
      face: (face, level = 0) => {
        if (!voiceMay(feature, featureOn)) return;
        const emote = faceEmote(face);
        this.voiceFace = face && !emote ? { face, level } : null;
        this.wear();
        // The face of a card that waits is not winked over.
        if (emote && !State.pendingApproval) this.engine.triggerEmote(emote);
        // Hidden, nobody sees the mascot: no frame is asked for.
        if (State.mode !== "hidden") this.ensureRunning();
      },
    };
  }

  // ── The voice assistant's hooks (the rules: island/voice.ts) ─────────────────

  /** The ear bar, by events alone: two styles, and one timer for the 0.3 s it is bright. */
  private setEar(look: EarLook) {
    window.clearTimeout(this.earTimer);
    this.earTimer = look === "heard" ? window.setTimeout(() => this.setEar("on"), EAR_HEARD_MS) : undefined;
    this.earEl.style.opacity = look === "off" ? "0" : look === "heard" ? "1" : "0.7";
    this.earEl.style.background = look === "heard" ? "var(--ink)" : "var(--accent)";
  }

  /** The island's box: the size it is heading for, and how far under its window's top edge. */
  private shapeNow(): IslandShape {
    const mode = State.mode === "hidden" ? "hidden" : State.mode === "expanded" ? "open" : "strip";
    return { mode, width: this.target.w, height: this.target.h, top: this.top };
  }

  /** The box changed: the voice row hears of it, if it asked to. */
  private tellShape() {
    const changed = this.shapeAsk?.changed;
    if (!changed) return;
    const now = this.shapeNow();
    if (this.shapeSaid && sameShape(now, this.shapeSaid)) return;
    this.shapeSaid = now;
    try {
      changed(now);
    } catch (err) {
      console.error("[island] shape watcher failed", err);
    }
  }

  /**
   * Hands the mascot the face voice asked for; again on every sync while one is asked for, so it
   * steps back for a card that comes and returns when the card is answered. The two new faces go to
   * `BotEngine.voice`, which the engine reads once it draws them (the package voice-mascot): until
   * then it is a property nothing reads, and the mascot keeps its own face.
   */
  private wear() {
    this.worn = mascotFace(this.voiceFace?.face ?? null, this.voiceFace?.level ?? 0, State.pendingApproval != null);
    this.engine.face = this.worn.held;
    Object.assign(this.engine, { voice: this.worn.voice });
    // The dot that stands in for a hidden mascot reads the same from its `data-state` and `--level`.
    if (this.worn.voice) this.botDot.style.setProperty("--level", this.worn.voice.level.toFixed(3));
    else this.botDot.style.removeProperty("--level");
  }

  /** Voice was switched off: its bar goes out, what it asked is forgotten, the mascot is its own again. */
  private voiceGone() {
    this.setEar("off");
    this.shapeAsk = null;
    this.voiceFace = null;
    this.wear();
  }

  /** A full-screen app came to the front or left (§4); a feature says so with `cx.island.fullscreen`. */
  setFullscreen(on: boolean, call = false) {
    this.arbiter.quiet = on;
    Sound.hushed = call;
    if (on && State.mode !== "expanded" && !this.fsm.held) this.fsm.forceHidden();
    else this.arbiter.flush();
  }

  /** The computer was locked or slept (the cursor poll tells, `away`): the strip is put away like under a full-screen app. */
  private away() {
    this.tray.away();
    if (State.mode === "compact" && !this.fsm.held) this.fsm.forceHidden();
  }

  /** Puts away a strip that stays (the island's shortcut, its fold mark). False when there was none to put away. */
  dismiss(): boolean {
    return this.fsm.dismiss();
  }

  /** An agent session works or waits for the user: what "While an agent is working" keeps the strip for. A service's pill is not an agent. */
  private agentsBusy(): boolean {
    return agentBusy(State.tasks.filter((t) => pillDefinition(familyOf(t))?.category !== "service"));
  }

  /** The fold mark shows on a strip that stays while the pointer is on it, unless the row has a button of its own there. */
  private syncFold() {
    const on = this.fsm.kept && this.wasInIsland && !this.strip.el.querySelector(".strip-in:not(.fx-out) button");
    if (on === this.foldShown) return;
    this.foldShown = on;
    this.foldMark.style.opacity = on ? "1" : "0";
    this.foldMark.style.pointerEvents = on ? "auto" : "none";
  }

  /** A feature was switched on or off: what it showed goes, what it held is let go. */
  private featuresChanged() {
    for (const [feature, mine] of this.holds) {
      if (featureOn(feature)) continue;
      for (const release of mine) release();
      this.holds.delete(feature);
    }
    if (this.arbiter.notice && !featureOn(this.arbiter.notice.feature)) this.arbiter.dismiss();
    if (!featureOn(VOICE)) this.voiceGone();
    this.sections.prune();
    this.syncActivityHold();
    const gone =
      (State.view === "section" && !this.sections.active) ||
      (State.view === "prompt" && !featureOn("chat")) ||
      (UPLOAD_VIEWS.has(State.view) && !featureOn("drop")) ||
      (State.view === "recap" && !featureOn("recap"));
    if (gone && State.mode === "expanded") this.setView(State.defaultView());
    else if (gone) State.view = State.defaultView();
    this.invalidate();
  }

  /** An activity that holds the strip (its own `hold`, or the setting) keeps the island from hiding. */
  private syncActivityHold() {
    const want = activities().some((a) => isActive(a) && (a.def.hold || State.settings.keepActivities));
    if (want && !this.activityHold) {
      this.activityHold = this.fsm.hold();
      this.reveal();
    } else if (!want && this.activityHold) {
      this.activityHold();
      this.activityHold = null;
    }
  }

  /**
   * A request has left the card (answered here, or handed back to its
   * terminal: island/agents.ts). The next one of the queue comes up in its
   * place, the island still pinned on it; after the last, the session carries on.
   */
  private afterAnswer() {
    if (State.pendingApproval) {
      this.setView(State.defaultView());
      return;
    }
    this.fsm.unpin();
    this.setView(State.defaultView());
    this.arbiter.flush();
    // Answered: the user goes on typing where they were, the island still open.
    if (!this.typingInField()) this.releaseKeyboard();
  }

  /**
   * Brings a session's home forward (its app or its terminal, island/agents.ts).
   * The island folds once it has jumped.
   */
  openSession(task: AgentTask | null) {
    if (!jumpToSession(task)) return;
    // A waiting card is only ever folded. Otherwise the island leaves with the user.
    if (State.pendingApproval) {
      this.collapse();
      return;
    }
    State.isPinned = false;
    this.fsm.pinned = false;
    this.fsm.leave();
  }

  /**
   * Folds a card that is waiting for an answer down to the compact island,
   * without answering it. Nothing is decided: the request keeps waiting, the
   * island stays on screen, and opening it shows the card again.
   */
  foldApproval() {
    if (!State.pendingApproval || State.mode !== "expanded") return;
    State.isPinned = true;
    this.fsm.pinned = true;
    this.fsm.forcePeek();
  }

  // ── DOM ─────────────────────────────────────────────────────────────────────

  private build() {
    const actions: ViewActions = {
      setView: (v) => this.setView(v),
      cancelDrop: () => this.discardDrop(),
      collapse: () => this.collapse(),
      foldApproval: () => this.foldApproval(),
      setFocus: (id) => {
        State.setFocus(id);
        Sound.play("blip");
        // A pill with a waiting request opens on its card: going back to it
        // after looking at another pill brings the card up again.
        const req = State.pendingApproval;
        if (req?.pillId === id) this.setView(req.questions ? "question" : "approval");
      },
      openTerminal: () => this.openSession(State.focusTask),
      openSessionOf: (id) => {
        State.setFocus(id);
        Sound.play("blip");
        this.openSession(State.tasks.find((x) => x.id === id) ?? null);
      },
      // The ↗ button: the pill's app, site or terminal.
      openTarget: () => {
        const task = State.focusTask;
        if (!task) return;
        const urls: Record<string, string> = {
          integration_resend: "https://resend.com/emails",
          integration_vercel: "https://vercel.com/dashboard",
          integration_github: "https://github.com/pulls",
          integration_stripe: "https://dashboard.stripe.com/payments",
          integration_notion: "https://notion.so",
          integration_calcom: "https://app.cal.com/bookings",
          integration_linear: "https://linear.app/inbox",
        };
        if (sessionOwner(familyOf(task)) || familyOf(task) === "integration_claude" || task.sessionId) {
          this.openSession(task);
        } else if (task.id === "integration_n8n") void Bridge.openN8n();
        else if (urls[task.id]) void Bridge.openUrl(urls[task.id]);
      },
      openUrl: (url) => {
        if (url) void Bridge.openUrl(url);
      },
      // The three ways a card is answered (island/agents.ts), each from a click
      // on the card (a question also from its own keys), each for the request
      // the card was drawn for: a click that lands after that request has left
      // the card answers nothing, and never the one that came up in its place.
      decide: (d, requestId) => {
        // Enough of the id to follow it through the log, never the whole of it.
        void Bridge.log(`decide ${d} req=${requestId.slice(0, 8)}`);
        if (!decideRequest(d, requestId)) return;
        Sound.play(d === "deny" ? "blip" : "approve");
        this.afterAnswer();
      },
      answer: (answers, other, requestId) => {
        if (!answerRequest(answers, other, requestId)) return;
        Sound.play("approve");
        this.afterAnswer();
      },
      answerInTerminal: (requestId) => {
        if (!returnRequest(requestId)) return;
        Sound.play("blip");
        this.afterAnswer();
      },
      keysAllowed: () => this.wasInIsland || (this.keyboard && this.keyboardByClick),
      toggleSound: () => {
        State.settings.soundEnabled = !State.settings.soundEnabled;
        Sound.setEnabled(State.settings.soundEnabled);
        // Switched on, it is heard to be on.
        if (State.settings.soundEnabled) Sound.play("tick");
        void Bridge.saveSettings(State.settings);
        State.notify();
      },
      setVolume: (v) => {
        State.settings.soundVolume = v;
        Sound.setVolume(v);
        void Bridge.saveSettings(State.settings);
        State.notify();
      },
      setAutoClose: (s) => {
        State.settings.autoCloseInterval = s;
        this.fsm.openToPeekDelay = s;
        void Bridge.saveSettings(State.settings);
        State.notify();
      },
      openSettingsWindow: () => void Bridge.openSettingsWindow(),
      blip: () => Sound.play("blip"),
      openSection: (key) => this.openSection(key),
      activeSection: () => this.sections.active,
      level: () => this.arbiter.level?.notice ?? null,
      featureOn,
    };

    this.wakeStrip = h("div", { id: "wake-strip" });
    this.botCanvas = h("canvas", { id: "bot-canvas" });
    this.botDot.id = "bot-dot";
    this.greetingCanvas = h("canvas", { id: "greeting-canvas" });
    this.countdown = h("div", { id: "countdown" });

    this.header = buildHeader(actions);
    // The fold button, last on the header's right: one click instead of Esc or the countdown.
    this.foldBtn = tab("chevronUp", tl("Fold"), () => {
      Sound.play("blip");
      this.collapse();
    });
    this.foldBtn.classList.add("icon");
    this.header.el.querySelector(".isl-head-right")?.append(this.foldBtn);
    this.views = buildViews(actions, () => this.animateGeometry());
    this.viewsEl = h("div", { id: "views" });
    for (const v of this.views.values()) this.viewsEl.append(v.el);
    this.sections = new Sections(this.viewsEl);
    try {
      this.lastTool = localStorage.getItem(LAST_TOOL);
    } catch {
      // No storage: the row starts empty.
    }
    // A tool acts once the island has folded: the window the user was in is in front again by then.
    setBuilder("island.utilities", () =>
      buildUtilities((feature, id) => {
        this.collapse();
        window.setTimeout(() => runTool(feature, id), 200);
      }),
    );
    setBuilder(HOME_KEY, () =>
      buildHome({
        openSection: (key) => this.openSection(key),
        openAgents: () => this.setView(State.defaultView()),
        section: () => (State.mode === "expanded" && State.view === "section" ? this.sections.active : null),
        lastTool: () => {
          const s = this.sections.find(this.lastTool);
          return s ? { key: s.key, icon: s.def.icon, title: s.def.title } : null;
        },
      }),
    );
    this.contentEl = h("div", { id: "content" }, this.header.el, this.viewsEl, this.reader);
    this.viewsEl.addEventListener("click", (e) => this.readMore(e));
    // A card that grows under the pointer takes the mascot with it: a frame for it, when no other is coming
    // (motion reduced, or a dot in the mascot's place).
    const grows = (e: MouseEvent) => {
      if (State.settings.hoverEnlarge && (e.target as Element).closest?.(GROWS)) this.invalidate();
    };
    this.viewsEl.addEventListener("mouseover", grows);
    this.viewsEl.addEventListener("mouseout", grows);
    this.reader.addEventListener("click", () => this.closeReader());

    // The drop sequence draws the card, the bar and its own mascot. It sits under
    // the header, which stays visible on top of it.
    this.uploadCanvas = new UploadCanvas({
      ask: () => {
        State.promptContext = State.droppedFile
          ? { kind: "file", name: State.droppedFile.name, path: State.droppedFile.path }
          : null;
        this.setView("prompt");
      },
      cancel: () => this.discardDrop(),
      // chat-plus: the dropped file by email, through its own window.
      mail: () => {
        const path = State.droppedFile?.path;
        if (path) void Bridge.featureCall("chat-plus", "mail_open", { path });
      },
      hasMail: () => featureOn("chat-plus"),
    });

    // A strip that stays is put away from its right end: the design language's
    // small ghost button, on the island's own black so the reading under it does not show through.
    const put = iconBtn("chevronUp", tl("Hide"), () => {
      Sound.play("blip");
      this.dismiss();
    });
    put.classList.add("sm");
    this.foldMark = h(
      "span",
      {
        id: "strip-fold",
        style:
          "position:absolute;top:var(--s-1);right:var(--s-1);z-index:1;display:flex;border-radius:var(--r-sm);" +
          "background:var(--bg);box-shadow:calc(var(--s-2) * -1) 0 var(--s-2) var(--bg);" +
          "opacity:0;pointer-events:none;transition:opacity var(--d-fast) var(--ease-out)",
      },
      put,
    );
    // Resting on it must not open the island under the click (hover-open).
    this.foldMark.addEventListener("mouseenter", () => this.fsm.overControl(true));
    this.foldMark.addEventListener("mouseleave", () => this.fsm.overControl(false));
    // First, so the strip's last child stays the row that is up (the journeys of §4.2 start there).
    this.strip.el.prepend(this.foldMark);
    // The same for every button of the strip (a notice's action, Pause and Stop of a recording): a pointer
    // that takes a third of a second to press it must find it still there.
    const onButton = (el: EventTarget | null) => !!(el as Element | null)?.closest?.("button");
    this.strip.el.addEventListener("mouseover", (e) => onButton(e.target) && this.fsm.overControl(true));
    this.strip.el.addEventListener("mouseout", (e) => onButton(e.target) && !onButton(e.relatedTarget) && this.fsm.overControl(false));

    this.clipEl = h(
      "div",
      { id: "island-clip" },
      this.uploadCanvas.el,
      this.strip.el,
      this.greetingCanvas,
      this.contentEl,
      this.travel.el,
    );
    this.islandEl = h("div", { id: "island" }, this.clipEl, this.botCanvas, this.botDot, this.countdown);

    // The greeting plays on the peek strip: the canvas is the strip's left end,
    // tall enough below it for the mascot to start out of sight.
    const dpr = Math.min(2, window.devicePixelRatio || 1);
    this.greetingCanvas.width = Math.round(64 * dpr);
    this.greetingCanvas.height = Math.round(NOTCH_H * dpr);

    this.root.append(this.wakeStrip, this.islandEl, this.earEl);
    this.applyGeometry();
  }

  // ── FSM ─────────────────────────────────────────────────────────────────────

  private wireFsm() {
    this.fsm.openToPeekDelay = State.settings.autoCloseInterval;
    this.fsm.openOnHover = State.settings.openOnHover;
    this.fsm.onTransition = (from, to) => {
      if (from === "greet") this.greeting.interrupt();
      switch (to) {
        case "hidden":
          this.wakeSpent = this.cursorKnown && this.onWakeStrip();
          this.setMode("hidden");
          break;
        case "peek":
          if (from === "hidden") Sound.play("peek");
          this.setMode("compact");
          // What waited behind the open island or a card may show now.
          this.arbiter.flush();
          break;
        case "open": {
          // Opened from the strip: on what the strip was showing, when it has a page.
          // And when the strip shows nothing of its own, on the tab the user chose (Settings › Island "Open on").
          const section = this.forced ? undefined : this.strip.target.section ?? (State.settings.openOn || undefined);
          if (section && this.sections.find(section)) {
            this.sections.active = section;
            this.expand("section");
          } else {
            // Straight onto the view it was opened for: passing by the home view
            // first would end a drop sequence that has only just begun.
            this.expand(this.forced ?? State.defaultView());
          }
          this.arbiter.dismiss();
          // Hooks may have been installed in a terminal since: the idle cards
          // say so on the next open, without polling while the island is shut.
          void refreshHookPills();
          break;
        }
        case "greet":
          this.setMode("compact");
          this.greeting.start();
          break;
      }
      State.notify();
      if (from === "hidden") this.onWake?.();
    };
  }

  /** At launch: the greeting, or just the strip for a moment when the mascot is hidden. */
  launch() {
    // The page starts believing the window is the full panel: make it so (a
    // reloaded page would otherwise draw into the 240×6 wake strip).
    void Bridge.setCollapsed(false);
    if (State.settings.showMascot) {
      this.fsm.launch();
      return;
    }
    this.reveal(2000);
    this.onGreetingDone?.();
  }

  // ── Mode / view ─────────────────────────────────────────────────────────────

  private setMode(mode: IslandMode) {
    const prev = State.mode;
    if (mode === prev) return;
    State.mode = mode;
    if (mode === "expanded") Sound.play("open");
    if (prev === "expanded") {
      Sound.play("close");
      // A folded card is still waiting: it keeps the island pinned.
      if (!State.pendingApproval) State.isPinned = false;
      // Folded, hidden, closed for any reason: the keyboard goes back (§4).
      this.releaseKeyboard(true);
    }
    // What a hint pointed at is leaving or changing place.
    hideTip();
    if (mode !== "expanded") {
      this.closeReader();
      closePlanCard();
      // Nothing can be seen of the sequence once the island is shut, and leaving
      // it running would keep the frame loop awake — the island must cost
      // nothing while hidden.
      UploadSeq.deactivate();
    }
    this.updateWindowCollapsed();
    this.animateGeometry();
    State.notify();
  }

  /**
   * A click on text that its card cut short: all of it, in the same letters
   * and colour, over the view. Never over a card that waits: nothing covers
   * its buttons.
   */
  private readMore(e: MouseEvent) {
    const at = e.target as Element;
    if (State.pendingApproval || at.closest(CONTROLS)) return;
    const el = at.closest<HTMLElement>(READS);
    const text = el?.textContent?.trim();
    if (!el || !text || !(el.scrollWidth > el.clientWidth + 1 || el.scrollHeight > el.clientHeight + 1)) return;
    const style = getComputedStyle(el);
    const page = h("div", { class: "card", text });
    page.style.font = style.font;
    page.style.color = style.color;
    this.reader.replaceChildren(page);
    this.reader.classList.add("on");
    this.invalidate();
  }

  private closeReader() {
    if (!this.reader.classList.contains("on")) return false;
    this.reader.classList.remove("on");
    this.invalidate();
    return true;
  }

  /** True while the drop sequence owns the island body. */
  private get uploadActive(): boolean {
    return State.mode === "expanded" && UploadSeq.isActive && UPLOAD_VIEWS.has(State.view);
  }

  /** Navigating out of the drop flow ends the sequence. */
  private stopSequenceIfLeaving(view: IslandViewName) {
    if (UploadSeq.isActive && !UPLOAD_VIEWS.has(view)) UploadSeq.deactivate();
  }

  expand(view: IslandViewName) {
    this.closeReader();
    this.stopSequenceIfLeaving(view);
    if (view !== "overview") closePlanCard();
    State.view = view;
    if (State.mode !== "expanded") this.setMode("expanded");
    else this.animateGeometry();
    State.lastActivity = performance.now();
    State.notify();
  }

  setView(view: IslandViewName) {
    this.closeReader();
    this.stopSequenceIfLeaving(view);
    if (view !== "overview") closePlanCard();
    if (State.mode !== "expanded") this.forceOpen(view);
    State.view = view;
    State.lastActivity = performance.now();
    this.animateGeometry();
    State.notify();
  }

  /** Opens the island on a feature's section (`<feature>.<id>`), if that feature is on. */
  openSection(key: string) {
    if (!this.sections.find(key)) return;
    this.sections.active = key;
    this.setView("section");
  }

  private forceOpen(view: IslandViewName) {
    this.forced = view;
    this.fsm.forceOpen();
    this.forced = null;
  }

  collapse() {
    // A waiting card is only ever folded, never dropped by a close.
    if (State.pendingApproval) {
      this.foldApproval();
      return;
    }
    State.isPinned = false;
    this.fsm.pinned = false;
    // Drive the state machine rather than the mode: setting the mode behind its
    // back left it thinking the island was still open, and a click on the compact
    // island then did nothing — the island could never be reopened.
    this.fsm.forcePeek();
  }

  /**
   * A press somewhere else while the island is open: it folds at once (§4).
   * Never a card that waits, a pinned island, or one a feature holds (a drag,
   * a share sheet: the press may be on what it holds the island for).
   */
  private outsidePress() {
    if (State.mode !== "expanded" || State.pendingApproval || State.isPinned) return;
    for (const mine of this.holds.values()) if (mine.size > 0) return;
    this.collapse();
  }

  /** The tab after the one that is on (or before it): Home, Chat, then the sections in their order. A card that waits is not left this way. */
  cycleTab(step: number) {
    if (State.pendingApproval) return;
    const tabs = ["home", ...(featureOn("chat") ? ["prompt"] : []), ...sections().map((s) => s.key)];
    const at = State.view === "prompt" ? "prompt" : State.view === "section" ? this.sections.active ?? "home" : "home";
    const next = tabs[(Math.max(0, tabs.indexOf(at)) + step + tabs.length) % tabs.length];
    Sound.play("blip");
    if (next === "home" || next === "prompt") this.setView(next === "home" ? State.defaultView() : "prompt");
    else this.openSection(next);
  }

  /** Alert from the hook server: open on this view. Pinned alerts never auto-close. */
  alert(view: IslandViewName) {
    this.fsm.pinned = State.isPinned;
    this.forceOpen(view);
    this.expand(view);
  }

  /** Slides the compact strip out for `ms` if the island is hidden (§4). */
  reveal(ms = ACTIVITY_REVEAL_MS, exact = false) {
    if (State.paused || this.arbiter.quiet) return;
    this.fsm.reveal(ms, exact);
  }

  /** An alert stopped waiting for an answer: let the island auto-close again. */
  dropPin() {
    this.fsm.unpin();
    this.arbiter.flush();
  }

  // ── Keyboard shortcuts (island/shortcuts.ts) ────────────────────────────────

  emote(name: BotEmoteName) {
    this.engine.triggerEmote(name);
    this.ensureRunning();
  }

  /** Ctrl+P: keep the open island from folding away, or let it fold again. */
  setPinned(on: boolean) {
    State.isPinned = on;
    if (on) {
      this.fsm.pinned = true;
      // The countdown bar reads the state machine's deadline, cleared with it.
      this.fsm.cancelTimers();
    } else {
      this.fsm.unpin();
    }
    State.notify();
  }

  /**
   * The island takes the keyboard: every key goes to it, text fields can be
   * typed in. A click inside it, a shortcut that opens it, a view with a text
   * field reached by either. Asked again while it is held, it only makes sure
   * the page has the keys.
   */
  takeKeyboard() {
    if (!this.keyboard) this.pointerSeen = this.wasInIsland;
    this.keyboard = true;
    this.cancelLeave();
    this.keyboardQueue = this.keyboardQueue.then(() => Bridge.focusWindow(true)).then((held) => {
      // Windows refused (false; null is a page outside the app): the keys under the pointer still work.
      if (held === false) this.keyboardGone();
    });
  }

  /** Hands the keyboard back to the window it was taken from. `always` asks Rust even when the page believes it holds none. */
  private releaseKeyboard(always = false) {
    this.cancelLeave();
    if (!this.keyboard && !always) return;
    this.keyboard = false;
    this.keyboardByClick = false;
    this.pointerSeen = false;
    this.keyboardQueue = this.keyboardQueue.then(() => Bridge.focusWindow(false));
  }

  /** The user gave the keyboard to another window (a click outside), or Windows refused it. */
  private keyboardGone() {
    this.keyboard = false;
    this.keyboardByClick = false;
    this.pointerSeen = false;
    this.cancelLeave();
    // No text cursor is left blinking in a window that has no keyboard.
    (document.activeElement as HTMLElement | null)?.blur?.();
  }

  /** A text field of the island has the text cursor. */
  private typingInField(): boolean {
    return document.activeElement?.matches?.(TEXT_ENTRY) === true;
  }

  private cancelLeave() {
    if (this.leaveTimer != null) window.clearTimeout(this.leaveTimer);
    this.leaveTimer = null;
  }

  /**
   * The pointer has left an island that holds the keyboard: unless text is
   * being typed or something is being dragged, the keyboard goes back a moment
   * later. The island stays open; under the pointer its own keys work again
   * without it (§4.1).
   */
  private pointerLeft() {
    if (!this.keyboard || !this.pointerSeen || this.leaveTimer != null) return;
    this.leaveTimer = window.setTimeout(() => {
      this.leaveTimer = null;
      if (this.wasInIsland || this.buttonDown || this.typingInField()) return;
      if (this.keyboard) this.releaseKeyboard();
    }, 250);
  }

  /**
   * A key of the island's, read by Rust under the pointer while the island does
   * not hold the keyboard (src/island/keys.ts): the page's own listeners get it
   * as if it had been typed here. A key is as deliberate as a click.
   */
  private islandKey(code: number) {
    const init = comboEvent(code);
    if (!init) return;
    // On a strip that stays, Esc under the pointer puts it away: the one key read there.
    if (State.mode === "compact" && init.key === "Escape") this.fsm.escape();
    if (State.mode !== "expanded") return;
    this.fsm.userInteracted();
    document.body.dispatchEvent(new KeyboardEvent("keydown", { ...init, bubbles: true, cancelable: true }));
    this.invalidate();
  }

  /** What Rust reads for the island under the pointer right now: exactly what the island would act on (§4). */
  private islandKeys(): number[] {
    if (State.paused) return [];
    // A strip that stays (Settings › Island): Esc puts it away, under the pointer like every key here.
    if (State.mode === "compact" && this.fsm.kept) return comboCodes(["Escape"]);
    if (State.mode !== "expanded") return [];
    const names = ["Escape"];
    const all = sections();
    // Ctrl+K: the gallery, or a new chat while the chat is up (island/shortcuts.ts).
    if (State.view === "prompt" || (State.settings.keyJumps && all.length > 0)) names.push("Ctrl+K");
    if (State.settings.keyJumps) for (const s of all) if (s.def.letter) names.push(`Alt+${s.def.letter}`);
    if (State.view === "section") names.push(...(this.sections.find(this.sections.active)?.def.keys ?? []));
    // The question card's own keys (1–9, Enter), read for it while the pointer
    // rests on it: the one place a digit alone is the island's. A permission
    // card names no key at all.
    const question = State.view === "question" ? this.views.get("question")?.keys?.() ?? [] : [];
    return [...new Set([...comboCodes(names), ...comboCodes(question, true)])].sort((a, b) => a - b);
  }

  /** Ctrl+O / Ctrl+E go to the view on screen. */
  viewCommand(command: ViewCommand) {
    this.views.get(State.view)?.command?.(command);
  }

  // ── File drop ───────────────────────────────────────────────────────────────

  private onDragDrop(e: { type: string; paths?: string[] }) {
    if (e.type !== "over") void Bridge.log(`drag ${e.type} ${e.paths?.length ?? 0} file(s)`);
    if (State.paused || !featureOn("drop")) return;
    // A card that waits is never covered: a file dragged over it is not taken.
    if (State.pendingApproval) {
      State.fileDragOver = false;
      return;
    }
    switch (e.type) {
      case "enter":
      case "over": {
        if (State.fileDragOver) return;
        State.fileDragOver = true;
        // enterZone must run before the island expands, so the sequence is
        // already active by the time the view becomes `upload`.
        UploadSeq.enterZone(State.mouseInIsland.x, State.mouseInIsland.y);
        this.alert("upload");
        break;
      }
      case "leave": {
        if (!State.fileDragOver) return;
        State.fileDragOver = false;
        // The island deliberately stays open: the drag session is still alive.
        UploadSeq.exitZone();
        State.notify();
        break;
      }
      case "drop": {
        State.fileDragOver = false;
        const path = e.paths?.[0];
        if (!path) {
          this.setView(State.defaultView());
          return;
        }
        this.swallow(path);
        break;
      }
    }
  }

  /**
   * The mascot takes the file. Nothing here waits on the file system: the copy
   * into the inbox runs in the background and swaps the path in when it lands,
   * so a slow disk can never stall the animation.
   */
  private swallow(path: string) {
    const name = path.split(/[\\/]/).pop() || "file";
    State.droppedFile = { name, path };
    State.promptContext = { kind: "file", name, path };
    State.chatHistory = [];
    void Bridge.chatReset();

    // Home first: opening a closed island shuts the sequence down.
    this.setView("uploading");
    UploadSeq.performDrop(State.uploadDuration);
    this.uploadTens = 0;
    this.uploadDone = false;

    Sound.play("approve");
    this.engine.triggerEmote("happy");

    State.uploadProgress = 0;
    this.ensureRunning();

    void Bridge.ingestFile(path)
      .then((file) => {
        // Cancelled (or replaced by another drop) while the copy was running.
        if (State.droppedFile?.path !== path) return;
        State.droppedFile = { name: file.name, path: file.path };
        State.promptContext = { kind: "file", name: file.name, path: file.path };
        State.notify();
      })
      .catch((err) => {
        if (State.droppedFile?.path !== path) return;
        // Nothing was taken: the chat must not go on to send a file that never arrived.
        State.droppedFile = null;
        State.promptContext = null;
        UploadSeq.deactivate();
        State.noteMessage = String(err).replace(/^Error:\s*/, "");
        this.setView("note");
        Sound.play("error");
        window.setTimeout(() => {
          if (State.mode === "expanded" && State.view === "note") this.setView(State.defaultView());
        }, 2400);
      });
  }

  /** "Cancel" on the dropped file: forget it, so the chat does not pick it up. */
  private discardDrop() {
    State.droppedFile = null;
    State.promptContext = null;
    this.setView(State.defaultView());
  }

  /**
   * Sounds and view changes hung off the canvas timeline: a `tick` every 10 %,
   * the ✓ chime when the bar completes, then `choose` once the mascot is back.
   */
  private stepSequence() {
    const since = UploadSeq.sinceDrop();
    if (since == null) return;
    const dur = State.uploadDuration;
    const p = Math.max(0, Math.min(1, (since - PRE_PROGRESS) / dur));

    const tens = Math.floor(p * 10);
    if (tens > this.uploadTens && tens < 10) {
      this.uploadTens = tens;
      Sound.play("tick");
    }

    if (!this.uploadDone && since >= PRE_PROGRESS + dur) {
      this.uploadDone = true;
      Sound.play("approve");
      this.engine.triggerEmote("happy");
    }
    // The extra second is the grow-back, after which the choose card is up.
    if (since >= PRE_PROGRESS + dur + 1 && State.view === "uploading") {
      this.setView("choose");
    }
  }

  // ── Geometry ────────────────────────────────────────────────────────────────

  private targetSize(): { w: number; h: number; r: number } {
    let { w, h } = islandSize(State.mode, State.view, State.chatHistory.length);
    if (State.mode === "compact") w = this.strip.width;
    if (State.mode === "expanded") {
      if (State.view === "question" && State.pendingApproval?.questions) h = QUESTION_PICKER_H;
      else if (State.view === "finished" && finishedIsTall(State.focusTask)) h = FINISHED_TALL_H;
      else if (State.view === "overview" && hasChips(State.focusTask)) h += CHIPS_H;
      else if (State.view === "section") h = this.sections.height();
      else if (State.view === "gallery") h = galleryHeight(sections().length);
      else if (State.view === "prompt" && State.pickingModel) h = chatPromptHeight(Infinity);
    }
    const r = State.mode === "expanded" ? EXPANDED_CORNER : ROUNDED_CORNER;
    return { w, h, r };
  }

  /** Grows on the spring, shrinks on the 340 ms curve (§7), each side on its own. */
  private animateGeometry() {
    const { w, h, r } = this.targetSize();
    const go = (value: Tracked, to: number, from: number) => {
      if (to < from) value.curveTowards(to);
      else if (to > from) value.springTo(to);
    };
    go(this.width, w, this.target.w);
    go(this.height, h, this.target.h);
    go(this.radius, r, this.radius.value);
    const reshaped = w !== this.target.w || h !== this.target.h;
    this.target = { w, h };
    this.tellShape();
    // A pointer that rests is not reported again: it may now be off the island
    // (Esc with the pointer on the body), or on it. Asked once this change is through.
    if (reshaped) queueMicrotask(() => this.pointerCheck());
    // The content is laid out once, at the size the island is heading for.
    if (State.mode === "expanded") this.contentEl.style.height = `${h}px`;
    this.ensureRunning();
  }

  private applyGeometry() {
    const w = pixelWidth(this.width.value);
    const hh = Math.max(0, this.height.value);
    const r = this.radius.value;
    this.islandEl.style.width = `${w}px`;
    this.islandEl.style.height = `${hh}px`;
    this.islandEl.style.borderRadius = this.floating ? `${r}px` : `0 0 ${r}px ${r}px`;
    const rect = { x: (PANEL_W - w) / 2, y: this.top, w, h: hh };
    const p = this.pushedRect;
    if (Math.abs(p.x - rect.x) > 0.5 || p.y !== rect.y || Math.abs(p.w - rect.w) > 0.5 || Math.abs(p.h - rect.h) > 0.5) {
      this.pushedRect = rect;
      void Bridge.setIslandRect(rect.x, rect.y, rect.w, rect.h);
    }
  }

  /** Island rect in window coordinates (origin top-left of the 720×360 window). */
  private islandRect(): { x: number; y: number; w: number; h: number } {
    const w = this.width.value;
    const hh = this.height.value;
    return { x: (PANEL_W - w) / 2, y: 0, w, h: hh };
  }

  // ── Window collapse (hidden → tiny wake strip, zero polling) ────────────────

  private updateWindowCollapsed() {
    if (this.collapseTimer != null) {
      window.clearTimeout(this.collapseTimer);
      this.collapseTimer = null;
    }
    if (State.mode === "hidden") {
      // Let the island finish retracting, then drop the window to the wake strip:
      // from there the OS delivers no cursor events, so nothing polls at all.
      this.collapseTimer = window.setTimeout(() => {
        this.collapseTimer = null;
        if (State.mode !== "hidden") return;
        this.collapsed = true;
        // Nothing reports the pointer from here on: the next report is the truth again.
        this.cursorKnown = false;
        void Bridge.setCollapsed(true);
      }, 420);
    } else if (this.collapsed) {
      // Grow the window back before the island animates open.
      this.collapsed = false;
      void Bridge.setCollapsed(false);
    }
  }

  // ── Input ───────────────────────────────────────────────────────────────────

  private wireInput() {
    // The wake strip is the only thing the OS can hit while the island is hidden.
    this.wakeStrip.addEventListener("mouseenter", () => {
      if (this.wakeSpent) return;
      // While the window is the panel, the cursor poll is the truth. The web view
      // goes on believing the pointer where it last saw it (a window dragged, then
      // put somewhere else than under the hand), and says "enter" when the island
      // retracts from that place: it came out again every two seconds.
      if (!this.collapsed && this.cursorKnown && !this.onWakeStrip()) return;
      Sound.resume();
      window.clearTimeout(this.wakeTimer);
      this.wakeTimer = window.setTimeout(() => {
        // Inert while paused, and under a full-screen app (§4).
        if (State.mode === "hidden" && !State.paused && !this.arbiter.quiet) this.fsm.mouseEntered();
      }, WAKE_DELAY_MS);
    });
    this.wakeStrip.addEventListener("mouseleave", () => {
      this.wakeSpent = false;
      window.clearTimeout(this.wakeTimer);
    });

    this.islandEl.addEventListener("mousedown", (e) => {
      Sound.resume();
      State.lastActivity = performance.now();
      // A click makes a hover-opened island an ordinary open one.
      this.fsm.userInteracted();
      const target = e.target as Element;
      // "Let me drag the island": a press on the strip, or on the empty part of
      // the open header, may be the start of a drag. What the click does then
      // waits for the button to come up without the pointer having travelled,
      // and a drag does none of it: it never takes the keyboard (§4.1).
      const mayDrag = IS_TAURI && State.settings.islandDrag && e.button === 0;
      const or = (click: () => void) => {
        if (mayDrag) this.press = { x: e.screenX, y: e.screenY, click };
        else click();
      };
      if (State.mode !== "expanded") {
        // A button of the strip (a notice's action) is its own click.
        if (target.closest?.("button")) return;
        or(() => {
          const strip = State.mode === "compact";
          const own = strip ? this.strip.target.onClick : undefined;
          if (own) return own();
          if (strip) this.fsm.click();
          // A click takes the keyboard (§4.1), every time.
          this.keyboardByClick = true;
          this.takeKeyboard();
        });
        return;
      }
      // A click inside the island gives it the keyboard, wherever it lands and
      // however often the user has clicked out and back in (§4.1).
      const click = () => {
        this.keyboardByClick = true;
        this.takeKeyboard();
      };
      if (target === this.header.el || target.classList?.contains("isl-gap")) or(click);
      else click();
      if (this.isBotHit(e.clientX, e.clientY - this.top)) {
        this.cancelBotHover();
        this.slap();
      }
    });

    // Wheel on the peek strip: down opens, up puts it away. One decision per
    // gesture: what follows within 0.35 s belongs to the same one.
    // Settings › Island "Wheel on the strip" turns it off.
    this.islandEl.addEventListener(
      "wheel",
      (e) => {
        if (!State.settings.wheelGesture) return;
        const now = performance.now();
        const fresh = now - this.lastWheel > 350;
        this.lastWheel = now;
        if (!fresh || State.mode !== "compact" || e.deltaY === 0) return;
        if (e.deltaY > 0) this.fsm.click();
        else if (!this.fsm.held) this.fsm.forceHidden();
      },
      { passive: true },
    );

    // No browser menu over the mascot. Everywhere else (the chat field) the
    // webview keeps its own menu.
    this.islandEl.addEventListener("contextmenu", (e) => {
      if (this.isBotHit(e.clientX, e.clientY - this.top)) e.preventDefault();
    });

    // Only keys typed into the island itself land here, never keys typed in a
    // terminal: the island has the keyboard only after an explicit open.
    window.addEventListener("keydown", (e) => {
      State.lastActivity = performance.now();
      if (State.mode !== "expanded") return;
      // Somebody is typing here: the island does not fold under their hands.
      this.fsm.activity();
      if (e.key === "Escape") {
        // Esc steps back: the reader, a layer inside the section, then the island folds.
        // A waiting card folds too, and stays pinned.
        if (this.closeReader()) return;
        if (State.pendingApproval) this.foldApproval();
        else if (State.view === "section" && this.sections.command("back")) return;
        else if (!State.isPinned) this.collapse();
        return;
      }
      // Ctrl+Tab, Ctrl+Shift+Tab: the next tab of the header, the one before. A
      // base Ctrl key: it works once the island holds the keyboard (§4.1).
      if (e.ctrlKey && !e.altKey && e.key === "Tab") {
        e.preventDefault();
        this.cycleTab(e.shiftKey ? -1 : 1);
        return;
      }
      // Settings › Island "Keyboard jumps" off: neither of the two below.
      if (!State.settings.keyJumps) return;
      // Ctrl+K: the gallery of sections (in the chat it starts a new chat, as before).
      if (e.ctrlKey && !e.altKey && !e.shiftKey && e.key.toLowerCase() === "k" && sections().length > 0) {
        e.preventDefault();
        this.setView(State.view === "gallery" ? State.defaultView() : "gallery");
        return;
      }
      // Alt+letter: straight to the section that claims it.
      if (e.altKey && !e.ctrlKey && e.key.length === 1) {
        const hit = sections().find((s) => s.def.letter?.toLowerCase() === e.key.toLowerCase());
        if (!hit) return;
        e.preventDefault();
        this.openSection(hit.key);
      }
    });

    void onDragDrop((e) => this.onDragDrop(e));
    void onEvent<number>("island-key", (code) => this.islandKey(code));
    void onEvent<{ playing: boolean; bpm: number; at: number }>("audio-extras:beat", (b) => {
      this.songPlays = b.playing;
      if (b.playing) this.mood.poke();
      setDanceBeat(b.bpm, b.at);
    });
    void onEvent<null>("keyboard-lost", () => this.keyboardGone());
    void onEvent<null>("outside-press", () => this.outsidePress());
    void onEvent<null>("away", () => this.away());

    // The press travels: a drag. It comes up where it went down: the click.
    window.addEventListener("mousemove", (e) => {
      if (!this.press) return;
      if (!(e.buttons & 1)) this.press = null;
      else if (dragStarted(e.screenX - this.press.x, e.screenY - this.press.y)) this.beginDrag();
    });
    window.addEventListener("mouseup", () => {
      const press = this.press;
      this.press = null;
      press?.click();
    });
    void onEvent<[number, number]>("island-drop", ([x, y]) => void this.dropped(x, y));

    // Outside Tauri (plain browser) drive the cursor from DOM events so the
    // island can be inspected with `npm run dev`.
    if (!IS_TAURI) this.followPageCursor();
  }

  /**
   * Takes the cursor from the page's own mouse events instead of Rust's poll.
   * Used outside Tauri (browser dev): the events only fire while the pointer is over the island, so leaving the window is
   * reported as a cursor far away, which is what the poll would have said.
   */
  followPageCursor() {
    window.addEventListener("mousemove", (e) => this.onCursor(e.clientX, e.clientY));
    window.addEventListener("mouseout", (e) => {
      if (e.relatedTarget == null) this.onCursor(-10_000, -10_000);
    });
  }

  // ── Drag the island anywhere (the rules: src/island/place.ts) ────────────────

  private get top(): number {
    return this.floating ? FLOAT_TOP : 0;
  }

  /** The release that ends a drag is nobody's click. */
  private eatClick = (e: Event) => {
    e.stopPropagation();
    e.preventDefault();
  };

  /** From here Rust carries the window under the pointer (the cursor poll) until the button comes up: `island-drop`. */
  private beginDrag() {
    this.press = null;
    // Nothing folds or hides under the hand, and a card that waits stays as it is.
    this.dragHold ??= this.fsm.hold();
    window.addEventListener("click", this.eatClick, true);
    // In the air it has the floating shape, which sits lower in the window: the window goes up by as much.
    const lift = FLOAT_TOP - this.top;
    this.setFloating(true);
    void Bridge.islandDrag(lift);
  }

  /** Let go with the window's top-centre point at (x, y), screen pixels: docked or floating, remembered. */
  private async dropped(x: number, y: number) {
    const displays = (await Bridge.islandDisplays()) ?? [];
    State.settings.islandSpot = settle(x, y + this.top * (window.devicePixelRatio || 1), displays);
    await this.place(displays);
    void Bridge.saveSettings(State.settings);
    this.dragHold?.();
    this.dragHold = null;
    window.setTimeout(() => window.removeEventListener("click", this.eatClick, true), 50);
  }

  /** Puts the window where the saved spot is on the displays of today (none, or its display gone: the top centre), in the shape that goes with it. */
  async place(displays?: Display[]) {
    this.placed = JSON.stringify(State.settings.islandSpot ?? null);
    const at = resolve(State.settings.islandSpot, displays ?? (await Bridge.islandDisplays()) ?? []);
    this.setFloating(!!at?.floating);
    await Bridge.islandPlace(at && { x: at.x, y: at.y - Math.round(this.top * at.scale), floating: at.floating });
  }

  /** Tray and Settings "Put the island back": the top centre again. */
  putBack() {
    State.settings.islandSpot = null;
    void this.place();
    void Bridge.saveSettings(State.settings);
  }

  private setFloating(on: boolean) {
    if (this.floating === on) return;
    this.floating = on;
    document.documentElement.dataset.place = on ? "float" : "dock";
    this.applyGeometry();
    this.tellShape();
  }

  /** Cursor in window-logical coordinates; `down` while the left button is held. */
  onCursor(x: number, y: number, down = false) {
    this.tray.back();
    // Everything below is measured from the island's own top edge.
    y -= this.top;
    this.buttonDown = down;
    State.mouse = { x, y };
    const rect = this.islandRect();
    State.mouseInIsland = { x: x - rect.x, y: y - rect.y };

    // Windows sends no cursor position with an OLE drag, so the drop sequence is
    // fed from the Win32 cursor poll instead — it runs throughout the drag.
    if (UploadSeq.isActive && !UploadSeq.dropped) {
      UploadSeq.updateCursor(State.mouseInIsland.x, State.mouseInIsland.y);
    }

    this.cursorKnown = true;
    // Not while the window is the wake strip itself: a late report is then in that window's coordinates, and the strip's own mouseleave is the truth.
    if (this.wakeSpent && !this.collapsed && !this.onWakeStrip()) this.wakeSpent = false;
    this.pointerCheck();

    // Resting on the mascot → love
    const overBot = State.mode === "expanded" && State.stateOverride == null && this.isBotHit(x, y);
    if (overBot && !this.botHovering) this.botHoverIn(x, y);
    if (!overBot && this.botHovering) this.cancelBotHover();
    this.botHovering = overBot;
    if (this.botHovering) {
      const d = Math.hypot(x - this.botHoverStart.x, y - this.botHoverStart.y);
      if (d > 40) {
        this.botHoverStart = { x, y };
        this.scheduleLove();
      }
    }

    this.ensureRunning();
  }

  /** The pointer, as the cursor poll last gave it, is on the wake strip (240×6 at the top centre, §4). */
  private onWakeStrip(): boolean {
    const { x, y } = State.mouse;
    // A floating island's wake strip is its pill.
    const [w, hh] = this.floating ? [PILL_W, PILL_H] : [NOTCH_W, 6];
    return y <= hh + 1 && Math.abs(x - PANEL_W / 2) <= w / 2 + 1;
  }

  /**
   * Is the pointer on the island? Against the shape the island is taking, not
   * the one it is still drawn at: asked when the pointer moves, and when the
   * island changes shape under a pointer that rests.
   */
  private pointerCheck() {
    if (!this.cursorKnown) return;
    const { x, y } = State.mouse;
    const left = (PANEL_W - this.target.w) / 2;
    const inIsland =
      x >= left - HIT_MARGIN && x <= left + this.target.w + HIT_MARGIN &&
      y >= -HIT_MARGIN && y <= this.target.h + HIT_MARGIN;

    // The state machine keeps its own idea of where the pointer is (the wake
    // strip tells it first, a reveal assumes it is away): this is the truth.
    this.wasInIsland = inIsland;
    if (inIsland) this.mood.poke();
    this.syncFold();
    // The window lets the pointer through from here on: the page is not told that it left.
    if (!inIsland) hideTip();
    if (inIsland) {
      this.cancelLeave();
      if (this.keyboard) this.pointerSeen = true;
    } else if (!this.buttonDown) this.pointerLeft();
    if (inIsland !== this.fsm.pointerInside && !State.paused) {
      if (inIsland) this.fsm.mouseEntered();
      else this.fsm.mouseLeft();
    }
  }

  private get mascotShown(): boolean {
    return State.settings.showMascot && this.botCanvas.style.opacity !== "0";
  }

  private isBotHit(x: number, y: number): boolean {
    if (!this.mascotShown) return false;
    const rect = this.islandRect();
    const cx = rect.x + rect.w / 2 + this.botXc.value;
    const cy = rect.y + this.botCy.value;
    const radius = Math.max(11, this.botSize.value / 2);
    return (x - cx) ** 2 + (y - cy) ** 2 <= radius * radius;
  }

  private botHoverIn(x: number, y: number) {
    if (performance.now() / 1000 - this.lastLoveTime < 6) return;
    this.botHoverStart = { x, y };
    Sound.play("hover");
    this.scheduleLove();
  }

  private scheduleLove() {
    if (this.botHoverTimer != null) window.clearTimeout(this.botHoverTimer);
    this.botHoverTimer = window.setTimeout(() => {
      this.botHoverTimer = null;
      if (!this.botHovering || State.stateOverride != null) return;
      if (performance.now() / 1000 - this.lastLoveTime < 6) return;
      this.lastLoveTime = performance.now() / 1000;
      this.engine.triggerEmote("love");
      Sound.play("love");
    }, 1900);
  }

  private cancelBotHover() {
    if (this.botHoverTimer != null) window.clearTimeout(this.botHoverTimer);
    this.botHoverTimer = null;
  }

  /** A click on the mascot annoys it; the third within 1.7 s makes it dizzy. */
  private slap() {
    if (State.stateOverride === "dizzy") return;
    const now = performance.now();
    this.slaps = this.slaps.filter((t) => now - t < 1700);
    this.slaps.push(now);
    if (this.slaps.length >= 3) {
      this.slaps = [];
      this.handleDizzy();
    } else {
      this.engine.triggerEmote("annoyed");
      Sound.play("annoyed");
    }
  }

  /** Three clicks → dizzy + confused view for 3.3 s, then back. */
  handleDizzy() {
    this.prevViewBeforeConfused = State.view;
    State.stateOverride = "dizzy";
    this.engine.setState("dizzy");
    Sound.play("dizzy");
    this.alert("confused");
    if (this.confusedRecovery != null) window.clearTimeout(this.confusedRecovery);
    this.confusedRecovery = window.setTimeout(() => {
      this.confusedRecovery = null;
      State.stateOverride = null;
      this.engine.setState(this.mood.apply(State.effectiveState));
      if (State.view === "confused") {
        const fallback = State.defaultView();
        this.setView(this.prevViewBeforeConfused === "confused" ? fallback : this.prevViewBeforeConfused);
      }
      this.engine.triggerEmote("happy");
    }, 3300);
  }

  // ── Frame loop ──────────────────────────────────────────────────────────────

  ensureRunning() {
    // Something changed while the strip rested: it is drawn now, not at the next slow frame.
    if (this.restTimer != null) {
      window.clearTimeout(this.restTimer);
      this.restFrame();
      return;
    }
    if (this.running) return;
    this.running = true;
    this.lastFrame = performance.now();
    requestAnimationFrame(this.frame);
  }

  private restFrame = () => {
    this.restTimer = null;
    requestAnimationFrame(this.frame);
  };

  /** Says to Rust when the strip begins and ends its rest: the cursor poll follows (`set_poll_rest`). */
  private setRest(on: boolean) {
    if (on === this.rest) return;
    this.rest = on;
    void Bridge.setPollRest(on);
  }

  private frame = (nowMs: number) => {
    let busy = false;
    try {
      busy = this.step(nowMs);
    } catch (err) {
      // One bad frame (a section, an activity, the mascot) must not leave the
      // island frozen with the loop marked as running: stop, and let the next
      // change start it again.
      console.error("[island] frame failed", err);
      void Bridge.log(`island frame failed: ${String(err)}`);
    }
    if (busy) {
      if (this.rest) this.restTimer = window.setTimeout(this.restFrame, REST_FRAME_MS);
      else requestAnimationFrame(this.frame);
    } else {
      this.running = false;
      Sound.idle();
    }
  };

  /** One frame. True = something is still moving and wants the next one. */
  private step(nowMs: number): boolean {
    const elapsed = (nowMs - this.lastFrame) / 1000;
    const dt = Math.min(0.05, elapsed);
    this.lastFrame = nowMs;

    this.width.step(dt, nowMs);
    this.height.step(dt, nowMs);
    this.radius.step(dt, nowMs);
    this.applyGeometry();

    if (this.dirty) {
      this.dirty = false;
      this.syncDom();
    }

    const mascot = State.settings.showMascot;
    const greeting = this.fsm.state === "greet";
    this.updateBotTargets(greeting, nowMs);
    this.botXc.step(dt, nowMs);
    this.botCy.step(dt, nowMs);
    this.botSize.step(dt, nowMs);
    this.placeBot();
    const travelling = this.travel.step(dt, nowMs);

    if (greeting) {
      const gctx = this.greetingCanvas.getContext("2d");
      if (gctx) {
        const dpr = Math.min(2, window.devicePixelRatio || 1);
        gctx.setTransform(dpr, 0, 0, dpr, 0, 0);
        this.greeting.draw(gctx, STRIP_BOT.cx, STRIP_BOT.cy, STRIP_BOT.diameter);
      }
    } else if (mascot && State.mode !== "hidden") {
      // Kept running even while the drop canvas is up, so the island's own mascot
      // is already in the right place the moment the canvas fades out.
      // After a slow frame of a strip that rests, the mascot moves by the time that really passed.
      this.drawBot(this.rest ? Math.min(0.1, elapsed) : dt);
    }

    const uploadActive = this.uploadActive;
    if (uploadActive) this.uploadCanvas.draw(UploadSeq.frame(), nowMs / 1000);
    this.uploadCanvas.el.classList.toggle("on", uploadActive);
    this.viewsEl.classList.toggle("hidden-by-upload", uploadActive);

    if (mascot) tickMiniBots(dt);
    const viewAnimating = this.views.get(State.view)?.tick?.(nowMs) === true;
    if (UploadSeq.isActive) this.stepSequence();
    this.updateCountdown(nowMs);

    // Nothing is drawn while the island is hidden, so nothing may keep the loop
    // alive either: the mascot always has a next frame (it breathes), and a
    // hidden island would go on burning frames in the state it spends most of
    // its life in. Geometry still has to finish retracting.
    const settling = this.width.animating || this.height.animating || this.radius.animating;
    const moving =
      settling || travelling ||
      this.botXc.animating || this.botCy.animating || this.botSize.animating ||
      greeting || UploadSeq.isActive || viewAnimating ||
      (this.fsm.openCollapseDueAt != null && this.cdDue !== this.fsm.openCollapseDueAt);
    // A strip the setting keeps out, with the pointer away and nothing moving,
    // rests (§4): idle means idle. A mascot that dances is something moving.
    this.setRest(this.fsm.kept && this.cursorKnown && !this.wasInIsland && !moving && !(mascot && this.botSeen && this.dances()));
    // At rest a mascot nobody sees (an activity has the strip) needs no frame at all.
    return State.mode === "hidden" ? settling : moving || (mascot && this.engine.busy && (this.botSeen || !this.rest));
  }

  private updateBotTargets(greeting: boolean, nowMs: number) {
    const expanded = State.mode === "expanded";
    const p = botPosition(State.mode, State.view, this.target.h, State.uploadProgress);
    // On the strip the mascot belongs to the pills; the greeting and the drop
    // canvas draw their own.
    const onStrip = State.mode !== "compact" || this.strip.showsPills;
    // The reader has the whole view: the mascot steps out while it is read.
    const visible = p.opacity > 0 && onStrip && !greeting && !this.uploadActive && !this.reader.classList.contains("on");

    // A view without a mascot: it fades out where it is rather than shrink to nothing.
    let x = p.cx - (expanded ? EXPANDED_W : COMPACT_W) / 2;
    let y = p.cy;
    let d = p.diameter;
    // Its card grew under the pointer: it grows with it, about the card's centre like everything in the card.
    const wasGrown = this.grown != null;
    const card = expanded && visible ? this.grownCard(x, y) : (this.grown = null);
    if (card) [x, y, d] = [scaleFrom(x, card.x, HOVER_GROW), scaleFrom(y, card.y, HOVER_GROW), d * HOVER_GROW];
    const withCard = (card != null) !== wasGrown;
    const t = this.botTarget;
    if (p.diameter > 0 && (x !== t.x || y !== t.y || d !== t.d)) {
      this.botTarget = { x, y, d };
      // On screen before and after, it travels there: with the shell's spring
      // when the island opens or moves on, on its curve when it folds (§7), and
      // with its card in the card's own time and curve (at once when motion is reduced).
      // Not on screen before, it is simply there when it shows.
      const go = (value: Tracked, to: number) => {
        if (!this.botSeen || (withCard && REDUCED?.matches)) value.jump(to);
        else if (withCard) value.curveTowards(to, HOVER_GROW_MS, nowMs, easeOut);
        else if (expanded) value.springTo(to);
        else value.curveTowards(to, undefined, nowMs);
      };
      go(this.botXc, x);
      go(this.botCy, y);
      go(this.botSize, d);
      // One small sign that it noticed: it perks up as its card grows, not again for a pointer that only wanders.
      if (withCard && card && this.botSeen && State.settings.showMascot && !REDUCED?.matches && nowMs - this.perkedAt > 3000) {
        this.perkedAt = nowMs;
        this.engine.triggerEmote("surprised", 0.5);
      }
    }

    const mascot = State.settings.showMascot;
    if (visible !== this.botSeen) {
      // New with the island that has just opened, it comes in with the body.
      const arriving = visible && expanded && nowMs - this.openedAt < 100;
      this.botCanvas.classList.toggle("arriving", arriving);
      this.botDot.classList.toggle("arriving", arriving);
      this.botSeen = visible;
    }
    this.mood.watch(visible && mascot);
    this.botCanvas.style.opacity = visible && mascot ? "1" : "0";
    this.botDot.style.opacity = visible && !mascot ? "1" : "0";
    if (!mascot) setDotMark(this.botDot, this.bodyColor() ?? "", this.worn.voice?.face ?? this.worn.held ?? State.effectiveState);
  }

  /**
   * The centre of the card the mascot sits in (its own place is `x`, `y`), while that card is grown
   * under the pointer (Settings › Island "Enlarge cards on hover"); null otherwise. All from the
   * island's centre line and top edge. Asked of the page's own `:hover`, so it is the card's growth
   * and nothing else. A card grows about its centre, so the centre is the same grown or not.
   */
  private grownCard(x: number, y: number): { card: Element; x: number; y: number } | null {
    if (!State.settings.hoverEnlarge) return (this.grown = null);
    for (const card of this.viewsEl.querySelectorAll(`:scope > .view.on :is(${GROWS}):hover`)) {
      // Measured once, as it begins to grow: asked again on every frame of the growth, the centre
      // would come back a hair off each time and the mascot would start over for each.
      if (card === this.grown?.card) return this.grown;
      const c = card.getBoundingClientRect();
      const i = this.islandEl.getBoundingClientRect();
      const [left, top] = [i.left + i.width / 2, i.top];
      if (left + x >= c.left && left + x <= c.right && top + y >= c.top && top + y <= c.bottom) {
        return (this.grown = { card, x: c.left + c.width / 2 - left, y: c.top + c.height / 2 - top });
      }
    }
    return (this.grown = null);
  }

  /**
   * Puts the mascot where it is this frame. Moved by transform, to whole device
   * pixels: the canvas is never drawn between two, and what is left over is
   * given to the drawing itself.
   */
  private placeBot() {
    const dpr = window.devicePixelRatio || 1;
    const snap = (v: number) => Math.round(v * dpr) / dpr;
    const [left, top] = [snap(this.botXc.value - BOT_BOX / 2), snap(this.botCy.value - BOT_BOX / 2)];
    this.botInCanvas = { x: this.botXc.value - left, y: this.botCy.value - top };
    this.botCanvas.style.transform = `translate(${left}px, ${top}px)`;
    this.botDot.style.transform = `translate(${snap(this.botXc.value - 5)}px, ${snap(this.botCy.value - 5)}px)`;
  }

  /** The mascot's centre inside its canvas. */
  private botInCanvas = { x: BOT_BOX / 2, y: BOT_BOX / 2 };

  /** The colour the mascot wears: its pill's, whatever card is open beside it. */
  private bodyColor(): string | null {
    return State.mascotColor;
  }

  private drawBot(dt: number) {
    const d = this.botSize.value;
    // A spring may dip under its target: never hand the engine a size it cannot draw.
    if (d < 1) return;
    // One size for good: the canvas is never laid out again, the mascot is drawn at the size it has.
    const dpr = Math.min(2, window.devicePixelRatio || 1);
    const px = Math.round(BOT_BOX * dpr);
    if (this.botCanvas.width !== px) this.botCanvas.width = this.botCanvas.height = px;
    const ctx = this.botCanvas.getContext("2d");
    if (!ctx) return;

    const color = this.bodyColor();
    this.engine.bodyColor = color ? hexToRGB(color) : null;
    this.engine.lookX = this.worn.look?.x ?? this.lookX();
    this.engine.lookY = this.worn.look?.y ?? this.lookY();
    this.engine.setDancing(this.dances());

    this.engine.update(dt);
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    wipe(ctx);
    this.engine.draw(ctx, this.botInCanvas.x, this.botInCanvas.y, d);
  }

  /** The mascot dances on the strip while music plays (`audio-extras:beat` says so). Asked every frame. */
  private dances(): boolean {
    return this.songPlays && State.mode === "compact" && DANCE_STATES.has(State.effectiveState);
  }

  /** Where the mascot looks: tanh of the distance to the pointer. lookY is positive upward. */
  private lookX(): number {
    const rect = this.islandRect();
    return Math.tanh((State.mouse.x - (rect.x + rect.w / 2 + this.botXc.value)) / 260);
  }

  private lookY(): number {
    return -Math.tanh((State.mouse.y - this.botCy.value) / 200);
  }

  private updateCountdown(nowMs: number) {
    // The state machine's own deadline, so the bar follows an auto-close delay
    // edited while the countdown runs.
    const dueAt = this.fsm.openCollapseDueAt;
    // A CSS animation armed once per deadline: it runs on the compositor, so
    // the bar moves with a still mouse and costs the loop no frames.
    const armed = State.mode === "expanded" && !State.isPinned && dueAt != null;
    if (!armed) {
      if (this.cdDue != null) { this.cdDue = null; this.countdown.style.animation = ""; }
      return;
    }
    if (this.cdDue === dueAt) return;
    this.cdDue = dueAt;
    const windowS = Math.min(10, this.fsm.openToPeekDelay * 0.6);
    const remaining = (dueAt! - nowMs) / 1000;
    this.countdown.style.animation = "none";
    void this.countdown.offsetWidth;
    this.countdown.style.animation = `island-cd ${windowS}s linear ${(remaining - windowS).toFixed(3)}s forwards`;
  }

  // ── DOM sync ────────────────────────────────────────────────────────────────

  /** The live activity that owns the strip and the ones riding beside it (§5.2), the agents among them. */
  private pickStrip() {
    const all = featureOn("agents") ? [...activities(), this.agents] : activities();
    return pickActivities(all.map((a) => ({ a, order: a.def.order, isActive: () => isActive(a) })));
  }

  /** What the strip shows (§5): a card that waits beats a notice beats an activity beats the pills. */
  private stripContent(): StripContent {
    if (State.pendingApproval) return { kind: "pills" };
    if (this.arbiter.notice) return { kind: "notice", shown: this.arbiter.notice };
    const { primary, companions } = this.pickStrip();
    // The agents' strip is the pills; with nothing live the pills are what there is.
    if (!primary || primary.a === this.agents) return { kind: "pills" };
    return {
      kind: "activity", activity: primary.a, companions: companions.map((c) => c.a), rev: this.activityRev,
      width: stripWidth(primary.a.feature, this.shapeAsk?.width),
    };
  }

  private syncDom() {
    const expanded = State.mode === "expanded";
    const greeting = this.fsm.state === "greet";
    // Settings › Island "The small island stays", and whether an agent is at work now.
    this.fsm.setStay(State.settings.peekStay, this.agentsBusy());

    // The home view follows the pills: the overview while there are any, the
    // empty card when the features that bring them are all off.
    const home = State.tasks.length === 0 ? "empty" : "overview";
    if ((State.view === "empty" || State.view === "overview") && State.view !== home) {
      State.view = home;
      if (expanded) this.animateGeometry();
    }

    this.islandEl.classList.toggle("greeting", greeting);
    this.islandEl.classList.toggle("outlined", State.settings.islandOutline);
    this.islandEl.classList.toggle("no-pct", !State.settings.levelPercent);
    this.islandEl.classList.toggle("hover-grow", State.settings.hoverEnlarge);
    this.foldBtn.style.display = State.pendingApproval ? "none" : "";
    // While the drop sequence owns the body its buttons are painted on the canvas
    // underneath, so only the header may keep taking clicks up here.
    this.contentEl.style.pointerEvents = expanded && !this.uploadActive ? "auto" : "none";
    this.header.el.style.pointerEvents = expanded ? "auto" : "none";

    this.header.sync();
    for (const [name, view] of this.views) {
      const on = name === State.view;
      view.el.classList.toggle("on", on);
      if (on) view.sync();
    }
    // A view whose height follows what it shows: the session chips, an answer that scrolls.
    if (expanded && (State.view === "overview" || State.view === "finished") && this.targetSize().h !== this.target.h) {
      this.animateGeometry();
    }
    // The one place sections learn they are on or off screen (§5.3).
    const section = expanded && State.view === "section" ? this.sections.active : null;
    this.sections.sync(section);
    if (section && section !== HOME_KEY && section !== this.lastTool) {
      this.lastTool = section;
      try {
        localStorage.setItem(LAST_TOOL, section);
      } catch {
        // Remembered for this run only.
      }
    }

    const content = this.stripContent();
    this.strip.sync(content, State.mode !== "hidden");
    this.syncFold();

    // The island's mode as the page's rules see it, written once both ends have
    // their content: what they share starts its journey in this same frame (§4).
    const mode = State.mode === "hidden" ? "hidden" : expanded ? "open" : "strip";
    const was = this.islandEl.dataset.mode ?? "hidden";
    const view = this.viewsEl.querySelector<HTMLElement>(":scope > .view.on");
    if (mode !== was || this.islandEl.dataset.mode == null) {
      const row = this.strip.el.lastElementChild;
      const now = performance.now();
      const half = pixelWidth(this.width.value) / 2;
      if (was === "strip" && mode === "open") {
        this.openedAt = now;
        this.travel.start(this.islandEl, row, view, half, true, now, BODY_IN_MS);
      } else if (was === "open" && mode === "strip") {
        this.travel.start(this.islandEl, this.openView, row, half, false, now, STRIP_IN_MS);
      } else {
        if (mode === "open") this.openedAt = now;
        this.travel.finish();
      }
      this.islandEl.dataset.from = was;
      this.islandEl.dataset.mode = mode;
      for (const fn of this.modeWatchers) fn();
    }
    if (expanded) this.openView = view;
    if (State.mode === "compact" && this.strip.width !== this.target.w) this.animateGeometry();
    this.syncTick(content);
    this.syncClock(content);

    // A view with a text field is when the island takes the keyboard without
    // being asked (the chat, the gallery's filter, a section that says so), and
    // never while it is only open because the pointer rested on it.
    const def = section ? this.sections.find(section)?.def : undefined;
    const typing = !expanded || this.fsm.openedByHover
      ? null
      : State.view === "prompt" || State.view === "gallery" ? State.view
      : def?.keyboard ? section
      : null;
    if (typing !== this.lastTyping) {
      this.lastTyping = typing;
      if (typing) {
        // Reached by a click the keyboard is here already; reached by a key or a
        // shortcut it is taken now (§4.1).
        this.takeKeyboard();
        const view = this.views.get(State.view);
        window.setTimeout(() => view?.focus?.(), 120);
      }
    }

    // The keys Rust reads for the island while the pointer is over it (§4).
    const keys = this.islandKeys();
    if (keys.join() !== this.pushedKeys) {
      this.pushedKeys = keys.join();
      void Bridge.setIslandKeys(keys);
    }

    // A press outside folds the open island: Rust listens for one only then.
    const watch = expanded && !State.pendingApproval && !State.isPinned && !State.paused;
    if (watch !== this.pushedWatch) {
      this.pushedWatch = watch;
      void Bridge.setOutsideWatch(watch);
    }

    if (State.settings.showMascot) syncMiniBotStates(State.tasks);
    this.engine.setState(this.mood.apply(State.effectiveState));
    this.botCanvas.title = this.mood.tip;
    if (this.voiceFace) this.wear();
    this.tray.sync();
  }

  /** The strip's clock is drawn again on the minute, and only while it is on screen. */
  private syncClock(content: StripContent) {
    const want = State.settings.stripClock && State.mode === "compact" && content.kind === "pills";
    if (want && this.clockTimer == null) {
      this.clockTimer = window.setTimeout(() => {
        this.clockTimer = null;
        this.invalidate();
      }, 60_000 - (Date.now() % 60_000) + 30);
    } else if (!want && this.clockTimer != null) {
      window.clearTimeout(this.clockTimer);
      this.clockTimer = null;
    }
  }

  /** An activity with a running clock is drawn again every second, while it is on the strip. */
  private syncTick(content: StripContent) {
    const want = State.mode === "compact" && content.kind === "activity" && content.activity.def.tick === true;
    if (want && this.tickTimer == null) {
      // The row was last drawn when the strip was last up: fresh at once, not a second from now.
      this.activityRev++;
      this.invalidate();
      this.tickTimer = window.setInterval(() => {
        this.activityRev++;
        this.invalidate();
      }, 1000);
    } else if (!want && this.tickTimer != null) {
      window.clearInterval(this.tickTimer);
      this.tickTimer = null;
    }
  }

  /** Applies settings coming from Rust at boot, and whenever they change. */
  applySettings() {
    Sound.setEnabled(State.settings.soundEnabled);
    Sound.setVolume(State.settings.soundVolume);
    this.fsm.openToPeekDelay = State.settings.autoCloseInterval;
    this.fsm.openOnHover = State.settings.openOnHover;
    this.syncActivityHold();
    // Dragged somewhere, or put back (here, in the tray or in Settings).
    if (JSON.stringify(State.settings.islandSpot ?? null) !== this.placed) void this.place();
    // The mini mascots of a mascot that was just hidden stop being ticked.
    if (!State.settings.showMascot) pruneMiniBots();
    State.notify();
  }

  get panelSize() {
    return { w: PANEL_W, h: PANEL_H };
  }

  get chatHeight() {
    return chatPromptHeight(State.chatHistory.length);
  }
}
