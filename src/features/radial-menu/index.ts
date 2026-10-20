// Radial menu — the page side of the feature (docs/ARCHITECTURE.md §2.3).
// Nothing runs at import. The wheel itself is a window (page "wheel") that only
// draws what Rust tells it (crates/radial-menu: the pointer, the keys and the
// slices all live there); this file also holds the settings page, where the
// wheels and their slices are edited, and the notice for a slice that failed.

import "./style.css";
import { defineFeature, type FeatureCtx, type Value } from "../../core/feature";
import { btn, field, iconBtn, row, tab, toggle } from "../../ui/components";
import { h } from "../../views/dom";
import { mountWheel, type Item, type NowPlaying, type Show } from "./wheel";

const P = "f-radial-menu-";
const MAX_SLICES = 12;
/** One shortcut per wheel is declared in the manifest. */
const MAX_WHEELS = 3;

interface Wheel {
  name: string;
  items: Item[];
}

const blank = (kind = "app"): Item => ({ kind, name: "", target: kind === "media" ? "playPause" : "", icon: "", items: [] });

const items = (v: unknown): Item[] =>
  (Array.isArray(v) ? (v as Partial<Item>[]) : []).map((i) => ({
    kind: String(i.kind ?? ""),
    name: String(i.name ?? ""),
    target: String(i.target ?? ""),
    icon: String(i.icon ?? ""),
    items: items(i.items),
  }));

const wheels = (s: Value): Wheel[] =>
  (Array.isArray(s.profiles) ? (s.profiles as Partial<Wheel>[]) : []).map((w) => ({ name: String(w.name ?? ""), items: items(w.items) }));

/** A `.field` around a native select. */
function select(opts: [string, string][], value: string, on: (v: string) => void): HTMLLabelElement {
  const sel = h("select");
  for (const [v, label] of opts) sel.append(h("option", { value: v, text: label }));
  sel.value = value;
  sel.addEventListener("change", () => on(sel.value));
  return h("label", { class: "field" }, sel);
}

// ── The list editor ───────────────────────────────────────────────────────────

function listEditor(cx: FeatureCtx, list: Item[], depth: number, save: (now?: boolean) => void, apps: () => Promise<{ id: string; name: string }[]>): HTMLElement {
  const box = h("div", { class: `${P}list` });
  const kinds = (): [string, string][] => {
    const all: [string, string][] = [
      ["app", cx.t("App")],
      ["file", cx.t("File or folder")],
      ["link", cx.t("Link")],
      ["media", cx.t("Media")],
      ["nowplaying", cx.t("Now playing")],
      ["shortcut", cx.t("Shortcut keys")],
      ["command", cx.t("Command or script")],
    ];
    // One level of submenus, as upstream.
    if (depth === 0) all.push(["submenu", cx.t("Group")]);
    return all;
  };

  const target = (it: Item): Node | null => {
    switch (it.kind) {
      case "app": {
        const sel = select([["", cx.t("Choose an app")]], it.target, () => {});
        const input = sel.querySelector("select")!;
        void apps().then((list) => {
          const known = list.some((a) => a.id === it.target);
          const opts: [string, string][] = [["", cx.t("Choose an app")], ...(it.target && !known ? [[it.target, it.name || it.target] as [string, string]] : []), ...list.map((a) => [a.id, a.name] as [string, string])];
          input.replaceChildren(...opts.map(([v, l]) => h("option", { value: v, text: l })));
          input.value = it.target;
        });
        input.addEventListener("change", () => {
          it.target = input.value;
          const picked = input.selectedOptions[0]?.textContent ?? "";
          if (!it.name && it.target) it.name = picked;
          save(true);
          draw();
        });
        sel.classList.add(`${P}target`);
        sel.classList.toggle("invalid", !it.target);
        return sel;
      }
      case "media": {
        const sel = select([["playPause", cx.t("Play or pause")], ["next", cx.t("Next track")], ["prev", cx.t("Previous track")]], it.target, (v) => {
          it.target = v;
          save(true);
        });
        sel.classList.add(`${P}target`);
        return sel;
      }
      case "nowplaying":
      case "submenu":
        return null;
      default: {
        const f = field({
          value: it.target,
          placeholder: it.kind === "file" ? cx.t("Path, folder or shell: location") : it.kind === "link" ? cx.t("Address") : it.kind === "command" ? cx.t("Command line or script path, run when you pick the slice") : cx.t("Keys, such as Ctrl+Shift+S"),
          onInput: (v) => {
            it.target = v;
            f.classList.toggle("invalid", !v.trim());
            save();
          },
        });
        f.classList.add(`${P}target`);
        f.classList.toggle("invalid", !it.target.trim());
        return f;
      }
    }
  };

  const move = (i: number, by: number) => {
    const j = i + by;
    if (j < 0 || j >= list.length) return;
    [list[i], list[j]] = [list[j], list[i]];
    save(true);
    draw();
  };

  const draw = () => {
    box.replaceChildren();
    list.forEach((it, i) => {
      const line = h(
        "div",
        { class: `${P}line` },
        h("span", { class: `${P}num`, text: String(i + 1) }),
        select(kinds(), it.kind, (v) => {
          it.kind = v;
          it.target = blank(v).target;
          if (v !== "submenu") it.items = [];
          save(true);
          draw();
        }),
        field({ value: it.name, placeholder: cx.t("Name (optional)"), onInput: (v) => ((it.name = v), save()) }),
        target(it),
        iconBtn("chevu", cx.t("Move up"), () => move(i, -1)),
        iconBtn("chevd", cx.t("Move down"), () => move(i, 1)),
        iconBtn("trash", cx.t("Remove"), () => {
          list.splice(i, 1);
          save(true);
          draw();
        }),
      );
      line.children[1].classList.add(`${P}kind`);
      line.children[2].classList.add(`${P}name`);
      const card = h("div", { class: `${P}item` }, line);
      if (it.kind === "submenu") card.append(h("div", { class: `${P}sub` }, listEditor(cx, it.items, depth + 1, save, apps)));
      box.append(card);
    });
    if (!list.length) box.append(h("div", { class: `${P}empty`, text: depth ? cx.t("An empty group is left out of the wheel.") : cx.t("No slices yet.") }));
    if (list.length < MAX_SLICES) {
      box.append(
        h("div", { class: `${P}add` }, btn(cx.t("Add a slice"), "secondary", () => {
          list.push(blank());
          save(true);
          draw();
        })),
      );
    }
  };
  draw();
  return box;
}

// ── Settings ──────────────────────────────────────────────────────────────────

function settingsPage(cx: FeatureCtx): HTMLElement {
  const page = h("div", { class: `${P}page` });
  let s: Value = {};
  let list: Wheel[] = [];
  let at = 0;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let installed: Promise<{ id: string; name: string }[]> | undefined;
  const apps = () => (installed ??= cx.call<{ id: string; name: string }[]>("apps").then((a) => a ?? []));

  // The settings window is the only editor: what is typed is saved after a pause, structure at once.
  const save = (now = false) => {
    clearTimeout(timer);
    const go = () => void cx.settings.patch({ profiles: list });
    if (now) go();
    else timer = setTimeout(go, 400);
  };

  const draw = () => {
    page.replaceChildren();
    if (!list.length) list = [{ name: "", items: [] }];
    at = Math.min(at, list.length - 1);
    const current = list[at];
    const tabs = h("div", { class: `${P}tabs` });
    list.forEach((w, i) => {
      const t = tab("grid", w.name || cx.t("Wheel {n}", { n: i + 1 }), () => {
        at = i;
        draw();
      });
      t.classList.toggle("on", i === at);
      tabs.append(t);
    });
    if (list.length < MAX_WHEELS) {
      tabs.append(
        iconBtn("plus", cx.t("Add a wheel"), () => {
          list.push({ name: "", items: [] });
          at = list.length - 1;
          save(true);
          draw();
        }),
      );
    }
    let sure = false;
    const remove = btn(cx.t("Remove this wheel"), "ghost", () => {
      if (!sure) {
        sure = true;
        remove.querySelector("span")!.textContent = cx.t("Click again to remove");
        return;
      }
      list.splice(at, 1);
      at = Math.max(0, at - 1);
      save(true);
      draw();
    });
    page.append(
      row(cx.t("Open at the pointer"), toggle(s.atPointer !== false, (on) => void cx.settings.patch({ atPointer: on })), cx.t("Off opens it in the middle of the screen.")),
      row(
        cx.t("Activation"),
        select([["pressOrHold", cx.t("Hold or tap")], ["press", cx.t("Tap")], ["hold", cx.t("Hold")]], String(s.mode ?? "pressOrHold"), (v) => void cx.settings.patch({ mode: v })),
        cx.t("Hold the shortcut, point at a slice and let go to run it. A tap leaves the wheel open to click."),
      ),
      row(cx.t("Try it"), btn(cx.t("Open this wheel"), "secondary", () => void cx.call("try", { profile: at })), cx.t("Opens it at the pointer and keeps it open until you click or press Esc.")),
      tabs,
      h(
        "div",
        { class: `${P}bar` },
        field({ value: current.name, placeholder: cx.t("Wheel {n}", { n: at + 1 }), onInput: (v) => ((current.name = v), save()) }),
        list.length > 1 ? remove : null,
      ),
      h("p", { class: `${P}hint`, text: cx.t("Each wheel has its own shortcut: the first, second and third wheel in the list of shortcuts. A slice without a target is left out.") }),
      listEditor(cx, current.items, 0, save, apps),
    );
  };

  void cx.settings.get().then((v) => {
    s = v;
    list = wheels(v);
    draw();
  });
  return page;
}

export default defineFeature({
  id: "radial-menu",

  settings: settingsPage,

  windows: {
    wheel(root, cx) {
      const wheel = mountWheel(root, {
        t: cx.t,
        icon: (kind, target) => cx.call<string>("icon", { kind, target }),
        art: (rev) => cx.call<string>("art", { rev }),
      });
      cx.on<Show>("show", (p) => wheel.show(p));
      cx.on<{ index: number | null }>("hover", (p) => wheel.hover(p.index));
      cx.on("close", () => wheel.close());
      cx.on<NowPlaying | null>("nowplaying", (p) => wheel.nowPlaying(p));
      // The window may be built after the wheel was shown (the first summons).
      void cx.call<Show>("session").then((p) => p && wheel.show(p));
    },
  },

  start(cx) {
    return cx.on<{ message: string }>("failed", (e) =>
      cx.island.notice({ kind: "accessory", title: cx.t("The wheel could not run that"), detail: e.message, icon: "alert", tone: "warn" }),
    );
  },
});
