// The TypeScript half of a feature (docs/ARCHITECTURE.md §2.3).
//
// A feature is one object, the default export of src/features/<id>/index.ts,
// made with `defineFeature`. It only declares surfaces; nothing runs at import.
// `start` runs once the feature is on and the island page has booted, and
// everything a feature needs from the app comes through its `FeatureCtx`.
// src/core/registry.ts discovers the modules and runs them.

import type { t } from "../i18n/i18n";
import type { PillDefinition } from "./pills";

/** A feature's settings: its Rust manifest defaults with what the user changed on top. */
export type Value = Record<string, unknown>;

// ── Island surfaces (docs/ARCHITECTURE.md §5; the island shell consumes these) ─

/** A live activity: what the compact strip shows while something of the feature runs. */
export interface ActivityDef {
  id: string;
  /**
   * The fixed order across features, lower first: timer 0, watch 1,
   * agents 3, calendar countdown 4, music 5, keep awake 6. The first active one
   * owns the strip.
   */
  order: number;
  isActive(): boolean;
  /**
   * The strip row: build it with `stripRow()` from src/ui/components.ts. Run
   * again on every `activityChanged()`, and once a second while `tick` is set.
   */
  render(): HTMLElement;
  /** A 22 px mark for when this activity rides beside a timer or a countdown (§5.2 pairs). */
  mark?(): HTMLElement;
  /** The section of this feature a click (or resting the pointer) on the strip opens. */
  section?: string;
  /** A click on the strip, instead of opening `section`. */
  onClick?(): void;
  /** Draw again every second while on screen (a running clock). */
  tick?: boolean;
  /** Keeps the strip on screen for as long as the activity is active. */
  hold?: boolean;
}

/** What a section may be asked to do. "back" is Esc: answer true when it was used. */
export type SectionCommand = "back";

/** What a section hands the island: its element and the moments it is told about. */
export interface SectionHost {
  el: HTMLElement;
  /** On screen from now on: start sampling, take the text cursor. */
  show(): void;
  /** Off screen, on every path out (tab switch, fold, Esc, feature off): stop everything. */
  hide(): void;
  /** State changed while the section is on screen (the island's dirty-flag frame). */
  sync(): void;
  command?(command: SectionCommand): boolean | void;
}

/** A page of the expanded island. */
export interface SectionDef {
  id: string;
  /** English; shown through `t()`. */
  title: string;
  /** An icon name of src/ui/icons.ts, or path data for an icon of the feature's own. */
  icon: string;
  /** Alt+letter opens it while the island has the keyboard. */
  letter?: string;
  /** Height of the open island on this section, 160–320 logical pixels (content = height − 56). */
  height: number;
  /** The section has a text field: the island takes the keyboard when it is shown by a click or a key (never by a hover). */
  keyboard?: boolean;
  /**
   * Keys the section acts on that the island reads for it while the pointer is
   * over it without the keyboard (docs/ARCHITECTURE.md §4): "ArrowUp", "Enter",
   * "Shift+Enter"… They no longer reach the window the user types in, so name
   * only what the section really uses; a letter or a digit alone is never taken.
   * The section still hears them as `keydown` on `window`.
   */
  keys?: string[];
  /** Called once, the first time the section is shown. */
  build(cx: FeatureCtx): SectionHost;
}

/** Decides a notice's priority and how long it stays (§5.1). */
export type NoticeKind =
  | "volume" | "brightness" | "mic"
  | "capture" | "timer" | "watch"
  | "battery" | "notification" | "accessory" | "agent"
  | "clipboard" | "download" | "track";

/** A short message in the compact strip (HUD). One is on screen at a time. */
export interface Notice {
  kind: NoticeKind;
  title: string;
  detail?: string;
  /** 0…1, drawn as a meter with its number. With the island open it shows in the header. */
  level?: number;
  /** An icon name of src/ui/icons.ts (or path data). */
  icon?: string;
  /** Tints the icon: "ok" | "warn" | "bad" | "info". */
  tone?: "ok" | "warn" | "bad" | "info";
  /** A 22 px mark instead of the icon (album art, an app icon). */
  mark?: HTMLElement;
  /** A small button at the right end. */
  action?: { label: string; run(): void };
  /** The playing bars at the right end ("paused" holds them still). */
  bars?: boolean | "paused";
  /** The section of this feature a click on the notice opens. */
  section?: string;
}

/** What a feature may do to the island. Inert in any window but the island's. */
export interface IslandApi {
  /** Shows a notice and slides the island out for as long as its kind lasts. */
  notice(notice: Notice): void;
  /** Slides the compact strip out for `ms` (3 s when omitted) if the island is hidden. */
  reveal(ms?: number): void;
  /**
   * Opens the island on one of the feature's sections, by its id. Refused while
   * a card waits for an answer. `dragged` says something is being dragged toward
   * the island and the section wants to take it: refused as well while the Drop
   * tab is on screen, which takes a dropped file itself.
   */
  open(section: string, opts?: { dragged?: boolean }): void;
  /**
   * One of the feature's activities started, ended or has news: by its id. The
   * strip slides out for it only when that activity is the one the strip shows.
   */
  activityChanged(activity: string): void;
  /** Draws an activity's strip row again where it stands; the island does not slide out. */
  redraw(activity: string): void;
  /** A full-screen app came to the front (or left): the island stays away and notices wait (§4). `call`: the reason is the microphone or camera in use, so sounds stop too. */
  fullscreen(on: boolean, call?: boolean): void;
  /** Keeps the island from folding until the returned function is called. */
  hold(): () => void;
  /** What the island shows right now. */
  mode(): "hidden" | "strip" | "open";
  /** `fn` is called whenever `mode()` changed; the returned function ends it. */
  onMode(fn: () => void): () => void;
  /** This feature's section is a tab of the header right now: a tile that only jumps to it would say the same thing twice. */
  inHeader(section: string): boolean;
  /** Folds the open island to its strip. Refused while a card waits for an answer. */
  close(): void;

  // The voice assistant's three (docs/VOICE.md §7.5, §7.6). They are the `voice` feature's only: for
  // any other feature, and while voice is off, each does nothing. What they set goes when voice stops.

  /**
   * The microphone indicator: a 28 × 3 px bar of light at the top centre of the screen, on from
   * `ear(true)` to `ear(false)`, so on exactly while the microphone is open. `heard` makes it bright
   * for 0.3 s (a voice is heard). It is drawn over whatever the island shows and is never a target of
   * the pointer: the wake strip, the click-through and the keys are what they were without it. No frame
   * is drawn for it.
   */
  ear(on: boolean, heard?: boolean): void;
  /**
   * The island's box right now. With `ask`, also what the voice row wants of it: the width of the
   * strip while the feature's own activity has it, and to be told when the box changes (the capsule
   * goes 6 px under its lower edge). An ask replaces the one before; `null` withdraws it; no argument
   * only reads.
   */
  shape(ask?: ShapeAsk | null): IslandShape;
  /**
   * The face of the island's own mascot during a turn, with a level 0…1 for `listening` (the
   * microphone) and `talking` (what is played); `null` gives the mascot back. `wink` and `surprised`
   * play once and are not kept. While a card waits for an answer the mascot keeps the card's face.
   */
  face(face: VoiceFace | null, level?: number): void;
}

/** The faces of a voice turn (docs/VOICE.md §7.6). `asking` is the question face. */
export type VoiceFace = "listening" | "thinking" | "talking" | "asking" | "wink" | "surprised";

/**
 * The island's box, in logical pixels: `width` × `height` is the size it is heading for (0 high
 * when hidden), centred in its window, its top edge `top` under the window's. The capsule's place
 * is 6 px under `top + height`.
 */
export interface IslandShape {
  mode: "hidden" | "strip" | "open";
  width: number;
  height: number;
  top: number;
}

/** What the voice row asks of the island's shape. */
export interface ShapeAsk {
  /** The strip's width while the voice activity owns it: 340 when left out, kept between 240 and 380. */
  width?: number;
  /** Called with the new box each time it changes (the island opens, folds, hides, grows). */
  changed?: (shape: IslandShape) => void;
}

// ── Command bar ───────────────────────────────────────────────────────────────

export interface CommandRow {
  id: string;
  title: string;
  subtitle?: string;
  icon?: string;
  /** Extra words the row is found by. */
  keywords?: string[];
  run(cx: FeatureCtx): void | Promise<void>;
}

/** The rows a feature offers for what is typed (consumed by the command-bar feature). */
export type CommandProvider = (query: string, cx: FeatureCtx) => CommandRow[] | Promise<CommandRow[]>;

// ── Voice actions (docs/VOICE.md §5; consumed by the voice feature) ──────────

/**
 * How far a spoken command goes by itself. `run`: it executes and the reply is spoken. `confirm`:
 * it asks first and runs on a spoken yes or a click. `prepare`: it opens the feature's own card,
 * filled in, and only that card's button completes it.
 */
export type VoiceSafety = "run" | "confirm" | "prepare";

/**
 * What was said for each argument, by its name: a number for `number`, `minutes` (may be a fraction:
 * "90 seconds" is 1.5) and `percent` (0…100); "HH:MM" on the 24-hour clock for `time`; the words as
 * they were said for `text`; the `id` of the choice for `choice`. An optional argument nobody said is
 * absent.
 */
export type VoiceArgs = Record<string, string | number>;

/** One thing a `choice` argument can name, and the ways it is said ("Speakers", "the speakers"). */
export interface VoiceChoice {
  id: string;
  say: string[];
}

export interface VoiceArg {
  /** The slot's name in the forms: `{minutes}`. */
  name: string;
  kind: "number" | "minutes" | "percent" | "time" | "text" | "choice";
  /** `choice` only: the list as it is now (sections, outputs, reminders…), asked each time it is needed. */
  choices?: (v: VoiceCx) => VoiceChoice[] | Promise<VoiceChoice[]>;
  optional?: boolean;
  /** The question for a required argument that was not said ("For how long?"). English, a key of strings.json. */
  ask?: string;
}

export interface VoiceAction {
  /** Unique within the feature: "start". */
  id: string;
  /** What the action model sees, unique across the app: "timer_start". */
  tool: string;
  /** One line for the model. English, never spoken. */
  describe: string;
  /**
   * What is said, the most usual first: "set a timer for {minutes}", "{minutes} timer". Words in
   * order, `[x]` optional, `(a|b)` alternatives, `{name}` an argument. English, never translated.
   */
  forms: string[];
  args?: VoiceArg[];
  safety: VoiceSafety;
  /** `confirm` only: the question ("Paste the last copy into Word?"). */
  confirm?: (a: VoiceArgs) => string;
  run(a: VoiceArgs, v: VoiceCx): Promise<VoiceReply | void>;
  /** Takes back what `run` did; offered for a few seconds after it. */
  undo?: (a: VoiceArgs, v: VoiceCx) => Promise<void>;
}

/** What an action answers. Nothing at all is a plain "done". */
export interface VoiceReply {
  /** Spoken, and shown when `show` is absent. */
  say?: string;
  /** The result on the strip ("Timer · 12 min"). */
  show?: string;
  /** One more thing is needed: its question is put and the reply runs the action again with it. */
  ask?: VoiceArg;
  /** False: it did not work (`say` tells why). */
  ok?: boolean;
}

/**
 * What an action gets instead of its feature's context: `call` reaches the action's own feature and
 * refuses what voice may never do (the deny list is the voice runner's), `open` opens one of its
 * sections.
 */
export interface VoiceCx {
  call: FeatureCtx["call"];
  t: FeatureCtx["t"];
  open(section: string): void;
}

/** A voice action with the feature that declared it. */
export interface VoiceEntry {
  feature: string;
  action: VoiceAction;
}

/** Types the default export of a feature's `voice.ts`. */
export function defineVoice(actions: VoiceAction[]): VoiceAction[] {
  return actions;
}

/**
 * Every voice action of the features `on` says are running, in the features' order. A feature that
 * is off contributes nothing and its `voice.ts` is not loaded; one that fails to load is logged and
 * skipped. `src/core/registry.ts` calls this with the real list (`voiceActions()`).
 */
export async function collectVoice(features: readonly FeatureModule[], on: (id: string) => boolean): Promise<VoiceEntry[]> {
  const lists = await Promise.all(
    features
      .filter((f) => f.voice && on(f.id))
      .map(async (f) => {
        try {
          return (await f.voice!()).default.map((action) => ({ feature: f.id, action }));
        } catch (err) {
          console.error(`[feature ${f.id}] voice actions failed to load`, err);
          return [];
        }
      }),
  );
  return lists.flat();
}

// ── The feature ───────────────────────────────────────────────────────────────

export interface FeatureModule {
  /** The same id as the Rust manifest. */
  id: string;
  /** Pill catalog entry (agent-style features only). */
  pill?: PillDefinition;
  /** Compact-strip live activities (§5). */
  activities?: ActivityDef[];
  /** Expanded-island sections (§5). */
  sections?: SectionDef[];
  /** Settings page body; the on/off switch is rendered by the host above it. */
  settings?: (cx: FeatureCtx) => HTMLElement;
  /** Floating window pages, mounted by window.html?feature=<id>&page=<name> (§6). */
  windows?: Record<string, (root: HTMLElement, cx: FeatureCtx) => void>;
  /** Command-bar rows (consumed by the command-bar feature). */
  commands?: CommandProvider;
  /** Global shortcut handlers by id, for shortcuts declared in the Rust manifest. */
  shortcuts?: Record<string, (cx: FeatureCtx) => void>;
  /**
   * What can be said to the feature (docs/VOICE.md §5). The actions live in `voice.ts` beside
   * `index.ts`, which ends with `export default defineVoice([…])` and is loaded only when the voice
   * assistant asks for it. The one line in `index.ts`:
   *
   *     voice: () => import("./voice"),
   */
  voice?: () => Promise<{ default: VoiceAction[] }>;
  /** Runs when the feature is on and the island page has booted; return a dispose fn. Nothing runs at import time. */
  start?: (cx: FeatureCtx) => (() => void) | void;
}

export interface FeatureCtx {
  id: string;
  /** `Feature::call` on the Rust side. Null when it failed (the error is logged). */
  call<T>(method: string, args?: object): Promise<T | null>;
  /** Listens to `<id>:<event>`; returns the unsubscribe. */
  on<T>(event: string, fn: (payload: T) => void): () => void;
  settings: {
    get(): Promise<Value>;
    /** Shallow merge, saved by Rust. */
    patch(patch: object): Promise<void>;
    onChange(fn: (value: Value) => void): () => void;
  };
  /** `notice`, `reveal`, `open`, `close`, `activityChanged`, `redraw`, `fullscreen`, `hold`: the feature's own sections and activities. */
  island: IslandApi;
  /** i18n, with the feature's strings.json merged in. */
  t: typeof t;
}

export function defineFeature(feature: FeatureModule): FeatureModule {
  return feature;
}
