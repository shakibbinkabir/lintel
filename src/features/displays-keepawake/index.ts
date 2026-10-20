// Displays and keep awake — the page side of the feature (docs/ARCHITECTURE.md §2.3).
// Rust holds the keep-awake request, the brightness writes, the overlays, the wallpaper and the radios;
// this file draws the strip, the three sections, the settings and the dimming overlay page.

import "./style.css";
import { defineFeature, type FeatureCtx } from "../../core/feature";
import { State } from "../../core/state";
import { h } from "../../views/dom";
import { t } from "../../i18n/i18n";
import { icon } from "../../ui/icons";
import { btn, card, field, row, setMeter, slider, stripRow, toggle } from "../../ui/components";

interface SessionState { active: boolean; startedAt?: number; endsAt?: number | null; auto?: boolean }
interface DisplayRow { id: string; name: string; internal: boolean; primary: boolean; method: "wmi" | "ddc" | "ddc-write" | "none"; level: number }
interface Pic { path: string; name: string }

const P = "f-displays-keepawake-";
let session: SessionState = { active: false };

// ── readings ─────────────────────────────────────────────────────────────────

/** Whole minutes left, rounded up: "45m", "1h35"; "∞" when open-ended. */
function left(): string {
  if (session.endsAt == null) return "∞";
  const m = Math.max(0, Math.ceil((session.endsAt - Date.now()) / 60000));
  return m >= 60 ? `${Math.floor(m / 60)}h${String(m % 60).padStart(2, "0")}` : `${m}m`;
}

const DURATIONS: [number, string][] = [[15, "15 min"], [30, "30 min"], [60, "1 hour"], [120, "2 hours"], [240, "4 hours"], [480, "8 hours"], [0, "Until stopped"]];

export default defineFeature({
  id: "displays-keepawake",
  voice: () => import("./voice"),

  activities: [
    {
      id: "keepawake",
      order: 6,
      tick: true,
      section: "keep",
      isActive: () => session.active,
      render: () => stripRow({ icon: "coffee", tone: "warn", title: t("Keep awake"), detail: session.auto ? t("Automatic") : undefined, value: left() }),
      mark: () => h("span", { class: "isl-ico warn" }, icon("coffee", 14)),
    },
  ],

  sections: [
    { id: "keep", title: "Keep awake", icon: "coffee", letter: "k", height: 250, build: (cx) => keepPage(cx) },
    { id: "displays", title: "Displays", icon: "monitor", letter: "y", height: 250, build: (cx) => displaysPage(cx) },
    { id: "wallpaper", title: "Wallpaper", icon: "image", letter: "w", height: 250, build: (cx) => wallpaperPage(cx) },
  ],

  settings(cx) {
    const box = card([]);
    cx.settings.get().then(async (s) => {
      const radios = (await cx.call<{ bluetooth: string; wifi: string }>("radios")) ?? { bluetooth: "none", wifi: "none" };
      const set = (k: string) => (on: boolean) => void cx.settings.patch({ [k]: on });
      const pick = (k: string, opts: [string, string][], num = true) =>
        select(opts, String(s[k]), (v) => void cx.settings.patch({ [k]: num ? Number(v) : v }));
      const rows: (HTMLElement | null)[] = [
        h("div", { class: "section-title", text: cx.t("Keep awake") }),
        row(cx.t("Let the display sleep"), toggle(s.allowDisplaySleep === true, set("allowDisplaySleep")), cx.t("The PC stays awake; only the screen may turn off.")),
        row(cx.t("Stop on low battery"), pick("batteryLimit", [["0", cx.t("Never")], ["5", "5%"], ["10", "10%"], ["15", "15%"], ["20", "20%"]]), cx.t("Ends a session on battery at or below this charge.")),
        row(cx.t("Keep the pointer active"), toggle(s.jiggle === true, set("jiggle")), cx.t("Moves the pointer one pixel now and then, so that chat apps and lock timers see activity.")),
        row(cx.t("Pointer interval"), pick("jiggleMinutes", [["1", "1 min"], ["2", "2 min"], ["5", "5 min"], ["10", "10 min"], ["15", "15 min"]])),
        row(cx.t("Start with an external display"), toggle(s.autoExternal === true, set("autoExternal")), cx.t("A session runs while a monitor other than the built-in one is connected.")),
        row(cx.t("Start on power"), toggle(s.autoPower === true, set("autoPower")), cx.t("A session runs while the charger is plugged in.")),
        row(cx.t("Start while agents work"), toggle(s.autoAgents === true, set("autoAgents")), cx.t("A session runs while an agent works, and for a few quiet minutes after.")),
        row(cx.t("Quiet minutes after agents"), pick("agentsMinutes", [["0", "0"], ["2", "2 min"], ["5", "5 min"], ["10", "10 min"], ["30", "30 min"]])),
        row(cx.t("Need every condition"), toggle(s.autoAll === true, set("autoAll")), cx.t("Off: any one of them is enough.")),
        h("div", { class: "section-title", text: cx.t("Displays") }),
        row(cx.t("Dim below the minimum"), toggle(s.extraDimming !== false, set("extraDimming")), cx.t("The lowest part of a slider darkens the picture over the backlight's lowest step.")),
        row(cx.t("Brightness keys move every display"), toggle(s.linkKeys === true, set("linkKeys")), cx.t("The shortcuts step all displays by the same amount, keeping the gaps between them.")),
        row(cx.t("Move displays together"), toggle(s.linkDisplays === true, set("linkDisplays")), cx.t("Moving one slider moves the others by the same amount.")),
        radios.bluetooth !== "none" || radios.wifi !== "none" ? h("div", { class: "section-title", text: cx.t("Sleep") }) : null,
        radios.bluetooth !== "none"
          ? row(cx.t("Bluetooth off during sleep"), toggle(s.bluetoothSleep === true, set("bluetoothSleep")), cx.t("Back on at wake only if Lintel turned it off."))
          : null,
        radios.wifi !== "none"
          ? row(cx.t("Wi-Fi off during sleep"), toggle(s.wifiSleep === true, set("wifiSleep")), cx.t("Back on at wake only if Lintel turned it off."))
          : null,
        h("div", { class: "section-title", text: cx.t("Wallpaper") }),
        row(
          cx.t("Picture folder"),
          field({ value: String(s.wallpaperFolder ?? ""), placeholder: cx.t("Your Pictures folder"), onEnter: (v) => void cx.settings.patch({ wallpaperFolder: v.trim() }) }),
          cx.t("Press Enter to apply. Only pictures directly in this folder are listed."),
        ),
      ];
      box.append(...rows.filter((r): r is HTMLElement => r !== null));
    });
    return box;
  },

  // The dimming overlay: a black page whose opacity is the dimming. Rust makes the window click-through.
  windows: {
    dim(root, cx) {
      root.className = `${P}dim`;
      const meta = (window as unknown as { __TAURI_INTERNALS__?: { metadata?: { currentWindow?: { label?: string } } } }).__TAURI_INTERNALS__;
      const label = meta?.metadata?.currentWindow?.label ?? "";
      const set = (a: number) => (root.style.opacity = String(Math.max(0, Math.min(1, a))));
      cx.on<{ label: string; alpha: number }>("dim", (e) => e.label === label && set(e.alpha));
      void cx.call<number>("dimAlpha", { label }).then((a) => set(a ?? 0));
    },
  },

  start(cx) {
    cx.call<SessionState>("state").then((s) => {
      if (s) {
        session = s;
        cx.island.activityChanged("keepawake");
      }
    });
    // Read-only: whether an agent works, told to Rust on each change (it decides if the condition is on).
    // debt: reads State directly (as agent-usage does); a host event would be cleaner.
    let working = false;
    const sync = () => {
      const w = State.tasks.some((t) => t.state !== "idle" && t.state !== "sleeping");
      if (w !== working) void cx.call("agents", { working: (working = w) });
    };
    sync();
    const offs = [
      State.subscribe(sync),
      cx.on<SessionState>("session", (s) => {
        session = s;
        cx.island.activityChanged("keepawake");
      }),
      cx.on<{ reason: string }>("ended", (e) =>
        cx.island.notice(
          e.reason === "battery"
            ? { kind: "battery", title: cx.t("Keep awake stopped"), detail: cx.t("The battery is low."), icon: "battery", tone: "warn", section: "keep" }
            : { kind: "timer", title: cx.t("Keep awake ended"), icon: "coffee", tone: "ok", section: "keep" },
        ),
      ),
      cx.on<{ name: string; level: number; shortcut?: boolean; count?: number }>("brightness", (e) => {
        // The display is named only when there is more than one to tell apart.
        if (e.shortcut) cx.island.notice({ kind: "brightness", title: (e.count ?? 1) > 1 ? e.name : cx.t("Brightness"), level: e.level, icon: "sun", section: "displays" });
      }),
    ];
    return () => {
      offs.forEach((f) => f());
      session = { active: false };
    };
  },
});

/** A `.field` around a native select. */
function select(opts: [string, string][], value: string, on: (v: string) => void): HTMLElement {
  const sel = h("select");
  for (const [v, label] of opts) sel.append(h("option", { value: v, text: label }));
  sel.value = value;
  sel.addEventListener("change", () => on(sel.value));
  return h("label", { class: "field" }, sel);
}

// ── Keep awake ───────────────────────────────────────────────────────────────

function keepPage(cx: FeatureCtx) {
  const el = h("div", { class: `${P}page` });
  let last = 60;
  let timer: number | undefined;
  cx.settings.get().then((s) => {
    if (typeof s.minutes === "number") last = s.minutes;
    draw();
  });

  const clock = () => {
    if (session.endsAt == null) return cx.t("Until you stop it");
    const at = new Date(session.endsAt);
    return cx.t("Ends at {time}", { time: at.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" }) });
  };

  function draw() {
    if (!session.active) {
      el.replaceChildren(
        h("p", { class: `${P}hint`, text: cx.t("Keeps this PC from sleeping. Pick how long.") }),
        h("div", { class: `${P}chips` },
          ...DURATIONS.map(([m, label]) =>
            h("button", { class: `tab${m === last ? " on" : ""}`, onclick: () => void cx.call("start", { minutes: m }) }, h("span", { text: cx.t(label) })))),
      );
      return;
    }
    el.replaceChildren(
      h("div", { class: `${P}clock`, text: left() }),
      h("p", { class: `${P}hint`, text: session.auto ? `${cx.t("Automatic")} · ${clock()}` : clock() }),
      h("div", { class: `${P}chips` },
        session.endsAt != null ? btn(cx.t("+15 min"), "secondary", () => void cx.call("extend", { minutes: 15 })) : null,
        btn(cx.t("Stop"), "danger", () => void cx.call("stop")),
      ),
    );
  }

  // Listeners of one event are not called in the order they were added: this one may run before the
  // one in `start`, so it takes the session from the event itself instead of reading it a step late.
  const off = cx.on<SessionState>("session", (s) => {
    session = s;
    draw();
  });
  void off;
  return {
    el,
    show() {
      draw();
      // The reading moves by the minute.
      timer = window.setInterval(() => session.active && draw(), 15000);
    },
    hide() {
      if (timer != null) clearInterval(timer);
      timer = undefined;
    },
    sync() {},
  };
}

// ── Displays ─────────────────────────────────────────────────────────────────

function displaysPage(cx: FeatureCtx) {
  const el = h("div", { class: `${P}list` });
  const bars = new Map<string, HTMLElement>();
  const nums = new Map<string, HTMLElement>();
  let shown = false;

  const put = (id: string, level: number) => {
    const bar = bars.get(id);
    // A slider being dragged is not moved under the pointer.
    if (bar && !bar.classList.contains("dragging")) setMeter(bar, level);
    const n = nums.get(id);
    if (n) n.textContent = `${Math.round(level * 100)}%`;
  };

  const hint = (d: DisplayRow) =>
    d.method === "none" ? cx.t("Dimming only") : d.method === "ddc-write" ? cx.t("Current level unknown") : d.internal ? cx.t("Built-in") : cx.t("External");

  async function load() {
    const list = await cx.call<DisplayRow[]>("displays");
    bars.clear();
    nums.clear();
    if (!list || !list.length) {
      el.replaceChildren(h("p", { class: `${P}hint`, text: cx.t("No display found.") }));
      return;
    }
    el.replaceChildren(
      ...list.map((d) => {
        // A drag writes at most every 120 ms; letting go writes the last value.
        let sent = 0;
        let pending: number | undefined;
        const send = (v: number) => {
          sent = Date.now();
          void cx.call<{ id: string; level: number }[]>("set", { id: d.id, level: v }).then((moved) =>
            moved?.forEach((m) => m.id !== d.id && put(m.id, m.level)),
          );
        };
        const bar = slider(d.level, {
          label: d.name,
          onInput: (v) => {
            put(d.id, v);
            clearTimeout(pending);
            if (Date.now() - sent >= 120) send(v);
            else pending = window.setTimeout(() => send(v), 120);
          },
          onChange: (v) => {
            clearTimeout(pending);
            put(d.id, v);
            send(v);
          },
          onDrag: (on) => {
            if (shown) (on ? holds.set(d.id, cx.island.hold()) : holds.get(d.id)?.());
          },
        });
        bars.set(d.id, bar);
        const num = h("b", { text: `${Math.round(d.level * 100)}%` });
        nums.set(d.id, num);
        return h("div", { class: `${P}disp` },
          h("span", { class: `${P}ico` }, icon(d.internal ? "sun" : "monitor", 16)),
          h("span", { class: `${P}name` }, h("b", { text: d.name }), h("small", { text: hint(d) })),
          bar,
          num,
        );
      }),
    );
  }
  const holds = new Map<string, () => void>();

  const offs = [
    cx.on<{ id: string; level: number }>("brightness", (e) => shown && put(e.id, e.level)),
    cx.on("displays", () => shown && void load()),
  ];
  void offs;
  return {
    el,
    show() {
      shown = true;
      void load();
    },
    hide() {
      shown = false;
      holds.forEach((release) => release());
      holds.clear();
    },
    sync() {},
  };
}

// ── Wallpaper ────────────────────────────────────────────────────────────────

const PER_PAGE = 12;

function wallpaperPage(cx: FeatureCtx) {
  const el = h("div", { class: `${P}wall` });
  let lists: { system: Pic[]; own: Pic[]; folder: string } = { system: [], own: [], folder: "" };
  let tab: "system" | "own" = "system";
  let page = 0;
  let turn = 0;
  const thumbs = new Map<string, string>();

  async function thumbs_(box: HTMLElement[], pics: Pic[], mine: number) {
    // One at a time, and only while this page is still the one on show.
    for (let i = 0; i < pics.length && mine === turn; i++) {
      let url = thumbs.get(pics[i].path);
      if (!url) {
        url = (await cx.call<string>("wallThumb", { path: pics[i].path })) ?? "";
        if (url) thumbs.set(pics[i].path, url);
      }
      if (url && mine === turn) box[i].append(h("img", { src: url, alt: "", draggable: "false" }));
    }
  }

  function draw() {
    turn++;
    const pics = lists[tab];
    const pages = Math.max(1, Math.ceil(pics.length / PER_PAGE));
    page = Math.min(page, pages - 1);
    const shownPics = pics.slice(page * PER_PAGE, (page + 1) * PER_PAGE);
    const cells = shownPics.map((p) =>
      h("button", {
        class: `${P}pic`,
        title: p.name,
        onclick: (e: Event) => {
          const b = e.currentTarget as HTMLElement;
          void cx.call("wallSet", { path: p.path }).then((r) => {
            if (r !== null) {
              b.classList.add("done");
              setTimeout(() => b.classList.remove("done"), 1500);
            }
          });
        },
      }));
    el.replaceChildren(
      h("div", { class: `${P}wall-bar` },
        h("div", { class: `${P}chips` },
          ...(["system", "own"] as const).map((k) =>
            h("button", { class: `tab${tab === k ? " on" : ""}`, onclick: () => { tab = k; page = 0; draw(); } }, h("span", { text: cx.t(k === "system" ? "Windows" : "My pictures") })))),
        h("span", { class: `${P}pager` },
          btn("‹", "ghost", () => { page = Math.max(0, page - 1); draw(); }),
          h("small", { text: `${page + 1} / ${pages}` }),
          btn("›", "ghost", () => { page = Math.min(pages - 1, page + 1); draw(); })),
      ),
      pics.length
        ? h("div", { class: `${P}grid` }, ...cells)
        : h("p", { class: `${P}hint`, text: tab === "own" ? cx.t("No pictures in {folder}.", { folder: lists.folder }) : cx.t("No pictures found.") }),
    );
    void thumbs_(cells, shownPics, turn);
  }

  return {
    el,
    show() {
      void cx.call<typeof lists>("wallpapers").then((l) => {
        if (l) lists = l;
        draw();
      });
    },
    hide() {
      turn++;
    },
    sync() {},
  };
}
