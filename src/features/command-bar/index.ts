// Command bar — the page side of the feature (docs/ARCHITECTURE.md §2.3).
// Nothing runs at import: one window (`bar`) and a settings page; nothing in
// the island.
//
// The window is Rust's (crates/command-bar): it opens it on the shortcut, takes
// the keyboard and hands it back. This page draws the field, the filters and
// the list. It asks Rust for the rows of what is typed and gives it, to be
// ranked with its own, the rows other features offer: their `commands`, their
// shortcuts and tray items, their switches and their pages of Settings.
// While the bar is closed nothing here runs: no timer, only the listeners.

import "./style.css";
import { emit } from "@tauri-apps/api/event";
import { Bridge, onEvent } from "../../core/bridge";
import { defineFeature, type FeatureCtx } from "../../core/feature";
import { FEATURES, featureCtx, featureInfos, featureOn, loadFeatures } from "../../core/registry";
import { displayKeys, type Bindings } from "../../core/shortcuts";
import type { Settings } from "../../core/state";
import { btn, card, field, iconBtn, kbd, row, tab, toggle } from "../../ui/components";
import { icon } from "../../ui/icons";
import { clear, h } from "../../views/dom";

/** One row, as Rust ranks and returns it (crates/command-bar/src/rows.rs). */
interface Row {
  id: string;
  kind: string;
  title: string;
  subtitle?: string;
  keywords?: string;
  icon?: string;
  /** What to ask Rust's `icon` for: an app, a program or a file. */
  picture?: string;
  /** An emoji, drawn where the icon goes. */
  glyph?: string;
  /** Keys at the right end. */
  hint?: string;
  running?: boolean;
  /** Asked before the row runs. */
  confirm?: string;
  /** What Enter copies and Tab puts in the field; for a saved search, what was typed after its name. */
  copy?: string;
  learn?: boolean;
  section?: string;
}

type Runner = () => void | Promise<void>;

/** Icons only the bar needs (docs/ARCHITECTURE.md §7: 24×24, one stroked path). */
const OWN: Record<string, string> = {
  power: "M12 3.5v8M7.2 6.5a7.5 7.5 0 1 0 9.6 0",
  folder: "M4 6.5h5.5l2 2H20a1 1 0 0 1 1 1V18a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1V7.5a1 1 0 0 1 1-1z",
  calc: "M7 3.5h10a1 1 0 0 1 1 1v15a1 1 0 0 1-1 1H7a1 1 0 0 1-1-1v-15a1 1 0 0 1 1-1zM9 7.5h6M9 12h.01M12 12h.01M15 12h.01M9 16h.01M12 16h.01M15 16h.01",
  smile: "M12 3.5a8.5 8.5 0 1 0 0 17 8.5 8.5 0 0 0 0-17zM8.5 14a4 4 0 0 0 7 0M9 9.8h.01M15 9.8h.01",
};

/** The filters under the field: id, icon, English name. */
const CHIPS: [string, string, string][] = [
  ["all", "search", "All"],
  ["apps", "grid", "Apps"],
  ["windows", "monitor", "Windows"],
  ["actions", "sparkle", "Actions"],
  ["clipboard", "clipboard", "Clipboard"],
  ["emoji", "smile", "Emoji"],
  ["settings", "gear", "Settings"],
  ["files", "file", "Files"],
];

/** The pages of Settings that belong to no feature: id, English title, icon. */
const PAGES: [string, string, string][] = [
  ["general", "General", "gear"],
  ["island", "Island", "island"],
  ["mascot", "Mascot", "sparkle"],
  ["pills", "Pills", "grid"],
  ["sounds", "Sounds", "volume"],
  ["shortcuts", "Shortcuts", "keyboard"],
  ["features", "Features", "sliders"],
  ["privacy", "Privacy", "shield"],
];

const pause = (ms: number) => new Promise((done) => window.setTimeout(done, ms));

// ── The bar ───────────────────────────────────────────────────────────────────

function mountBar(root: HTMLElement, cx: FeatureCtx) {
  const t = cx.t;
  let rows: Row[] = [];
  let els: HTMLElement[] = [];
  let sel = 0;
  let chip = "all";
  /** Every question to Rust belongs to a turn; an answer that comes back in a later one is dropped. */
  let turn = 0;
  /** Other features' rows: the ones that are always there, the ones for what is typed, and files. */
  let fixed: Row[] = [];
  let offered: Row[] = [];
  let found: Row[] = [];
  /** What Enter does for a row that is not Rust's. */
  const runners = new Map<string, Runner>();
  const pictures = new Map<string, string>();
  /** A row waits for its confirmation; the list shows a row's actions. */
  let asking: Row | null = null;
  let acting: Row | null = null;
  let folders = "";
  /** The bar is on screen. Closed, nothing is asked of the other features. */
  let up = false;
  let bindings: Bindings = {};
  /** Where the pointer last was ("" until it is seen): a list drawn under a pointer at rest does not move the selection. */
  let pointer = "";

  const search = field({
    icon: "search",
    placeholder: t("Search apps, windows and commands"),
    onInput: () => {
      asking = null;
      acting = null;
      void refresh();
    },
  });
  const input = search.input as HTMLInputElement;
  const chips = h("div", { class: "f-command-bar-chips" });
  const list = h("div", { class: "list f-command-bar-rows", role: "listbox" });
  const foot = h("div", { class: "f-command-bar-foot" });
  const panel = h("div", { class: "win f-command-bar-panel" }, search, chips, list, foot);
  const stage = h("div", { class: "f-command-bar-stage" }, panel);
  root.append(stage);

  // A click beside the panel closes the bar; a click inside never takes the text cursor out of the field.
  stage.addEventListener("mousedown", (e) => {
    if (e.target === stage) void cx.call("close");
    else if (e.target !== input) e.preventDefault();
  });

  // ── Rows of other features ──────────────────────────────────────────────────

  /** A feature's shortcut or tray item, as if it had been pressed: its page hears the event, its Rust half gets the call. */
  const press = async (feature: string, what: "shortcut" | "tray", id: string) => {
    await cx.call("close");
    await emit(`${feature}:${what}`, { id });
    await Bridge.featureCall(feature, what, { id });
  };

  const openPage = async (page: string) => {
    // Settings comes up while the bar is still in front, then the bar goes.
    await cx.call("settings-page", { page });
    await Bridge.openSettingsWindow();
    await cx.call("close", { back: false });
  };

  function fixedRows(): Row[] {
    const out: Row[] = [];
    const add = (r: Row, run: Runner) => {
      out.push(r);
      runners.set(r.id, run);
    };
    const page = (id: string, name: string, mark: string) =>
      add({ id: `set:${id}`, kind: "settings", title: t(name), subtitle: t("{app} settings"), icon: mark, keywords: `${name} settings preferences`, learn: true }, () => openPage(id));
    for (const f of featureInfos()) {
      const mark = FEATURES.find((m) => m.id === f.id)?.sections?.[0]?.icon ?? "sparkle";
      const name = t(f.name);
      // Every feature has a page of Settings, named `f-<id>` there (src/settings/main.ts).
      page(`f-${f.id}`, f.name, mark);
      if (f.id === cx.id) continue;
      if (f.running) {
        for (const s of f.shortcuts) {
          const bound = bindings[`${f.id}:${s.id}`];
          const keys = bound ? (bound.enabled ? bound.keys : "") : (s.default ?? "");
          add({ id: `act:${f.id}:${s.id}`, kind: "action", title: t(s.label), subtitle: name, icon: mark, hint: keys && displayKeys(keys), keywords: `${s.label} ${f.name}`, learn: true }, () => press(f.id, "shortcut", s.id));
        }
        for (const item of f.tray) {
          add({ id: `tray:${f.id}:${item.id}`, kind: "action", title: t(item.label), subtitle: name, icon: mark, keywords: `${item.label} ${f.name}`, learn: true }, () => press(f.id, "tray", item.id));
        }
      }
      add(
        { id: `toggle:${f.id}`, kind: "action", title: f.on ? t("Turn off {name}", { name }) : t("Turn on {name}", { name }), subtitle: t("Feature"), icon: "sliders", keywords: `${f.on ? "turn off disable" : "turn on enable"} ${f.name}`, learn: true },
        async () => {
          await cx.call("close");
          await Bridge.featureSetEnabled(f.id, !f.on).catch((err) => console.error(`[command bar] ${f.id} was not switched`, err));
        },
      );
    }
    for (const [id, name, mark] of PAGES) page(id, name, mark);
    return out;
  }

  /** What the other features offer for this text (their `commands`). */
  async function offer(text: string): Promise<Row[]> {
    const out: Row[] = [];
    const theirs = FEATURES.filter((m) => m.commands && m.id !== cx.id && featureOn(m.id));
    await Promise.all(
      theirs.map(async (m) => {
        try {
          const its = featureCtx(m.id);
          const name = t(featureInfos().find((f) => f.id === m.id)?.name ?? m.id);
          for (const r of await m.commands!(text, its)) {
            out.push({ id: r.id, kind: m.id, title: r.title, subtitle: r.subtitle ?? name, icon: r.icon ?? "sparkle", keywords: (r.keywords ?? []).join(" ") });
            // The keyboard goes back first: such a row may paste or type into the app that had it.
            runners.set(r.id, async () => {
              await cx.call("close");
              await r.run(its);
            });
          }
        } catch (err) {
          console.error(`[command bar] ${m.id}: its rows failed`, err);
        }
      }),
    );
    return out;
  }

  // ── The list ────────────────────────────────────────────────────────────────

  function rowEl(r: Row, at: number): HTMLElement {
    let mark: Element = r.glyph ? h("i", { class: "f-command-bar-glyph", text: r.glyph }) : icon(OWN[r.icon ?? ""] ?? (r.icon || "sparkle"), 16);
    const el = h(
      "button",
      { class: `list-item f-command-bar-row${r.kind === "answer" ? " f-command-bar-answer" : ""}`, role: "option" },
      mark,
      h("span", { text: r.title }),
      r.running ? h("i", { class: "f-command-bar-live", title: t("Running") }) : null,
      r.subtitle ? h("small", { text: r.subtitle }) : null,
      r.hint ? kbd(r.hint) : null,
    );
    if (r.picture) {
      const key = r.picture;
      const put = (url: string) => {
        const img = h("img", { class: "f-command-bar-pic", src: url, alt: "" });
        mark.replaceWith(img);
        mark = img;
      };
      const have = pictures.get(key);
      if (have) put(have);
      else {
        void cx.call<string>("icon", { picture: key }).then((url) => {
          if (!url) return;
          if (pictures.size > 400) pictures.clear();
          pictures.set(key, url);
          if (el.isConnected) put(url);
        });
      }
    }
    el.addEventListener("click", () => {
      select(at);
      void run(r);
    });
    el.addEventListener("mousemove", (e) => {
      const now = `${e.screenX},${e.screenY}`;
      const moved = pointer !== "" && now !== pointer;
      pointer = now;
      if (moved && sel !== at) select(at);
    });
    el.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      select(at);
      void actions(r);
    });
    return el;
  }

  function show(next: Row[], keep: boolean) {
    const was = keep ? rows[sel]?.id : undefined;
    rows = next;
    els = [];
    clear(list);
    let heading = "";
    if (acting) list.append(h("div", { class: "section-title", text: acting.title }));
    rows.forEach((r, at) => {
      if (r.section && r.section !== heading) {
        heading = r.section;
        list.append(h("div", { class: "section-title", text: heading }));
      }
      const el = rowEl(r, at);
      els.push(el);
      list.append(el);
    });
    if (rows.length === 0) {
      list.append(h("div", { class: "f-command-bar-empty", text: input.value.trim() ? t("Nothing matches.") : t("Type to search apps, windows, commands and settings.") }));
    }
    const at = was ? rows.findIndex((r) => r.id === was) : -1;
    select(Math.max(0, at));
  }

  function select(at: number) {
    sel = at;
    els.forEach((el, i) => {
      el.classList.toggle("on", i === sel);
      el.setAttribute("aria-selected", String(i === sel));
    });
    // The first row scrolls its heading into view with it.
    if (sel === 0) list.scrollTop = 0;
    else els[sel]?.scrollIntoView({ block: "nearest" });
    drawFoot();
  }

  /** What Enter does to the row, in a word. */
  function verb(r: Row | undefined): string {
    if (!r || acting) return t("Run");
    if (r.kind === "answer" || r.kind === "info") return t("Copy");
    if (r.kind === "emoji") return t("Type");
    if (r.kind === "window") return t("Switch to");
    if (r.kind === "app" || r.kind === "file" || r.kind === "link" || r.kind === "settings") return t("Open");
    return t("Run");
  }

  function drawFoot() {
    clear(foot);
    foot.classList.toggle("f-command-bar-asking", asking != null);
    const gap = () => h("span", { class: "f-command-bar-gap" });
    if (asking) {
      const waiting = asking;
      const cancel = btn(t("Cancel"), "ghost", () => {
        asking = null;
        drawFoot();
      }, "Esc");
      const confirm = btn(t("Confirm"), "danger", () => void run(waiting, true), "↵");
      cancel.classList.add("sm");
      confirm.classList.add("sm");
      foot.append(icon("alert", 14), h("span", { text: waiting.confirm ?? "" }), gap(), cancel, confirm);
      return;
    }
    const hint = (keys: string, what: string) => h("span", { class: "f-command-bar-hint" }, kbd(keys), h("span", { text: what }));
    foot.append(hint("↵", verb(rows[sel])));
    if (!acting) foot.append(hint("Tab", t("Complete")), hint("Ctrl+K", t("Actions")));
    foot.append(gap(), hint("Esc", acting || input.value || chip !== "all" ? t("Back") : t("Close")));
  }

  function drawChips() {
    clear(chips);
    for (const [id, mark, name] of shownChips()) {
      const el = tab(OWN[mark] ?? mark, t(name), () => setChip(id));
      el.classList.toggle("on", id === chip);
      chips.append(el);
    }
  }

  const shownChips = () => CHIPS.filter(([id]) => (id !== "clipboard" || featureOn("clipboard")) && (id !== "files" || folders.trim() !== ""));

  function setChip(id: string) {
    chip = id;
    asking = null;
    acting = null;
    drawChips();
    void refresh();
  }

  /** Asks for the rows of what is typed: at once with what is known, again when other features and the file search have answered. */
  async function refresh(keep = false) {
    const mine = ++turn;
    const text = input.value;
    const ask = async (keeping: boolean) => {
      const got = await cx.call<Row[]>("search", { text, chip, extras: [...fixed, ...offered, ...found] });
      if (mine === turn && got && !acting) show(got, keeping);
    };
    await ask(keep);
    if (!up) return;
    const [had, more] = [offered.length, await offer(text)];
    if (mine !== turn) return;
    offered = more;
    if (had || more.length) await ask(true);
    if (!folders.trim() || text.trim().length < 2) {
      found = [];
      return;
    }
    // Files wait for a pause in the typing: the folders are read each time.
    await pause(120);
    if (mine !== turn) return;
    const files = (await cx.call<Row[]>("files", { text })) ?? [];
    if (mine !== turn) return;
    const before = found.length;
    found = files;
    if (before || files.length) await ask(true);
  }

  // ── Running a row ───────────────────────────────────────────────────────────

  /** A row is being run: a second Enter waits for the next opening. */
  let busy = false;

  async function run(r: Row | undefined, sure = false) {
    if (!r || busy) return;
    busy = true;
    try {
      await carryOut(r, sure);
    } finally {
      busy = false;
    }
  }

  async function carryOut(r: Row, sure: boolean) {
    if (acting) {
      // The list shows the actions of a row: `r` is one of them.
      const target = acting;
      acting = null;
      await cx.call("act", { id: target.id, action: r.id });
      void refresh(true);
      return;
    }
    if (r.confirm && !sure) {
      asking = r;
      drawFoot();
      return;
    }
    asking = null;
    const own = runners.get(r.id);
    if (own) {
      if (r.learn) void cx.call("used", { id: r.id });
      await own();
      return;
    }
    // A saved search called by its name alone: the name goes in the field, to be completed.
    if (r.id.startsWith("link:") && !r.copy && r.subtitle?.includes("…")) {
      input.value = `${r.title} `;
      void refresh();
      return;
    }
    const done = await cx.call("run", { id: r.id, sure, copy: r.copy ?? "", argument: r.copy ?? "", text: input.value });
    // It did not run (a window that closed meanwhile): the list was out of date.
    if (!done) void refresh(true);
  }

  /** Ctrl+K, or a right click: the list shows what else can be done with the row. */
  async function actions(r: Row | undefined) {
    if (!r || acting) return;
    const items = await cx.call<{ id: string; label: string; icon: string }[]>("actions", { id: r.id, learn: r.learn === true });
    if (!items?.length) return;
    turn++;
    asking = null;
    acting = r;
    show(items.map((a) => ({ id: a.id, kind: "action", title: a.label, icon: a.icon })), false);
  }

  /** Esc steps back one level: the question, the actions, the text, the filter, then the bar itself. */
  function back() {
    if (asking) {
      asking = null;
      drawFoot();
    } else if (acting) {
      acting = null;
      void refresh(true);
    } else if (input.value) {
      input.value = "";
      void refresh();
    } else if (chip !== "all") {
      setChip("all");
    } else {
      void cx.call("close");
    }
  }

  window.addEventListener("keydown", (e) => {
    // A key that ends a composition (Chinese, Japanese) belongs to the input method.
    if (e.isComposing || e.keyCode === 229) return;
    const ctrl = e.ctrlKey && !e.altKey && !e.metaKey;
    const done = () => e.preventDefault();
    if (e.key === "Escape") {
      done();
      back();
    } else if (e.key === "Enter") {
      done();
      void run(asking ?? rows[sel], asking != null);
    } else if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      done();
      if (rows.length === 0) return;
      asking = null;
      select((sel + (e.key === "ArrowDown" ? 1 : rows.length - 1)) % rows.length);
    } else if ((e.key === "ArrowLeft" || e.key === "ArrowRight") && !input.value && !acting) {
      // With nothing typed the arrows walk the filters.
      done();
      const all = shownChips().map(([id]) => id);
      const at = Math.max(0, all.indexOf(chip)) + (e.key === "ArrowRight" ? 1 : all.length - 1);
      setChip(all[at % all.length]);
    } else if (e.key === "Tab") {
      done();
      const r = rows[sel];
      if (!r || acting) return;
      input.value = r.kind === "answer" ? (r.copy ?? r.title) : r.title;
      void refresh();
    } else if (ctrl && e.code === "KeyK") {
      done();
      void actions(rows[sel]);
    } else if (ctrl && /^Digit[1-9]$/.test(e.code)) {
      done();
      void run(rows[Number(e.code.slice(5)) - 1]);
    } else if (ctrl && e.key === ",") {
      done();
      void openPage(`f-${cx.id}`);
    }
  });
  window.addEventListener("focus", () => input.focus());

  // ── Opening and closing (Rust says when) ────────────────────────────────────

  async function opened() {
    up = true;
    pointer = "";
    input.value = "";
    chip = "all";
    asking = null;
    acting = null;
    input.focus();
    // The panel comes in as content does everywhere (components.css).
    panel.classList.remove("fx-in");
    void panel.offsetWidth;
    panel.classList.add("fx-in");
    void refresh();
    folders = String((await cx.settings.get()).fileFolders ?? "");
    await loadFeatures();
    runners.clear();
    offered = [];
    found = [];
    fixed = fixedRows();
    drawChips();
    await refresh(true);
  }

  cx.on("open", () => void opened());
  cx.on("refresh", () => void refresh(true));
  // What was typed does not outlive the bar; the list of an empty bar is ready for the next opening.
  cx.on("closed", () => {
    up = false;
    input.value = "";
    chip = "all";
    asking = null;
    acting = null;
    drawChips();
    void refresh();
  });
  void onEvent<Settings>("settings-changed", (s) => (bindings = s.shortcuts ?? {}));
  void Bridge.boot().then((boot) => {
    bindings = boot?.settings.shortcuts ?? {};
    // Built by the first opening: the bar is already on screen.
    void opened();
  });
  drawChips();
  drawFoot();
}

// ── The bar beside selected text ──────────────────────────────────────────────

/** Upper case, then lower case, then each word capitalised, and round again. */
function nextCase(text: string): string {
  const upper = text.toUpperCase();
  const lower = text.toLowerCase();
  if (text !== upper) return upper;
  if (text !== lower) return lower;
  return lower.replace(/(^|\s)(\S)/g, (_, space: string, letter: string) => space + letter.toUpperCase());
}

/** Never takes focus (Rust shows it that way): a click acts on the text Rust read and the bar goes. */
function mountSelection(root: HTMLElement, cx: FeatureCtx) {
  const t = cx.t;
  let text = "";
  const done = (note?: string) => {
    if (note) bar.replaceChildren(h("span", { class: "f-command-bar-sel-note", text: note }));
    window.setTimeout(() => void cx.call("selection-hide"), note ? 700 : 0);
  };
  const copy = async (what: string, note: string) => {
    await cx.call("selection-copy", { text: what });
    done(note);
  };
  const buttons = [
    iconBtn("copy", t("Copy"), () => void copy(text, t("Copied"))),
    iconBtn("link", t("Clean link"), () => {
      void Bridge.featureCall<{ url: string; removed: string[] } | null>("clipboard", "clean", { text: text.trim() })
        .catch(() => null)
        .then((c) => (c ? copy(c.url, c.removed.length ? t("Link copied, clean") : t("Nothing to clean")) : done(t("Not a link"))));
    }),
    iconBtn("text", t("Change case"), () => void copy(nextCase(text), t("Copied"))),
    iconBtn("shelf", t("Keep on the shelf"), () => {
      void Bridge.featureCall("shelf", "add", { text })
        .then(() => done(t("Kept")))
        .catch(() => done(t("The shelf is off")));
    }),
  ];
  const bar = h("div", { class: "win f-command-bar-sel" }, ...buttons);
  bar.addEventListener("mousedown", (e) => e.preventDefault());
  root.append(bar);
  const set = (next: string) => {
    text = next;
    bar.replaceChildren(...buttons);
  };
  cx.on<{ text: string }>("selection", (e) => set(e.text));
  void cx.call<string>("selection-get").then((now) => set(now ?? ""));
}

// ── Settings ──────────────────────────────────────────────────────────────────

function settingsBody(cx: FeatureCtx): HTMLElement {
  const t = cx.t;
  const el = h("div", { class: "f-command-bar-settings" });
  void cx.settings.get().then((s) => {
    const lines = (key: string, example: string) => {
      const f = field({ value: String(s[key] ?? ""), placeholder: example, multiline: true });
      f.input.addEventListener("change", () => void cx.settings.patch({ [key]: f.input.value }));
      return f;
    };
    const note = (text: string) => h("p", { class: "f-command-bar-note", text });
    const forget = btn(t("Forget"), "secondary", () => {
      void cx.call("forget-all").then(() => (forget.firstElementChild!.textContent = t("Forgotten")));
    });
    el.replaceChildren(
      note(t("Opens over the app you are in and gives the keyboard back when it closes. Change its keys on the Shortcuts page.")),
      h("div", { class: "section-title", text: t("Links") }),
      card([
        note(t("One per line: a name, an equals sign, then a web address or a folder. With {query} in an address, what you type after the name goes in its place.")),
        lines("links", "gh = https://github.com/search?q={query}"),
      ]),
      h("div", { class: "section-title", text: t("Files") }),
      card([
        note(t("Folders to look for files in, one per line. ~ is your home folder. Nothing is indexed: the folders are read when you search.")),
        lines("fileFolders", "~\\Documents"),
      ]),
      h("div", { class: "section-title", text: t("Selected text") }),
      card([
        row(
          t("Show a bar beside selected text"),
          toggle(s.selectionBar === true, (on) => void cx.settings.patch({ selectionBar: on })),
          t("After you select text in another app, a small bar offers to copy it, clean a link, change its case or keep it on the shelf. Nothing is read in password fields, and nothing happens until you click."),
        ),
      ]),
      h("div", { class: "section-title", text: t("What you use most") }),
      card([row(t("Forget what I use most"), forget, t("The bar puts what you run often first. Pinned rows stay pinned."))]),
    );
  });
  return el;
}

// ── The feature ───────────────────────────────────────────────────────────────

export default defineFeature({
  id: "command-bar",
  settings: settingsBody,
  windows: { bar: mountBar, selection: mountSelection },
});
