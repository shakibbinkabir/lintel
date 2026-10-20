// Window layout — the page side of the feature (docs/ARCHITECTURE.md §2.3).
// Nothing runs at import: declare surfaces here, begin work in `start`.
//
// A section of the island with every placement as a tile (the window that was
// in front stays in front: the island does not take the keyboard for it), the
// settings of the package's tools, and the two things a shortcut can only say
// on the strip: a placement that did not happen, a close that was held back.
// Windows are measured and moved by Rust (crates/window-layout).

import "./style.css";
import { Bridge } from "../../core/bridge";
import { defineFeature, type FeatureCtx, type SectionHost } from "../../core/feature";
import { N_ } from "../../i18n/i18n";
import { btn, card, field, iconBtn, kbd, menu, row, tile, toggle } from "../../ui/components";
import { h } from "../../views/dom";

const P = "f-window-layout-";

type S = Record<string, unknown>;

/** A placement that tiles, as Rust lists them: `share` is left, right, top and bottom in twelfths of the screen. */
interface Tile {
  id: string;
  name: string;
  share: number[];
}

interface Result {
  action: string;
  result: string;
}

// ── Icons: a screen with the part the window takes filled in ──────────────────

const SCREEN = "M4 5h16a1 1 0 0 1 1 1v12a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1V6a1 1 0 0 1 1-1z";

/** Drawn on the 24 grid as lines close enough to read as one filled block (the set's icons are strokes). */
function glyph([x0, x1, y0, y1]: number[]): string {
  const x = (n: number) => 5.2 + (13.6 * n) / 12;
  const y = (n: number) => 7.2 + (9.6 * n) / 12;
  const [top, bottom] = [y(y0) + 0.875, y(y1) - 0.875];
  const lines = Math.max(2, Math.ceil((bottom - top) / 0.9) + 1);
  let d = SCREEN;
  for (let i = 0; i < lines; i++) {
    d += `M${(x(x0) + 0.875).toFixed(2)} ${(top + ((bottom - top) * i) / (lines - 1)).toFixed(2)}H${(x(x1) - 0.875).toFixed(2)}`;
  }
  return d;
}

const LAYOUT = glyph([0, 6, 0, 12]);

/** The placements that are not a share of the screen, where they stand in the grid. */
const MAXIMIZE = { id: "maximize", name: N_("Maximize"), icon: glyph([0, 12, 0, 12]) };
const ALMOST = { id: "almost-maximize", name: N_("Almost maximize"), icon: glyph([1.5, 10.5, 1.5, 10.5]) };
const CENTER = { id: "center", name: N_("Center"), icon: glyph([3.5, 8.5, 3, 9]) };
const PREVIOUS = { id: "previous-display", name: N_("Previous display"), icon: `${SCREEN}M16 12H8M11 9l-3 3 3 3` };
const NEXT = { id: "next-display", name: N_("Next display"), icon: `${SCREEN}M8 12h8M13 9l3 3-3 3` };
const RESTORE = { id: "restore", name: N_("Put back"), icon: "replay" };
const PIN = { id: "pin", name: N_("Keep on top"), icon: "pin" };

/** What a placement that did not happen says. One that did says nothing: the window shows it. */
const SAID: Record<string, string> = {
  "no-window": N_("No window to arrange"),
  "nothing-to-restore": N_("Nothing to put back"),
  "one-display": N_("Only one display"),
  failed: N_("Window not moved"),
  pinned: N_("Kept above the other windows"),
  unpinned: N_("Let go"),
};
/** A result that is good news, not a refusal. */
const GOOD = ["pinned", "unpinned"];

// ── The section ───────────────────────────────────────────────────────────────

function section(cx: FeatureCtx): SectionHost {
  const t = cx.t;
  const grid = h("div", { class: `${P}grid` });
  const said = h("div", { class: `${P}said` });
  const hint = () => (said.textContent = t("Arranges the window in front"));
  const run = (id: string) =>
    void cx.call<Result>("apply", { action: id }).then((r) => {
      if (r && SAID[r.result]) said.textContent = t(SAID[r.result]);
      else hint();
    });
  void cx.call<Tile[]>("tiles").then((list) => {
    const tiles = (list ?? []).map((x) => ({ id: x.id, name: x.name, icon: glyph(x.share) }));
    // Halves and the sizes of the whole screen, corners and thirds, then the rest.
    const all = [...tiles.slice(0, 4), MAXIMIZE, ALMOST, CENTER, ...tiles.slice(4), PREVIOUS, NEXT, RESTORE, PIN];
    grid.replaceChildren(...all.map((a) => tile(a.icon, t(a.name), () => run(a.id))));
  });
  hint();
  return { el: h("div", { class: `${P}page` }, grid, said), show: hint, hide() {}, sync() {} };
}

// ── The virtual desktops ──────────────────────────────────────────────────────

interface Desktops {
  names: (string | null)[];
  current: number;
  on: boolean;
}

/** The strip of virtual desktops: click one, or the arrows, to go there. Off until it is turned on here. */
function desktops(cx: FeatureCtx): SectionHost {
  const t = cx.t;
  const strip = h("div", { class: `${P}desks` });
  const el = h("div", { class: `${P}page` }, strip);
  let state: Desktops | null = null;
  const go = (to: number) => {
    if (state && to >= 0 && to < state.names.length && to !== state.current) void cx.call("desktop", { to });
  };
  const draw = () => {
    if (!state) return;
    const d = state;
    if (!d.on) {
      strip.replaceChildren(
        h("span", { class: `${P}note`, text: t("Shows the virtual desktops of Windows by name, to switch between them.") }),
        btn(t("Turn on"), "primary", () => void cx.settings.patch({ desktopStrip: true }).then(() => void load())),
      );
      return;
    }
    strip.replaceChildren(
      ...d.names.map((name, i) => btn(name ?? t("Desktop {n}", { n: i + 1 }), i === d.current ? "primary" : "secondary", () => go(i))),
    );
  };
  const load = () => cx.call<Desktops>("desktops").then((d) => ((state = d), draw()));
  const key = (e: KeyboardEvent) => {
    if (e.key === "ArrowLeft" || e.key === "ArrowRight") go((state?.current ?? 0) + (e.key === "ArrowLeft" ? -1 : 1));
  };
  // The registry is read when the section is shown and after a switch, never in between.
  cx.on("desktop", () => void load());
  return {
    el,
    show() {
      window.addEventListener("keydown", key);
      void load();
    },
    hide() {
      window.removeEventListener("keydown", key);
    },
    sync() {},
  };
}

// ── Settings ──────────────────────────────────────────────────────────────────

/** How a key's stored name is shown. Key names are not translated. */
const KEY: Record<string, string> = { Left: "←", Right: "→", Up: "↑", Down: "↓", Super: "Win", PageUp: "Page Up", PageDown: "Page Down" };

/** A combination as keycaps: "Ctrl+Alt+Super+Left" → Ctrl Alt Win ←. */
const keysOf = (chord: string) => h("span", { class: `${P}keys` }, ...chord.split("+").map((k) => kbd(KEY[k] ?? k)));

const GAPS = [0, 8, 16, 32, 64, 128];

function settingsBody(cx: FeatureCtx): HTMLElement {
  const t = cx.t;
  const el = h("div", { class: `${P}settings` });
  const shortcuts = card([]);
  let s: S = {};

  /** Saves without drawing again (a switch has already moved). */
  const set = async (patch: S) => {
    Object.assign(s, patch);
    await cx.settings.patch(patch);
  };
  /** Saves and draws the page again (a list changed, or what a row is about). */
  const change = async (patch: S) => {
    await set(patch);
    draw();
  };

  const sw = (key: string) => toggle(s[key] === true, (on) => void set({ [key]: on }));
  const num = (key: string, min: number, max: number) => {
    const f = field({ type: "number", value: String(s[key] ?? min) });
    f.classList.add(`${P}num`);
    f.input.setAttribute("min", String(min));
    f.input.setAttribute("max", String(max));
    f.input.addEventListener("change", () => {
      const v = Math.max(min, Math.min(max, Math.round(Number(f.input.value) || 0)));
      f.input.value = String(v);
      void set({ [key]: v });
    });
    return f;
  };
  const pick = (key: string, options: [string | number, string][], then: (patch: S) => Promise<void> = set) => {
    const select = h("select", {}, ...options.map(([value, name]) => h("option", { value: String(value), text: name })));
    select.value = String(s[key] ?? options[0][0]);
    select.addEventListener("change", () => void then({ [key]: typeof options[0][0] === "number" ? Number(select.value) : select.value }));
    return h("label", { class: `field ${P}pick` }, select);
  };
  const gap = (key: string) => pick(key, GAPS.map((px) => [px, px === 0 ? t("None") : t("{px} px", { px })]));
  const edgeGap = (key: string) => pick(key, [[-1, t("Same as the screen gap")], ...GAPS.map((px): [number, string] => [px, px === 0 ? t("None") : t("{px} px", { px })])]);
  /** A guarded shortcut: its keys and its switch. */
  const guarded = (chord: string, key: string) => h("span", { class: `${P}ctl` }, keysOf(chord), sw(key));

  /** Program names as chips, with the open apps to pick from and a field for any other. */
  const appList = (key: string) => {
    const list = (Array.isArray(s[key]) ? (s[key] as unknown[]) : []).map(String);
    const add = (name: string) => {
      const exe = name.trim().toLowerCase();
      if (exe && !list.includes(exe)) void change({ [key]: [...list, exe] });
    };
    const other = field({ placeholder: "app.exe", onEnter: add });
    other.classList.add(`${P}app`);
    const more = iconBtn("plus", t("Add an app that is open"), () => {
      const at = more.getBoundingClientRect();
      void cx.call<string[]>("apps").then((apps) => {
        const open = (apps ?? []).filter((a) => !list.includes(a));
        if (open.length > 0) menu(open.map((a) => ({ label: a, onClick: () => add(a) })), { x: at.left, y: at.bottom + 4 });
      });
    }, "secondary");
    return h(
      "div",
      { class: `${P}chips` },
      ...list.map((exe) => h("span", { class: `${P}chip` }, exe, iconBtn("close", t("Remove"), () => void change({ [key]: list.filter((x) => x !== exe) })))),
      other,
      more,
    );
  };

  /** The shortcuts that have keys, and whether Windows gave them. Changed on the Shortcuts page. */
  const drawShortcuts = () =>
    void Promise.all([Bridge.featuresList(), Bridge.shortcutsStatus(), Bridge.boot()]).then(([features, report, boot]) => {
      const stored = boot?.settings.shortcuts ?? {};
      const rows = ((features ?? []).find((f) => f.id === cx.id)?.shortcuts ?? [])
        .map((sc) => ({ id: `${cx.id}:${sc.id}`, label: sc.label, binding: stored[`${cx.id}:${sc.id}`] ?? { keys: sc.default ?? "", enabled: true } }))
        .filter((x) => x.binding.enabled && x.binding.keys);
      shortcuts.replaceChildren(
        row(
          t("Shortcuts"),
          btn(t("Change shortcuts"), "secondary", () => (location.hash = "#/shortcuts")),
          t("An arrow gives a half. The arrow across it then gives a quarter, and up on the top half fills the screen. Every placement can have keys of its own."),
        ),
        ...rows.map((x) =>
          h(
            "div",
            { class: `${P}item` },
            h("span", { class: `${P}grow`, text: t(x.label) }),
            report?.actions.find((a) => a.id === x.id)?.status === "inUse" ? h("span", { class: `${P}tag`, text: t("In use by another app") }) : null,
            keysOf(x.binding.keys),
          ),
        ),
      );
    });

  const draw = () => {
    const hold = s.closeMode !== "double";
    el.replaceChildren(
      h("div", { class: "section-title", text: t("Placement") }),
      shortcuts,
      card([
        row(
          t("Left or right again changes the width"),
          sw("cycleThirds"),
          t("A half, then two thirds, a third and the half again. Off: the second press sends the window to the display on that side."),
        ),
        row(t("Gap between windows"), gap("windowGap")),
        row(t("Gap at the edges of the screen"), gap("screenGap")),
        row(t("Left edge gap"), edgeGap("screenGapLeft")),
        row(t("Top edge gap"), edgeGap("screenGapTop")),
        row(t("Right edge gap"), edgeGap("screenGapRight")),
        row(t("Bottom edge gap"), edgeGap("screenGapBottom")),
        row(t("Margin of Almost maximize, in percent"), num("margin", 0, 25), t("0 to 25, on every side.")),
        h(
          "div",
          { class: `${P}block` },
          h("b", { text: t("Apps to leave alone") }),
          h("span", { class: `${P}note`, text: t("The placement shortcuts do nothing while one of them is in front.") }),
          appList("ignoredApps"),
        ),
      ]),

      h("div", { class: "section-title", text: t("Dragging") }),
      card([
        row(
          t("Snap a window dragged by its title bar"),
          sw("edgeSnap"),
          t("Let go at a screen edge for a half, at a corner for a quarter, at the top for the whole screen. A window Lintel placed gets its old size back when you drag it away."),
        ),
        row(
          t("Move a window by dragging it from anywhere"),
          pick("dragModifier", [["off", t("Off")], ["alt", "Alt"], ["ctrl-alt", "Ctrl+Alt"], ["shift-alt", "Shift+Alt"]], change),
          t("Hold these keys and drag. It snaps like the title bar does. The click is not passed on to the app."),
        ),
        row(
          t("Hold for columns while dragging"),
          pick("zoneKey", [["off", t("Off")], ["shift", "Shift"], ["ctrl", "Ctrl"], ["alt", "Alt"]]),
          t("The display is split in two columns, three on an ultrawide, and letting go places the window in the column under the pointer."),
        ),
      ]),

      h("div", { class: "section-title", text: t("Virtual desktops") }),
      card([row(t("Show the desktops in the island"), sw("desktopStrip"), t("A section that lists the virtual desktops by the names given in Windows. It reads them only when it is shown."))]),

      h("div", { class: "section-title", text: t("Pointer") }),
      card([
        row(
          t("Focus follows the pointer"),
          sw("follow"),
          t("The window the pointer rests on comes to the front. Nothing is changed in Windows' own settings."),
        ),
        row(t("Rest before it comes forward, in milliseconds"), num("followDelay", 100, 3000), t("100 to 3000.")),
      ]),

      h("div", { class: "section-title", text: t("Close protection") }),
      card([
        row(t("Ask twice before a window closes"), guarded("Alt+F4", "closeAltF4")),
        row(
          t("Ask twice before a tab closes"),
          guarded("Ctrl+W", "closeCtrlW"),
          t("In every app, also where these keys mean something else, such as deleting a word in a terminal."),
        ),
        row(t("Confirm by"), pick("closeMode", [["hold", t("Holding the keys")], ["double", t("Pressing them twice")]], change)),
        hold
          ? row(t("Hold for, in milliseconds"), num("closeHoldMs", 250, 2000), t("250 to 2000. Letting go earlier, or Esc, cancels."))
          : row(t("Second press within, in milliseconds"), num("closeIntervalMs", 200, 1500), t("200 to 1500.")),
      ]),
      h("div", { class: `${P}note`, text: t("A window that runs as administrator cannot be moved, brought forward or guarded.") }),
    );
  };

  void cx.settings.get().then((value) => {
    s = { ...value };
    draw();
    drawShortcuts();
  });
  return el;
}

// ── The feature ───────────────────────────────────────────────────────────────

export default defineFeature({
  id: "window-layout",
  sections: [
    { id: "layout", title: "Layout", icon: LAYOUT, letter: "l", height: 320, build: section },
    { id: "desktops", title: "Desktops", icon: "grid", height: 160, keys: ["ArrowLeft", "ArrowRight"], build: desktops },
  ],
  settings: settingsBody,
  start: (cx) => {
    // A shortcut's placement that did not happen (a tile's is said in the section).
    const offResult = cx.on<Result>("result", (r) => {
      if (SAID[r.result]) cx.island.notice({ kind: "notification", title: cx.t(SAID[r.result]), icon: LAYOUT, tone: GOOD.includes(r.result) ? "ok" : "warn", section: "layout" });
    });
    // A guarded shortcut was held back: what confirms it.
    const offGuard = cx.on<{ state: string; keys: string; hold: boolean }>("guard", (g) => {
      if (g.state !== "asked") return;
      cx.island.notice({ kind: "notification", title: g.hold ? cx.t("Keep holding to close") : cx.t("Press again to close"), detail: g.keys, icon: "shield", tone: "warn" });
    });
    return () => {
      offResult();
      offGuard();
    };
  },
});
