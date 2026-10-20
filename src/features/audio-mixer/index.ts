// Audio mixer — the page side of the feature (docs/ARCHITECTURE.md §2.3).
// Nothing runs at import: surfaces are declared here, work begins in `start`.
//
// Rust (crates/audio-mixer over crates/svc-audio) reads and sets the levels and
// tells of changes. This draws the Mixer section: the output and the
// microphone on the left, a fader per app that plays on the right. The apps
// are watched only while the section is on screen (`watch`). What is kept
// here: the pins, the order and the hide list (settings), and the notices for
// what the shortcuts did.

import "./style.css";
import { defineFeature, type FeatureCtx, type SectionHost, type Value } from "../../core/feature";
import { card, iconBtn, menu, row, setMeter, slider, stripRow, toggle, type MenuItem } from "../../ui/components";
import { icon } from "../../ui/icons";
import { h } from "../../views/dom";

/** The mouse wheel over a fader: 5 % a notch, up is louder. */
const wheel = (fader: HTMLElement, set: (fraction: number) => void) =>
  fader.addEventListener(
    "wheel",
    (e) => {
      if (fader.getAttribute("aria-disabled") === "true") return;
      e.preventDefault();
      const v = Math.max(0, Math.min(1, Number(fader.style.getPropertyValue("--v") || 0) + (e.deltaY < 0 ? 0.05 : -0.05)));
      setMeter(fader, v);
      set(v);
    },
    { passive: false },
  );

// ── What Rust sends ───────────────────────────────────────────────────────────

interface App {
  /** The program's file name in lower case; "system" for the system sounds. */
  id: string;
  name: string;
  volume: number;
  mute: boolean;
  playing: boolean;
  system: boolean;
}

interface Level {
  level: number;
  mute: boolean;
}

interface Device {
  id: string;
  name: string;
  default: boolean;
  /** The device call apps use (the Windows communications device). */
  calls: boolean;
  headphones: boolean;
}

interface Devices {
  /** False where Windows will not let the default device be set: no pickers. */
  canSwitch: boolean;
  out: Level | null;
  mic: Level | null;
  outputs: Device[];
  inputs: Device[];
  micsMuted: boolean;
}

const DEFAULTS = {
  pinned: [] as string[],
  order: [] as string[],
  /** id -> the name it had, for the list of what is hidden. */
  hidden: {} as Record<string, string>,
  hideInactive: false,
  switcher: [] as string[],
  switchNotice: true,
  callsFollow: false,
  guard: false,
  guardPercent: 25,
  preferredMic: "",
  auto: false,
  /** Output ids, the preferred first. */
  priority: [] as string[],
};
type Options = typeof DEFAULTS;
type Point = { x: number; y: number };

// Icons only this feature draws (src/ui/icons.ts has the shared set).
const MIC_OFF = "M12 4a3 3 0 0 0-3 3v4a3 3 0 0 0 6 0V7a3 3 0 0 0-3-3zM6 11a6 6 0 0 0 12 0M12 17v3.5M5 4.5l14 15";
const HEADPHONES =
  "M5 15v-3a7 7 0 0 1 14 0v3M5 14.5h2a1 1 0 0 1 1 1v3a1 1 0 0 1-1 1H6a1 1 0 0 1-1-1zM19 14.5h-2a1 1 0 0 0-1 1v3a1 1 0 0 0 1 1h1a1 1 0 0 0 1-1z";
/** Draws nothing: keeps the labels of a menu in line with the ticked one. */
const BLANK = "M0 0";

// ── State (set in `start`, cleared when the feature stops) ────────────────────

let opts: Options = DEFAULTS;
let micsMuted = false;
/** The feature's context, for the strip row, which is not handed one. */
let ctx: FeatureCtx | null = null;
/** The section, while it is on screen. */
let repaint: (() => void) | null = null;
/** App icons as data URLs, by app id; null = asked, or it has none. */
const icons = new Map<string, string | null>();

/**
 * A fader's place for a level, and back. Hearing is logarithmic: on a straight
 * fader everything happens in the bottom fifth. The level is the square of the
 * place, and the number beside the fader is always the level (vorssaint#2700).
 * Only the apps' faders: the level of a device is tapered by Windows already.
 */
const placeOf = (level: number) => Math.sqrt(Math.max(0, Math.min(1, level)));
const levelOf = (place: number) => place * place;

/** While a fader is dragged, the newest value at most every 60 ms; the last one always goes. */
function paced(send: (value: number) => void): (value: number) => void {
  let timer = 0;
  let waiting: number | null = null;
  const fire = () => {
    timer = 0;
    if (waiting == null) return;
    const value = waiting;
    waiting = null;
    send(value);
    timer = window.setTimeout(fire, 60);
  };
  return (value) => {
    waiting = value;
    if (!timer) fire();
  };
}

/**
 * The saved order after `id` moved `offset` places among the rows on screen
 * that are pinned like it. Only those rows' slots are rewritten: apps that are
 * closed or hidden keep theirs. Null when there is nowhere to go.
 */
function moved(order: string[], pinned: string[], id: string, offset: number, visible: string[]): string[] | null {
  const isPinned = (x: string) => pinned.includes(x);
  const group = visible.filter((x) => isPinned(x) === isPinned(id));
  const from = group.indexOf(id);
  const to = from + offset;
  if (from < 0 || to < 0 || to >= group.length) return null;
  group.splice(from, 1);
  group.splice(to, 0, id);
  const moving = new Set(group);
  let next = 0;
  return [...new Set([...order, ...visible])].map((x) => (moving.has(x) ? group[next++] : x));
}

/** An icon of a button that changes with the state: drawn again only when it does. */
function setIcon(button: HTMLElement, name: string) {
  if (button.dataset.icon === name) return;
  button.dataset.icon = name;
  button.replaceChildren(icon(name, 14));
}

/** A button that names what is picked and opens the list under itself. */
function picker(kind: "ghost sm" | "secondary", open: (at: Point) => void) {
  const name = h("span");
  const el = h("button", { class: `btn ${kind} f-audio-mixer-pick` }, name, icon("chevd", 12));
  el.addEventListener("click", () => {
    const r = el.getBoundingClientRect();
    open({ x: r.left, y: r.bottom + 4 });
  });
  return { el, name };
}

// ── The Mixer section ─────────────────────────────────────────────────────────

interface Row {
  app: App;
  el: HTMLElement;
  ico: HTMLElement;
  name: HTMLElement;
  fader: HTMLElement;
  pct: HTMLElement;
  mute: HTMLButtonElement;
}

function buildSection(cx: FeatureCtx): SectionHost {
  const t = cx.t;
  const percent = (level: number) => t("{percent}%", { percent: Math.round(level * 100) });
  let shown = false;
  let devices: Devices | null = null;
  let apps: App[] = [];
  /** The ids of the rows on screen, in order: the list is rebuilt only when it changes. */
  let drawn = "";
  let offs: (() => void)[] = [];
  let release: (() => void) | null = null;
  /** `watch` on and off reach Rust in the order they were asked. */
  let chain: Promise<unknown> = Promise.resolve();
  const rows = new Map<string, Row>();

  // The pointer may leave the island while a fader is dragged: the island stays.
  const hold = (on: boolean) => {
    release?.();
    release = on ? cx.island.hold() : null;
    if (!on) paint();
  };
  /** A choice of the user's: shown at once, then saved. */
  const save = (patch: Partial<Options>) => {
    opts = { ...opts, ...patch };
    paint();
    void cx.settings.patch(patch);
  };

  // ── Left: the output and the microphone ──

  /** The level of the default output or microphone, as last told (or dragged to). */
  const current = (flow: "out" | "in") => (flow === "out" ? devices?.out : devices?.mic) ?? null;

  const device = (flow: "out" | "in", label: string, onButton: () => void, onPick: (at: Point) => void) => {
    const button = iconBtn(flow === "out" ? "volume" : "mic", label, onButton);
    button.classList.add("sm");
    const pick = picker("ghost sm", onPick);
    const pct = h("span", { class: "f-audio-mixer-pct" });
    const send = paced((level) => void cx.call("volume", { flow, level }));
    const set = (level: number) => {
      const now = current(flow);
      if (now) now.level = level;
      pct.textContent = percent(level);
      send(level);
    };
    const fader = slider(0, { label, onInput: set, onChange: set, onDrag: hold });
    wheel(fader, set);
    // The device's name gets the whole width: Windows names them at length.
    const el = card([pick.el, h("div", { class: "f-audio-mixer-fader" }, button, fader, pct)]);
    el.classList.add("f-audio-mixer-dev");
    return { el, button, pick, pct, fader };
  };

  const ticked = (d: Device, pinned = false) => (pinned ? "pin" : d.default ? "check" : BLANK);
  const out = device(
    "out",
    t("Output volume"),
    () => {
      if (!devices?.out) return;
      devices.out.mute = !devices.out.mute;
      void cx.call("mute", { mute: devices.out.mute });
      paint();
    },
    (at) =>
      menu(
        (devices?.outputs ?? []).map((d) => ({
          label: d.name,
          icon: ticked(d),
          onClick: () => void cx.call("output", { id: d.id }),
        })),
        at,
      ),
  );
  const mic = device(
    "in",
    t("Microphone level"),
    () => void cx.call("muteMics", { on: !micsMuted }),
    (at) => {
      const items: (MenuItem | null)[] = (devices?.inputs ?? []).map((d) => ({
        label: d.name,
        icon: ticked(d, d.id === opts.preferredMic),
        onClick: () => save({ preferredMic: d.id }),
      }));
      if (opts.preferredMic) {
        items.push(null, { label: t("Let Windows pick the microphone"), icon: "close", onClick: () => save({ preferredMic: "" }) });
      }
      menu(items, at);
    },
  );
  out.pick.el.title = t("Sound output");
  mic.pick.el.title = t("Microphone. The one you pick stays the default while it is connected.");

  const hiddenBtn = h("button", { class: "btn sm ghost" });
  hiddenBtn.addEventListener("click", () => {
    const r = hiddenBtn.getBoundingClientRect();
    const items = Object.entries(opts.hidden).map(([id, name]) => ({
      label: name || id,
      icon: "plus",
      onClick: () => save({ hidden: Object.fromEntries(Object.entries(opts.hidden).filter(([x]) => x !== id)) }),
    }));
    menu(items, { x: r.left, y: r.top - 4 - 28 * items.length - 8 });
  });
  const foot = h("div", { class: "f-audio-mixer-foot" }, hiddenBtn);

  // ── Right: a fader per app ──

  const list = h("div", { class: "f-audio-mixer-apps" });
  const empty = h(
    "div",
    { class: "f-audio-mixer-empty" },
    icon("sliders", 20),
    h("b", { text: t("Nothing to mix yet") }),
    h("span", { text: t("Apps that use audio show up here.") }),
  );

  empty.hidden = true;

  const nameOf = (a: App) => (a.system ? t("System sounds") : a.name);

  const rowMenu = (a: App, at: Point) => {
    const pinned = opts.pinned.includes(a.id);
    const ids = visible().map((x) => x.id);
    const move = (offset: number) => {
      const order = moved(opts.order, opts.pinned, a.id, offset, ids);
      if (order) save({ order });
    };
    menu(
      [
        {
          label: t(pinned ? "Unpin" : "Pin to top"),
          icon: "pin",
          onClick: () => save({ pinned: pinned ? opts.pinned.filter((x) => x !== a.id) : [...opts.pinned, a.id] }),
        },
        { label: t("Move up"), icon: "chevu", onClick: () => move(-1) },
        { label: t("Move down"), icon: "chevd", onClick: () => move(1) },
        null,
        {
          label: t("Reset to 100%"),
          icon: "refresh",
          onClick: () => {
            a.volume = 1;
            a.mute = false;
            void cx.call("appVolume", { id: a.id, level: 1 });
            void cx.call("appMute", { id: a.id, mute: false });
            paint();
          },
        },
        { label: t("Hide from the list"), icon: "close", onClick: () => save({ hidden: { ...opts.hidden, [a.id]: nameOf(a) } }) },
      ],
      at,
    );
  };

  const makeRow = (app: App): Row => {
    const id = app.id;
    const ico = h("span", { class: "f-audio-mixer-ico none" }, icon(app.system ? "volume" : "music", 13));
    const name = h("span");
    const pct = h("span", { class: "f-audio-mixer-pct" });
    const send = paced((level) => void cx.call("appVolume", { id, level }));
    const set = (place: number) => {
      r.app.volume = levelOf(place);
      pct.textContent = percent(r.app.volume);
      send(r.app.volume);
    };
    const fader = slider(0, { label: nameOf(app), onInput: set, onChange: set, onDrag: hold });
    wheel(fader, set);
    const mute = iconBtn("volume", t("Mute"), () => {
      r.app.mute = !r.app.mute;
      void cx.call("appMute", { id, mute: r.app.mute });
      paintRow(r);
    });
    mute.classList.add("sm");
    const el = h(
      "div",
      { class: "f-audio-mixer-app" },
      ico,
      h("span", { class: "f-audio-mixer-name" }, icon("pin", 11), name),
      fader,
      pct,
      mute,
    );
    el.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      rowMenu(r.app, { x: e.clientX, y: e.clientY });
    });
    const r: Row = { app, el, ico, name, fader, pct, mute };
    return r;
  };

  /** The app's own icon, asked for once; a note until it is here, and for an app that has none. */
  const paintIcon = (r: Row) => {
    const id = r.app.id;
    if (r.app.system) return;
    const known = icons.get(id);
    if (known === undefined) {
      icons.set(id, null);
      void cx.call<string>("icon", { id }).then((url) => {
        icons.set(id, url);
        const now = rows.get(id);
        if (now) paintIcon(now);
      });
      return;
    }
    r.ico.style.backgroundImage = known ? `url("${known}")` : "";
    r.ico.classList.toggle("none", !known);
  };

  const paintRow = (r: Row) => {
    const a = r.app;
    r.el.classList.toggle("playing", a.playing);
    r.el.classList.toggle("muted", a.mute);
    r.el.classList.toggle("pinned", opts.pinned.includes(a.id));
    r.name.textContent = nameOf(a);
    r.mute.title = t(a.mute ? "Unmute" : "Mute");
    setIcon(r.mute, a.mute ? "mute" : "volume");
    // A fader that is held is the user's: it is left where the pointer has it.
    if (!r.fader.classList.contains("dragging")) {
      setMeter(r.fader, placeOf(a.volume));
      r.pct.textContent = percent(a.volume);
    }
    paintIcon(r);
  };

  /** What is listed: not what is hidden; pinned first, then the saved order, then by name (as Rust sends them). */
  function visible(): App[] {
    const listed = apps.filter(
      (a) => !(a.id in opts.hidden) && (!opts.hideInactive || a.playing || a.mute || a.volume < 0.995),
    );
    const pin = (a: App) => (opts.pinned.includes(a.id) ? 0 : 1);
    const rank = (a: App) => (opts.order.indexOf(a.id) + 1 || opts.order.length + 1);
    return listed
      .map((a, i) => ({ a, i }))
      .sort((x, y) => pin(x.a) - pin(y.a) || rank(x.a) - rank(y.a) || x.i - y.i)
      .map((x) => x.a);
  }

  function paintApps() {
    const listed = visible();
    for (const a of listed) {
      let r = rows.get(a.id);
      if (!r) rows.set(a.id, (r = makeRow(a)));
      r.app = a;
      paintRow(r);
    }
    const order = listed.map((a) => a.id).join("\n");
    // Moving a row takes the pointer off its fader: the rows move once it is let go.
    if (order !== drawn && !list.querySelector(".dragging")) {
      drawn = order;
      list.replaceChildren(...listed.map((a) => rows.get(a.id)!.el));
      for (const id of [...rows.keys()]) if (!listed.some((a) => a.id === id)) rows.delete(id);
    }
    list.hidden = listed.length === 0;
    empty.hidden = listed.length > 0;
    const hidden = Object.keys(opts.hidden).length;
    foot.hidden = hidden === 0;
    hiddenBtn.textContent = t("Hidden: {count}", { count: hidden });
  }

  function paintDevices() {
    const d = devices;
    if (!d) return;
    const level = (b: typeof out, now: Level | null, off: boolean) => {
      b.fader.setAttribute("aria-disabled", String(!now || off));
      if (b.fader.classList.contains("dragging")) return;
      setMeter(b.fader, now?.level ?? 0);
      b.pct.textContent = now ? percent(now.level) : "";
    };
    const speaker = d.outputs.find((x) => x.default);
    out.pick.name.textContent = speaker?.name ?? t("No output");
    out.pick.el.disabled = !d.canSwitch || d.outputs.length === 0;
    const silent = !d.out || d.out.mute || d.out.level < 0.005;
    setIcon(out.button, silent ? "mute" : speaker?.headphones ? HEADPHONES : "volume");
    out.button.title = t(d.out?.mute ? "Unmute" : "Mute");
    out.button.disabled = !d.out;
    level(out, d.out, false);

    const input = d.inputs.find((x) => x.default);
    mic.pick.name.textContent = input?.name ?? t("No microphone");
    mic.pick.el.disabled = !d.canSwitch || d.inputs.length === 0;
    mic.pick.el.classList.toggle("kept", !!opts.preferredMic && input?.id === opts.preferredMic);
    // Red while this app holds every microphone muted; a mute of the user's own is only drawn.
    setIcon(mic.button, micsMuted || d.mic?.mute ? MIC_OFF : "mic");
    mic.button.title = t(micsMuted ? "Unmute the microphones" : "Mute all microphones");
    mic.button.classList.toggle("danger", micsMuted);
    mic.button.classList.toggle("ghost", !micsMuted);
    mic.button.disabled = d.inputs.length === 0 && !micsMuted;
    // A level never opens a muted microphone: the fader waits for the mute to end.
    level(mic, d.mic, micsMuted);
  }

  function paint() {
    if (!shown) return;
    paintDevices();
    paintApps();
  }

  const el = h(
    "div",
    { class: "f-audio-mixer-page" },
    h("div", { class: "f-audio-mixer-side" }, out.el, mic.el, foot),
    h("div", { class: "f-audio-mixer-main" }, list, empty),
  );

  return {
    el,
    show() {
      shown = true;
      repaint = paint;
      offs = [
        cx.on<App[]>("apps", (next) => {
          apps = next;
          paintApps();
        }),
        cx.on<Devices>("devices", (next) => {
          devices = next;
          paintDevices();
        }),
        cx.on<Level & { flow: "out" | "in" }>("level", ({ flow, level, mute }) => {
          if (!devices) return;
          // What comes while a fader is held is the echo of where it was a moment ago.
          const held = (flow === "out" ? out : mic).fader.classList.contains("dragging");
          const now = { level: held ? (current(flow)?.level ?? level) : level, mute };
          if (flow === "out") devices.out = now;
          else devices.mic = now;
          paintDevices();
        }),
      ];
      chain = chain
        .then(() => cx.call("watch", { on: true }))
        .then(() => cx.call<Devices & { apps: App[] }>("state"))
        .then((state) => {
          if (!state || !shown) return;
          devices = state;
          apps = state.apps;
          paint();
        });
    },
    hide() {
      shown = false;
      repaint = null;
      for (const off of offs) off();
      offs = [];
      release?.();
      release = null;
      chain = chain.then(() => cx.call("watch", { on: false }));
    },
    sync() {},
  };
}

// ── Settings ──────────────────────────────────────────────────────────────────

function settingsPage(cx: FeatureCtx): HTMLElement {
  const t = cx.t;
  const el = h("div", { class: "f-audio-mixer-settings" });
  void Promise.all([cx.settings.get(), cx.call<Devices>("devices")]).then(([stored, devices]) => {
    let now = { ...DEFAULTS, ...stored } as Options;
    const save = (patch: Partial<Options>) => {
      now = { ...now, ...patch };
      void cx.settings.patch(patch);
    };
    const item = (key: "hideInactive" | "switchNotice" | "callsFollow" | "guard" | "auto", label: string, hint: string) =>
      row(t(label), toggle(now[key], (on) => save({ [key]: on })), t(hint));
    const outputs = devices?.outputs ?? [];
    /** The outputs present, the ranked first in their order; the arrows rewrite the saved order. */
    const priorityList = () => {
      const box = h("div");
      const draw = () => {
        const rank = (d: Device) => now.priority.indexOf(d.id) + 1 || now.priority.length + 1;
        const list = outputs.map((d, i) => ({ d, i })).sort((a, b) => rank(a.d) - rank(b.d) || a.i - b.i).map((x) => x.d);
        const ids = list.map((d) => d.id);
        const go = (id: string, offset: number) => {
          const order = moved(now.priority, [], id, offset, ids);
          if (!order) return;
          save({ priority: order });
          draw();
        };
        box.replaceChildren(
          card(
            list.map((d, i) => {
              const up = iconBtn("chevu", t("Move up"), () => go(d.id, -1));
              const down = iconBtn("chevd", t("Move down"), () => go(d.id, 1));
              up.disabled = i === 0;
              down.disabled = i === list.length - 1;
              return row(d.name, h("span", { class: "f-audio-mixer-arrows" }, up, down));
            }),
          ),
          h("p", { class: "f-audio-mixer-note", text: t("The top one is preferred.") }),
        );
      };
      draw();
      return box;
    };

    el.append(
      h("div", { class: "section-title", text: t("Mixer") }),
      card([item("hideInactive", "Only list apps that are playing", "An app whose volume you changed stays listed.")]),
    );

    if (devices?.canSwitch) {
      const calls = picker("secondary", (at) =>
        menu(
          outputs.map((d) => ({
            label: d.name,
            icon: d.name === calls.name.textContent ? "check" : BLANK,
            onClick: () => {
              calls.name.textContent = d.name;
              void cx.call("calls", { id: d.id });
            },
          })),
          at,
        ),
      );
      calls.name.textContent = outputs.find((d) => d.calls)?.name ?? t("No output");
      el.append(
        h("div", { class: "section-title", text: t("Outputs") }),
        card([
          row(t("Output for calls"), calls.el, t("Where call apps play: the Windows communications device.")),
          item("callsFollow", "Calls follow the output", "Picking an output also makes it the one for calls."),
          item("switchNotice", "Name the new output", "The island names the device after the shortcut switched to it."),
        ]),
        h("div", { class: "section-title", text: t("Automatic output") }),
        card([
          item(
            "auto",
            "Switch to the preferred output by itself",
            "When an output higher in the list is plugged in or connected it becomes the output; when the current one goes away, the highest left takes over. Not after you pick one yourself.",
          ),
        ]),
        priorityList(),
        h("div", { class: "section-title", text: t("Outputs the shortcut goes through") }),
        card(
          outputs.map((d) =>
            row(
              d.name,
              toggle(now.switcher.includes(d.id), (on) =>
                save({ switcher: on ? [...now.switcher, d.id] : now.switcher.filter((x) => x !== d.id) }),
              ),
            ),
          ),
        ),
        h("p", { class: "f-audio-mixer-note", text: t("With none switched on, the shortcut goes through every output.") }),
      );
    }

    const pct = h("span", { class: "f-audio-mixer-pct", text: t("{percent}%", { percent: now.guardPercent }) });
    // In steps of 5, down to silence.
    const snap = (fraction: number) => Math.round(fraction * 20) * 5;
    const range = slider(now.guardPercent / 100, {
      label: t("Volume to lower to"),
      onInput: (fraction) => (pct.textContent = t("{percent}%", { percent: snap(fraction) })),
      onChange: (fraction) => {
        const guardPercent = snap(fraction);
        setMeter(range, guardPercent / 100);
        pct.textContent = t("{percent}%", { percent: guardPercent });
        save({ guardPercent });
      },
    });
    el.append(
      h("div", { class: "section-title", text: t("Headphones") }),
      card([
        item(
          "guard",
          "Lower the volume when headphones disconnect",
          "So the speakers do not blare. They get their level back when the headphones return. For Bluetooth, USB and any jack that is a device of its own.",
        ),
        row(t("Volume to lower to"), h("span", { class: "f-audio-mixer-range" }, range, pct)),
      ]),
    );
  });
  return el;
}

// ── The feature ───────────────────────────────────────────────────────────────

export default defineFeature({
  id: "audio-mixer",
  voice: () => import("./voice"),

  // While this app holds every microphone muted, the strip says so in red.
  activities: [
    {
      id: "muted",
      order: 3.7,
      section: "mixer",
      isActive: () => micsMuted,
      render: () =>
        stripRow({
          icon: MIC_OFF,
          tone: "bad",
          title: ctx?.t("Microphones muted") ?? "",
          action: { label: ctx?.t("Unmute") ?? "", run: () => void ctx?.call("muteMics", { on: false }) },
        }),
    },
  ],

  sections: [{ id: "mixer", title: "Mixer", icon: "sliders", letter: "x", height: 300, build: buildSection }],

  settings: settingsPage,

  start: (cx) => {
    ctx = cx;
    const applyOptions = (stored: Value) => {
      opts = { ...DEFAULTS, ...stored } as Options;
      repaint?.();
    };
    const setMuted = (muted: boolean) => {
      if (muted === micsMuted) return;
      micsMuted = muted;
      cx.island.activityChanged("muted");
      repaint?.();
    };
    void cx.settings.get().then(applyOptions);
    const offs = [
      cx.settings.onChange(applyOptions),
      // The shortcut switched the output: the island names it (vorssaint#2098).
      cx.on<{ name: string; ok: boolean; headphones: boolean }>("switched", (s) => {
        if (!s.ok) {
          cx.island.notice({
            kind: "accessory",
            title: cx.t(s.name ? "Could not switch the output" : "No output to switch to"),
            detail: s.name || undefined,
            icon: "volume",
            tone: "bad",
            section: "mixer",
          });
        } else if (opts.switchNotice) {
          cx.island.notice({
            kind: "accessory",
            title: s.name,
            detail: cx.t("Sound output"),
            icon: s.headphones ? HEADPHONES : "volume",
            section: "mixer",
          });
        }
      }),
      // The priority list switched the output by itself: named, with a way back.
      cx.on<{ name: string; headphones: boolean; from: string | null }>("auto", (a) => {
        const from = a.from;
        cx.island.notice({
          kind: "accessory",
          title: a.name,
          detail: cx.t("Switched automatically"),
          icon: a.headphones ? HEADPHONES : "volume",
          section: "mixer",
          action: from ? { label: cx.t("Undo"), run: () => void cx.call("revertOutput", { id: from }) } : undefined,
        });
      }),
      // A level, so that it shows in the header while the island is open and is not kept for later.
      cx.on<{ muted: boolean; partial: boolean; level: number }>("mics", (m) => {
        setMuted(m.muted);
        const title = m.partial
          ? m.muted ? "Some microphones could not be muted" : "Some microphones are still muted"
          : m.muted ? "Microphones muted" : "Microphones on";
        // A warning is a sentence: it has the whole row and four seconds, not the place beside a meter.
        cx.island.notice({
          kind: m.partial ? "accessory" : "mic",
          title: cx.t(title),
          level: m.partial ? undefined : m.level,
          icon: m.muted ? MIC_OFF : "mic",
          tone: m.partial ? "warn" : m.muted ? "bad" : undefined,
          section: "mixer",
        });
      }),
      cx.on<{ level: number }>("guard", ({ level }) =>
        cx.island.notice({ kind: "volume", title: cx.t("Headphones disconnected"), level, icon: "volume" }),
      ),
    ];
    // The page may start after Rust did (a reload): the red state is not left behind.
    void cx.call<Devices>("devices").then((d) => d && ctx === cx && setMuted(d.micsMuted));

    return () => {
      for (const off of offs) off();
      ctx = null;
      repaint = null;
      micsMuted = false;
      opts = DEFAULTS;
      icons.clear();
    };
  },
});
