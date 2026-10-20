// Timer — the page side of the feature (docs/ARCHITECTURE.md §2.3).
// Rust owns the timer; this file draws the strip, the section and the notices.

import "./style.css";
import { defineFeature, type FeatureCtx } from "../../core/feature";
import { Bridge } from "../../core/bridge";
import { Sound } from "../../core/sound";
import { h } from "../../views/dom";
import { t } from "../../i18n/i18n";
import { icon } from "../../ui/icons";
import { btn, card, meter, setMeter, slider, stripRow, toggle, row } from "../../ui/components";

interface Clock {
  mode: "timer" | "pomodoro" | "stopwatch";
  status: "idle" | "running" | "paused" | "waiting" | "finished";
  phase: "focus" | "short" | "long";
  totalMs: number;
  remainingMs: number;
  elapsedMs: number;
  laps: number[];
  session: number;
  sessions: number;
}

const IDLE: Clock = { mode: "timer", status: "idle", phase: "focus", totalMs: 0, remainingMs: 0, elapsedMs: 0, laps: [], session: 0, sessions: 4 };
let clock: Clock = IDLE;
let at = Date.now(); // when `clock` was received; running readings count from here
let sound = true;
let alarm: number | undefined;

// ── readings ─────────────────────────────────────────────────────────────────

const live = () => (clock.status === "running" ? Date.now() - at : 0);
const left = () => Math.max(0, clock.remainingMs - live());
const up = () => clock.elapsedMs + live();
/** Whole seconds shown: a countdown rounds up, a stopwatch down. */
const secs = () => (clock.mode === "stopwatch" ? Math.floor(up() / 1000) : Math.ceil(left() / 1000));
const pad = (n: number) => String(n).padStart(2, "0");
/** "12:04", "1:35:07". */
const full = (s: number) => (s >= 3600 ? `${Math.floor(s / 3600)}:${pad(Math.floor(s / 60) % 60)}:${pad(s % 60)}` : `${pad(Math.floor(s / 60))}:${pad(s % 60)}`);
/** The strip's shorter "12:04", "1h35". */
const short = (s: number) => (s >= 3600 ? `${Math.floor(s / 3600)}h${pad(Math.floor(s / 60) % 60)}` : full(s));

/** The power timer: sleep or shut down at `at` (ms). Rust counts; the page shows it. */
let power: { action: "sleep" | "shutdown"; at: number } | null = null;
const powerLeft = () => (power ? Math.max(0, Math.ceil((power.at - Date.now()) / 1000)) : 0);
const powerCall = (m: string, a?: object) => Bridge.featureCall("timers-downloads", m, a);
const powerWord = () => (power?.action === "shutdown" ? "Shut down" : "Sleep");

const timerOn = () => clock.status !== "idle";

export default defineFeature({
  id: "timers-downloads",
  voice: () => import("./voice"),

  activities: [
    {
      id: "timer",
      order: 0,
      tick: true,
      section: "timer",
      isActive: timerOn,
      render() {
        const done = clock.status === "finished" || clock.status === "waiting";
        const title =
          clock.status === "finished" ? "Time's up"
          : clock.status === "waiting" ? "Ready"
          : clock.mode === "stopwatch" ? "Stopwatch"
          : clock.mode === "pomodoro" ? (clock.phase === "focus" ? "Focus" : "Break")
          : "Timer";
        return stripRow({
          icon: "timer",
          tone: done ? "ok" : "warn",
          title: t(title),
          detail: clock.status === "paused" ? t("Paused") : undefined,
          value: done ? "00:00" : short(secs()),
        });
      },
      mark: () => h("span", { class: "isl-ico warn" }, icon("timer", 14)),
    },
    {
      id: "power",
      order: 7,
      tick: true,
      section: "timer",
      isActive: () => power != null,
      render: () => stripRow({ icon: "moon", tone: "warn", title: t(powerWord()), value: short(powerLeft()), action: { label: t("Cancel"), run: () => void powerCall("powerCancel") } }),
      mark: () => h("span", { class: "isl-ico warn" }, icon("moon", 14)),
    },
  ],

  sections: [
    {
      id: "timer",
      title: "Timer",
      icon: "timer",
      letter: "r",
      height: 280,
      build: (cx) => timerPage(cx),
    },
  ],

  settings(cx) {
    const box = card([]);
    cx.settings.get().then((s) => {
      box.append(
        row(cx.t("Alarm sound"), toggle(s.sound !== false, (on) => cx.settings.patch({ sound: on })), cx.t("Plays when a timer ends.")),
        row(cx.t("Start the next phase automatically"), toggle(s.auto === true, (on) => cx.settings.patch({ auto: on })), cx.t("Pomodoro: a break or focus begins when the last one ends.")),
      );
    });
    return box;
  },

  start(cx) {
    cx.settings.get().then((s) => (sound = s.sound !== false));
    const offS = cx.settings.onChange((s) => (sound = s.sound !== false));
    const changed = () => cx.island.activityChanged("timer");
    const setClock = (c: Clock) => {
      const was = timerOn();
      clock = c;
      at = Date.now();
      if (c.status !== "finished" && c.status !== "waiting") stopAlarm();
      if (was || timerOn()) changed();
    };
    cx.call<{ clock: Clock }>("state").then((r) => {
      if (!r) return;
      clock = r.clock;
      at = Date.now();
      if (timerOn()) changed();
    });
    const setPower = (p: { action: "sleep" | "shutdown" | null; at?: number }) => {
      power = p.action ? { action: p.action, at: p.at! } : null;
      cx.island.activityChanged("power");
    };
    cx.call<{ action: "sleep" | "shutdown" | null; at?: number }>("powerState").then((p) => p && setPower(p));
    const offs = [
      cx.on("power", setPower),
      // Shutdown asks first: nothing runs until the click.
      cx.on<{ action: string }>("powerDue", () =>
        cx.island.notice({
          kind: "timer",
          title: cx.t("Shut down now?"),
          icon: "moon",
          tone: "warn",
          action: { label: cx.t("Shut down"), run: () => void cx.call("powerNow", { action: "shutdown" }) },
          section: "timer",
        }),
      ),
      cx.on<Clock>("state", setClock),
      cx.on<{ mode: string; phase: string; next?: string; final: boolean; skipped?: boolean }>("done", (e) => {
        const pomo = e.mode === "pomodoro";
        const title = pomo && !e.final ? (e.skipped ? "Skipped" : e.phase === "focus" ? "Focus done" : "Break over") : "Time's up";
        cx.island.notice({
          kind: "timer",
          title: cx.t(title),
          detail: e.next ? cx.t(e.next === "focus" ? "Next: focus" : e.next === "long" ? "Next: long break" : "Next: short break") : undefined,
          icon: "timer",
          tone: "warn",
          section: "timer",
        });
        if (!e.skipped) startAlarm();
      }),
    ];
    return () => {
      offs.forEach((f) => f());
      offS();
      stopAlarm();
      clock = IDLE;
      power = null;
    };
  },
});

/** The alarm tone repeats every 2 s for at most 5 minutes, until dismissed. */
function startAlarm() {
  stopAlarm();
  if (!sound) return;
  const t0 = Date.now();
  const ring = () => (Date.now() - t0 > 300_000 ? stopAlarm() : Sound.play("finish"));
  ring();
  alarm = window.setInterval(ring, 2000);
}
function stopAlarm() {
  if (alarm != null) clearInterval(alarm);
  alarm = undefined;
}

// ── Timer section ────────────────────────────────────────────────────────────

const MODES = [["timer", "Timer"], ["pomodoro", "Pomodoro"], ["stopwatch", "Stopwatch"]] as const;

function timerPage(cx: FeatureCtx) {
  const el = h("div", { class: "f-timers-downloads-page" });
  let mode: Clock["mode"] = "timer";
  const cfg: Record<string, number> = { minutes: 25, focus: 25, short: 5, long: 15, every: 4, sessions: 4 };
  let shown = false;
  let timerId: number | undefined;
  let big: HTMLElement | undefined;
  let bar: HTMLElement | undefined;

  cx.settings.get().then((s) => {
    for (const k of Object.keys(cfg)) if (typeof s[k] === "number") cfg[k] = s[k] as number;
    if (typeof s.mode === "string") mode = s.mode as Clock["mode"];
    draw();
  });

  const num = (key: string, label: string, min: number, max: number) =>
    h("label", { class: "f-timers-downloads-num" },
      h("span", { text: cx.t(label) }),
      Object.assign(h("input", { type: "number", min: String(min), max: String(max) }), { value: String(cfg[key]) }, {
        oninput(this: HTMLInputElement) { cfg[key] = Math.min(max, Math.max(min, Number(this.value) || min)); },
      }));

  let release: (() => void) | undefined;
  const setMin = (f: number) => {
    cfg.minutes = 1 + Math.round(f * 179);
    readout.textContent = t("{count} min", { count: cfg.minutes });
  };

  const go = () => {
    cx.settings.patch({ mode, ...cfg });
    cx.call("start", { mode, ...cfg });
  };

  function draw() {
    big = bar = undefined;
    const c = clock;
    if (c.status === "idle") {
      const tabs = h("div", { class: "f-timers-downloads-tabs" },
        ...MODES.map(([m, label]) =>
          h("button", { class: `tab${m === mode ? " on" : ""}`, onclick: () => { mode = m; draw(); } }, h("span", { text: cx.t(label) }))));
      const body =
        mode === "timer"
          ? [
              h("div", { class: "f-timers-downloads-presets" },
                ...[5, 10, 15, 25, 45, 60].map((m) => h("button", { class: `tab${cfg.minutes === m ? " on" : ""}`, onclick: () => { cfg.minutes = m; draw(); } }, h("span", { text: t("{count} min", { count: m }) })))),
              h("div", { class: "f-timers-downloads-ruler" },
                slider((cfg.minutes - 1) / 179, {
                  label: cx.t("Minutes"),
                  onInput: setMin,
                  onChange: setMin,
                  onDrag: (d) => { release?.(); release = d ? cx.island.hold() : undefined; },
                }),
                (readout = h("b", { text: t("{count} min", { count: cfg.minutes }) }))),
            ]
          : mode === "pomodoro"
            ? [h("div", { class: "f-timers-downloads-nums" }, num("focus", "Focus (min)", 1, 180), num("short", "Short break", 1, 60), num("long", "Long break", 1, 60), num("every", "Long every", 1, 24), num("sessions", "Sessions", 1, 24))]
            : [h("p", { class: "f-timers-downloads-hint", text: cx.t("Counts up with laps, to 99:59:59.") })];
      el.replaceChildren(tabs, ...body, h("div", { class: "f-timers-downloads-actions" }, btn(cx.t("Start"), "primary", go)), powerRow());
      return;
    }
    const label =
      c.status === "finished" ? "Time's up"
      : c.status === "waiting" ? (c.phase === "focus" ? "Next: focus" : c.phase === "long" ? "Next: long break" : "Next: short break")
      : c.mode === "stopwatch" ? "Stopwatch"
      : c.mode === "pomodoro" ? (c.phase === "focus" ? "Focus" : "Break")
      : "Timer";
    const actions: HTMLElement[] = [];
    if (c.status === "running") actions.push(btn(cx.t("Pause"), "secondary", () => cx.call("pause")));
    if (c.status === "paused") actions.push(btn(cx.t("Resume"), "primary", () => cx.call("resume")));
    if (c.status === "waiting") actions.push(btn(cx.t("Start"), "primary", () => cx.call("start", {})));
    if (c.mode === "pomodoro" && c.status !== "finished") actions.push(btn(cx.t("Skip"), "secondary", () => cx.call("skip")));
    if (c.mode === "stopwatch" && c.status === "running") actions.push(btn(cx.t("Lap"), "secondary", () => cx.call("lap")));
    actions.push(btn(cx.t(c.status === "finished" ? "Dismiss" : "Cancel"), c.status === "finished" ? "primary" : "ghost", () => cx.call("cancel")));
    big = h("div", { class: `f-timers-downloads-clock${c.status === "paused" ? " paused" : ""}`, text: full(secs()) });
    bar = c.mode === "stopwatch" ? undefined : meter(0, "warn");
    el.replaceChildren(
      h("div", { class: "f-timers-downloads-label" },
        h("span", { text: cx.t(label) }),
        c.mode === "pomodoro" ? h("small", { text: cx.t("Session {count} of {total}", { count: Math.min(c.session, c.sessions), total: c.sessions }) }) : null),
      big,
      bar ?? "",
      h("div", { class: "f-timers-downloads-actions" }, ...actions),
      c.laps.length
        ? h("div", { class: "f-timers-downloads-laps" }, ...c.laps.map((l, i) => h("span", { text: `${i + 1}  ${full(Math.floor(l / 1000))}` })).reverse())
        : "",
      powerRow(),
    );
    paint();
  }
  let readout = h("b");
  /** Armed: the countdown and Cancel. Idle: pick sleep or shut down, and when. */
  function powerRow(): HTMLElement | string {
    if (power) {
      return h("div", { class: "f-timers-downloads-power" }, h("span", { text: `${cx.t(powerWord())} · ${full(powerLeft())}` }), btn(cx.t("Cancel"), "ghost", () => void powerCall("powerCancel")));
    }
    if (clock.status !== "idle") return "";
    const act = h("select", {}, h("option", { value: "sleep", text: cx.t("Sleep") }), h("option", { value: "shutdown", text: cx.t("Shut down") }));
    const mins = h("select", {}, ...[15, 30, 60, 90, 120, 180].map((m) => h("option", { value: String(m), text: t("{count} min", { count: m }) })));
    const at = h("input", { type: "time" });
    const arm = () => {
      let when = Date.now() + Number(mins.value) * 60000;
      if (at.value) {
        const [hh, mm] = at.value.split(":").map(Number);
        const d = new Date();
        d.setHours(hh, mm, 0, 0);
        if (d.getTime() <= Date.now()) d.setDate(d.getDate() + 1);
        when = d.getTime();
      }
      void powerCall("powerArm", { action: act.value, at: when });
    };
    return h("div", { class: "f-timers-downloads-power" }, h("label", { class: "field" }, act), h("label", { class: "field" }, mins), at, btn(cx.t("Arm"), "secondary", arm));
  }

  let drawnStatus = "";
  function paint() {
    if (big) big.textContent = full(secs());
    const pr = el.querySelector(".f-timers-downloads-power span");
    if (power && pr) pr.textContent = `${cx.t(powerWord())} · ${full(powerLeft())}`;
    if (bar && clock.totalMs) setMeter(bar, 1 - left() / clock.totalMs);
  }
  const tick = () => {
    const key = `${clock.status}|${clock.mode}|${clock.phase}|${clock.laps.length}|${power ? power.action : ""}`;
    if (key !== drawnStatus) { drawnStatus = key; draw(); } else paint();
  };

  return {
    el,
    show() {
      shown = true;
      drawnStatus = "";
      tick();
      timerId = window.setInterval(tick, 250);
    },
    hide() {
      shown = false;
      if (timerId != null) clearInterval(timerId);
      timerId = undefined;
    },
    sync() {
      if (shown) tick();
    },
  };
}
