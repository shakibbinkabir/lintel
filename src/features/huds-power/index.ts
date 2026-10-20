// Levels and battery — the page side of the feature (docs/ARCHITECTURE.md §2.3).
// Nothing runs at import: declare surfaces here, begin work in `start`.
//
// Rust tells of changes (crates/huds-power); this turns them into notices: the
// volume, the microphone, the brightness, the charger and the battery. It also
// owns the Battery section, the strip resting on the charge, and the word to
// the island that a full-screen app is in front.

import "./style.css";
import { defineFeature, type FeatureCtx, type Notice, type SectionDef, type SectionHost } from "../../core/feature";
import { ORDER } from "../../island/arbiter";
import { btn, card, row, stripRow, toggle, type Tone } from "../../ui/components";
import { h } from "../../views/dom";

// ── What Rust sends ───────────────────────────────────────────────────────────

interface Level {
  level: number;
  mute: boolean;
  /** News for our state, not for the user: a device switch and the level the new device says right after it, our own write. */
  quiet: boolean;
}

interface Power {
  ac: boolean;
  /** Null on a PC without a battery. */
  percent: number | null;
  charging: boolean;
  saver: boolean;
}

interface Battery extends Power {
  /** At the present rate: to full on the charger, to empty on battery. */
  minutes: number | null;
  /** Share of its design capacity the battery still holds, 0–100. */
  health: number | null;
}

const DEFAULTS = { volume: true, mic: true, brightness: true, battery: true, restOnBattery: false, fullscreen: true, micUse: true, quietMic: false, quietCamera: false, chargeLimit: 0 };
type Options = typeof DEFAULTS;

/** At or below this charge, off the charger, the battery is low. */
const LOW = 20;
/** How long a level stays on the strip (§5.1). */
const LEVEL_MS = 1600;
/** A notice held back longer than this is old news. */
const HELD_MS = 30_000;
/** How long the strip gets to draw the microphone row before the app is announced by a notice instead. */
const MIC_WAIT_MS = 400;

// Icons only this feature draws (src/ui/icons.ts has the shared set).
const VOLUME_LOW = "M4 9.5h3.5L12 6v12l-4.5-3.5H4zM15.5 9a4.2 4.2 0 0 1 0 6";
const MIC_OFF = "M12 4a3 3 0 0 0-3 3v4a3 3 0 0 0 6 0V7a3 3 0 0 0-3-3zM6 11a6 6 0 0 0 12 0M12 17v3.5M5 4.5l14 15";

// ── State (set in `start`, cleared when the feature stops) ────────────────────

let opts: Options = DEFAULTS;
let power: Power | null = null;
let micMute: boolean | null = null;
let fullscreen = false;
/** The apps with the microphone open now (never Lintel's own dictation). */
let micApps: string[] = [];
/** The apps with the camera open now. */
let camApps: string[] = [];
/** The island was told to keep quiet because the microphone or the camera is in use. */
let calling = false;
/** The strip drew the microphone row (its `render` ran): the row owns the strip, no notice is needed. */
let micDrawn = false;
let micTimer = 0;
/** A battery notice waiting for the full-screen app to leave. */
let held: { notice: Notice; at: number } | null = null;
/** When the level now on the strip is gone: a battery notice waits for it. */
let levelUntil = 0;
let timer = 0;
/** Reads the battery again while its section is on screen. */
let redraw: (() => void) | null = null;
/** `cx.t`, for the activity's `render`, which is not handed a context. */
let featureT: FeatureCtx["t"] = (key) => key;

/** The pulsing dot of the microphone strip. */
const micDot = () => h("i", { class: "f-huds-power-mic" });

const hasBattery = () => power?.percent != null;

// ── The battery glyph: the mark of every battery row ──────────────────────────

function tone(p: Power): Tone | undefined {
  if (!p.ac && (p.percent ?? 100) <= LOW) return "bad";
  if (p.saver) return "warn";
  return p.ac ? "ok" : undefined;
}

function glyph(p: Power, large = false): HTMLElement {
  const el = h("span", { class: ["f-huds-power-batt", large && "large", tone(p)].filter(Boolean).join(" ") }, h("i"));
  el.style.setProperty("--v", String((p.percent ?? 0) / 100));
  return el;
}

const percent = (cx: FeatureCtx, value: number) => cx.t("{percent}%", { percent: Math.round(value) });

function statusTitle(cx: FeatureCtx, p: Power): string {
  if (p.charging) return cx.t("Charging");
  if (!p.ac) return cx.t("On battery");
  return (p.percent ?? 0) >= 100 ? cx.t("Fully charged") : cx.t("Plugged in, not charging");
}

// ── Notices ───────────────────────────────────────────────────────────────────

/**
 * A level is feedback for a key just pressed: over a full-screen app it is
 * dropped. A battery notice waits, for that app to leave and for a level of
 * ours that is on the strip (it would be refused behind it, and lost).
 */
function post(cx: FeatureCtx, notice: Notice) {
  if (fullscreen) {
    if (notice.level == null) held = { notice, at: performance.now() };
    return;
  }
  if (notice.level != null) {
    levelUntil = performance.now() + LEVEL_MS;
    cx.island.notice(notice);
    return;
  }
  const wait = levelUntil - performance.now();
  window.clearTimeout(timer);
  if (wait > 0) timer = window.setTimeout(() => post(cx, notice), wait + 60);
  else cx.island.notice(notice);
}

function onVolume(cx: FeatureCtx, v: Level) {
  if (v.quiet || !opts.volume) return;
  const muted = v.mute || v.level < 0.005;
  post(cx, {
    kind: "volume",
    title: cx.t(muted ? "Muted" : "Volume"),
    level: muted ? 0 : v.level,
    icon: muted ? "mute" : v.level < 0.34 ? VOLUME_LOW : "volume",
  });
}

/** Only the mute switch is news: apps move the microphone's level on their own all day. */
function onMic(cx: FeatureCtx, v: Level) {
  const flipped = micMute != null && v.mute !== micMute;
  micMute = v.mute;
  if (!flipped || v.quiet || !opts.mic) return;
  post(cx, {
    kind: "mic",
    title: cx.t(v.mute ? "Microphone muted" : "Microphone on"),
    level: v.mute ? 0 : v.level,
    icon: v.mute ? MIC_OFF : "mic",
  });
}

function onPower(cx: FeatureCtx, now: Power) {
  const was = power;
  power = now;
  redraw?.();
  // The first reading is where we start, not news; it also brings the section and the resting strip.
  if (!was) cx.island.activityChanged("battery");
  if (!was || was.percent == null || now.percent == null || !opts.battery) return;

  let title: string | null = null;
  if (!now.ac && was.percent > LOW && now.percent <= LOW) title = cx.t("Battery low");
  else if (now.ac !== was.ac) title = now.ac ? (now.charging ? cx.t("Charging") : cx.t("Plugged in")) : cx.t("On battery");
  else if (now.ac && was.percent < 100 && now.percent >= 100) title = cx.t("Fully charged");
  else if (now.saver !== was.saver) title = cx.t(now.saver ? "Battery saver on" : "Battery saver off");
  if (!title) return;
  post(cx, { kind: "battery", title, detail: percent(cx, now.percent), mark: glyph(now), section: "battery" });
}

/** The microphone or camera is in use and its switch is on: no sounds, no reveals, notices wait. */
function syncQuiet(cx: FeatureCtx) {
  const call = (opts.quietMic && micApps.length > 0) || (opts.quietCamera && camApps.length > 0);
  if (call === calling) return;
  calling = call;
  cx.island.fullscreen(fullscreen || call, call);
}

function onMicUse(cx: FeatureCtx, apps: string[], camera: string[] = []) {
  camApps = camera;
  const fresh = apps.find((a) => !micApps.includes(a));
  const was = micApps.length;
  micApps = apps;
  syncQuiet(cx);
  // A new app on the strip comes out; a change of names only redraws the row.
  if (was === 0 || apps.length === 0) cx.island.activityChanged("mic-use");
  else cx.island.redraw("mic-use");
  window.clearTimeout(micTimer);
  if (!fresh) return;
  // The row waits behind a timer or a recording (§5.2), and is then never
  // drawn: the app is announced by a notice instead, which nothing live covers.
  micDrawn = false;
  micTimer = window.setTimeout(() => {
    if (micDrawn || fullscreen || !opts.micUse || !micApps.includes(fresh)) return;
    cx.island.notice({ kind: "mic", title: fresh, detail: cx.t("Microphone in use"), mark: micDot() });
  }, MIC_WAIT_MS);
}

/** The battery reached the limit the user set, on the charger: one notice per plug-in session. */
function onChargeLimit(cx: FeatureCtx, pct: number) {
  const p: Power = { ac: true, percent: pct, charging: true, saver: false };
  post(cx, { kind: "battery", title: cx.t("Charge limit reached"), detail: percent(cx, pct), mark: glyph(p), section: "battery" });
}

/** A full-screen app came to the front, or left: the island is told, and what waited for it shows. */
function setFullscreen(cx: FeatureCtx, on: boolean) {
  if (on === fullscreen) return;
  fullscreen = on;
  cx.island.fullscreen(on || calling, calling);
  if (micApps.length > 0) cx.island.activityChanged("mic-use");
  const waiting = held;
  held = null;
  if (!on && waiting && performance.now() - waiting.at < HELD_MS) post(cx, waiting.notice);
}

// ── The Battery section ───────────────────────────────────────────────────────

function duration(cx: FeatureCtx, minutes: number): string {
  const hours = Math.floor(minutes / 60);
  const rest = minutes % 60;
  return hours > 0 ? cx.t("{hours} h {minutes} min", { hours, minutes: rest }) : cx.t("{minutes} min", { minutes: rest });
}

function fact(label: string, value: string): HTMLElement {
  return card([h("span", { text: label }), h("b", { text: value })]);
}

function buildBattery(cx: FeatureCtx): SectionHost {
  const el = h("div", { class: "f-huds-power-page" });
  let on = false;

  const draw = (b: Battery) => {
    if (b.percent == null) return;
    const time =
      b.minutes == null ? null
      : b.ac ? cx.t("About {time} until full", { time: duration(cx, b.minutes) })
      : cx.t("About {time} left", { time: duration(cx, b.minutes) });
    el.replaceChildren(
      h(
        "div",
        { class: "f-huds-power-head" },
        glyph(b, true),
        h("b", { class: "f-huds-power-pct", text: percent(cx, b.percent) }),
        h("div", { class: "f-huds-power-status" }, h("span", { text: statusTitle(cx, b) }), time ? h("small", { text: time }) : null),
        h("span", { class: "isl-gap" }),
        btn(cx.t("Battery settings"), "secondary", () => void cx.call("batterySettings")),
      ),
      h(
        "div",
        { class: "f-huds-power-facts" },
        fact(cx.t("Power source"), cx.t(b.ac ? "Charger" : "Battery")),
        fact(cx.t("Battery health"), b.health == null ? cx.t("Not reported") : percent(cx, b.health)),
        fact(cx.t("Battery saver"), cx.t(b.saver ? "On" : "Off")),
      ),
    );
  };
  const load = () => void cx.call<Battery>("battery").then((b) => on && b && draw(b));

  return {
    el,
    show() {
      on = true;
      redraw = load;
      load();
    },
    hide() {
      on = false;
      redraw = null;
    },
    sync() {},
  };
}

const LIMITS = [0, 60, 65, 70, 75, 80, 85, 90, 95, 100];

const batterySection: SectionDef = { id: "battery", title: "Battery", icon: "battery", letter: "b", height: 184, build: buildBattery };

// ── Settings ──────────────────────────────────────────────────────────────────

/** A native select: off, or 60 to 100 % in steps of 5 (80 is the usual). */
function limitSelect(cx: FeatureCtx, value: number): HTMLElement {
  const sel = h("select");
  for (const v of LIMITS) sel.append(h("option", { value: String(v), text: v === 0 ? cx.t("Off") : percent(cx, v) }));
  sel.value = String(LIMITS.includes(value) ? value : 0);
  sel.addEventListener("change", () => void cx.settings.patch({ chargeLimit: Number(sel.value) }));
  return h("label", { class: "field" }, sel);
}

function settingsPage(cx: FeatureCtx): HTMLElement {
  const el = h("div", { class: "f-huds-power-settings" });
  void cx.settings.get().then((stored) => {
    const now = { ...DEFAULTS, ...stored } as Options;
    const item = (key: "volume" | "mic" | "brightness" | "battery" | "restOnBattery" | "fullscreen" | "micUse" | "quietMic" | "quietCamera", label: string, hint: string) =>
      row(cx.t(label), toggle(now[key], (on) => void cx.settings.patch({ [key]: on })), cx.t(hint));
    el.append(
      h("div", { class: "section-title", text: cx.t("Notices") }),
      card([
        item("volume", "Volume", "The volume keys, a headset button or the Windows slider."),
        item("mic", "Microphone", "When the microphone is muted or unmuted."),
        item("brightness", "Brightness", "On a PC with a built-in screen."),
        item("battery", "Battery", "Plugging in, unplugging, a full charge and a low battery."),
      ]),
      h("div", { class: "section-title", text: cx.t("Privacy and charging") }),
      card([
        item("micUse", "Microphone in use", "A pulsing dot and the app's name while an app has the microphone open. Dictation here is not announced."),
        item("quietMic", "Quiet while the microphone is in use", "No sounds, no sliding out and notices wait while an app has the microphone open (a call). An app that holds it all day keeps the island quiet all day."),
        item("quietCamera", "Quiet while the camera is in use", "The same while an app has the camera open."),
        row(cx.t("Charge limit reminder"), limitSelect(cx, now.chargeLimit), cx.t("One notice per charging session when the battery reaches this level on the charger.")),
      ]),
      h("div", { class: "section-title", text: cx.t("Island") }),
      card([
        item("restOnBattery", "Rest on the battery level", "The strip shows the charge when nothing else is going on."),
        item("fullscreen", "Stay away from full-screen apps", "The island does not come out over a game or a video."),
      ]),
    );
  });
  return el;
}

// ── The feature ───────────────────────────────────────────────────────────────

export default defineFeature({
  id: "huds-power",
  voice: () => import("./voice"),

  // "mic-use" is a privacy indicator: before the agents, the calendar and music (`ORDER.micInUse`, 2.5).
  // What the strip rests on, after everything that is live (§5.2 ends at keep awake, 6).
  // `tick` keeps the reading fresh while it is looked at; a new percent is no reason to come out.
  // It is a reading and never the words of a battery notice ("Charging"): under those, on a strip that
  // stays, the notice seemed never to leave. The glyph's colour says that the charger is in.
  // No `section`: resting on the strip must open the island where it always opens.
  activities: [
    {
      id: "mic-use",
      order: ORDER.micInUse,
      // The dot is there for as long as the microphone is held (a 3 s reveal is not a privacy indicator),
      // but never over a full-screen app.
      hold: true,
      isActive: () => opts.micUse && micApps.length > 0 && !fullscreen,
      render: () => {
        micDrawn = true;
        return stripRow({
          mark: micDot(),
          title: micApps[0] ?? "",
          detail: featureT("Microphone in use"),
          value: micApps.length > 1 ? featureT("+{count}", { count: micApps.length - 1 }) : undefined,
        });
      },
      mark: micDot,
    },
    {
      id: "battery",
      order: 7,
      tick: true,
      isActive: () => opts.restOnBattery && hasBattery(),
      render: () => {
        const p = power!;
        return stripRow({
          mark: glyph(p),
          title: featureT("Battery"),
          value: featureT("{percent}%", { percent: p.percent ?? 0 }),
        });
      },
    },
  ],

  // A PC without a battery has no Battery section.
  get sections() {
    return hasBattery() ? [batterySection] : [];
  },

  settings: settingsPage,

  start: (cx) => {
    featureT = cx.t;
    const applyOptions = (stored: object) => {
      const rested = opts.restOnBattery;
      const micWas = opts.micUse;
      opts = { ...DEFAULTS, ...stored } as Options;
      if (opts.restOnBattery !== rested) cx.island.activityChanged("battery");
      if (opts.micUse !== micWas) cx.island.activityChanged("mic-use");
      syncQuiet(cx);
    };
    void cx.settings.get().then(applyOptions);
    const offs = [
      cx.settings.onChange(applyOptions),
      cx.on<Level>("volume", (v) => onVolume(cx, v)),
      cx.on<Level>("mic", (v) => onMic(cx, v)),
      cx.on<{ level: number; displays?: number }>("brightness", ({ level, displays }) => {
        // With more than one display this is the built-in panel: say so.
        const title = cx.t((displays ?? 1) > 1 ? "Built-in display" : "Brightness");
        if (opts.brightness) post(cx, { kind: "brightness", title, level, icon: "sun" });
      }),
      cx.on<{ apps: string[]; camera?: string[] }>("micUse", ({ apps, camera }) => onMicUse(cx, apps, camera)),
      cx.on<{ percent: number }>("chargeLimit", ({ percent: pct }) => onChargeLimit(cx, pct)),
      cx.on<Power>("power", (p) => onPower(cx, p)),
      cx.on<{ on: boolean }>("fullscreen", ({ on }) => setFullscreen(cx, on)),
    ];
    // Events only tell of changes: this is where we start from.
    void cx.call<{ power: Power; micMute: boolean | null; fullscreen: boolean; micApps: string[] | null; cameraApps?: string[] | null }>("state").then((s) => {
      if (!s) return;
      if (s.micApps && !micApps.length) onMicUse(cx, s.micApps, s.cameraApps ?? []);
      micMute ??= s.micMute;
      if (!power) onPower(cx, s.power);
      setFullscreen(cx, s.fullscreen);
    });

    return () => {
      for (const off of offs) off();
      window.clearTimeout(timer);
      window.clearTimeout(micTimer);
      held = null;
      setFullscreen(cx, false);
      if (calling) cx.island.fullscreen(false, false);
      calling = false;
      camApps = [];
      power = null;
      micMute = null;
      micApps = [];
      redraw = null;
      opts = DEFAULTS;
    };
  },
});
