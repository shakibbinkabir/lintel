// Shelf — the page side of the feature (docs/ARCHITECTURE.md §2.3).
// Nothing runs at import: declare surfaces here, begin work in `start`.
//
// One section: what is parked on the shelf as a row of cells, and a row of
// actions under it. While the section is on screen the whole island is its
// drop zone (files, text, links); a cell is dragged back out through Rust (an
// OLE drag, so Explorer and browsers take it). The items live in Rust, which
// also looks at the files: every answer here arrives later than its question,
// and one that comes for an item that has gone meanwhile is dropped.
//
// The keys (`KEYS`) work with the pointer over the island and no click
// (docs/ARCHITECTURE.md §4.1). One window: the preview that Space opens.

import "./style.css";
import { dropMessage, onEvent } from "../../core/bridge";
import { defineFeature, type FeatureCtx, type SectionHost } from "../../core/feature";
import { tn } from "../../i18n/i18n";
import { card, field, menu, row, toggle, type MenuItem } from "../../ui/components";
import { icon } from "../../ui/icons";
import { clear, h } from "../../views/dom";

interface Item {
  id: string;
  kind: "file" | "text" | "link";
  /** File name, first line of a text, address without its scheme. */
  name: string;
  path?: string;
  text?: string;
  dir?: boolean;
  /** The file is not where it was (moved, deleted, its drive unplugged). It keeps its place. */
  missing?: boolean;
  size?: number;
  mtime?: number;
}

interface Thumb {
  url: string;
  /** The file has no picture of its own: this is its icon. */
  icon: boolean;
  /** An icon for a file that has only just landed: its writer may still hold it. Ask once more. */
  again?: boolean;
}

/** What the preview shows of a file: its picture, the start of its text, or its icon. */
interface Picture {
  url?: string;
  text?: string;
  icon?: boolean;
}

interface WebView2 {
  postMessageWithAdditionalObjects(message: unknown, objects: ArrayLike<unknown>): void;
}

/**
 * The keys the island reads for the shelf while the pointer is over it. The
 * arrows move along the row (Shift takes a range), Enter opens, Delete takes
 * off the shelf, Space previews, Ctrl+V adds what the clipboard holds.
 */
const KEYS = ["ArrowLeft", "ArrowRight", "Shift+ArrowLeft", "Shift+ArrowRight", "Enter", "Delete", "Space", "Ctrl+V"];

/** A pencil: the one icon only the shelf needs (src/ui/icons.ts takes path data). */
const PENCIL = "M4 20h4L18.5 9.5a2.1 2.1 0 0 0-3-3L5 17zM14 8l3 3";

function bytes(n: number): string {
  if (n < 1000) return `${n} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let value = n / 1024;
  let unit = 0;
  while (value >= 1000 && unit < units.length - 1) {
    value /= 1024;
    unit++;
  }
  return `${value < 10 ? value.toFixed(1) : Math.round(value)} ${units[unit]}`;
}

/** The line under a name: what kind of thing it is, or how large. */
function detailOf(it: Item, t: FeatureCtx["t"]): string {
  if (it.kind === "text") return t("Text");
  if (it.kind === "link") return t("Link");
  if (it.missing) return t("Missing");
  return it.dir ? t("Folder") : bytes(it.size ?? 0);
}

function buildShelf(cx: FeatureCtx): SectionHost {
  const t = cx.t;
  let items: Item[] = [];
  const selected = new Set<string>();
  /** Where a range starts (Shift), and the item the arrows move from. */
  let anchor: string | null = null;
  let cursor: string | null = null;
  /** One of ours is on its way out: whatever comes over the island is not a drop. */
  let dragging = false;
  /** A line for the bottom row instead of the count ("Zipping…"). */
  let note = "";
  let noteTimer = 0;
  /** The note stays until the shelf changes (work in progress), rather than for a time. */
  let sticky = false;
  /** On screen: answers that arrive after the section has gone start nothing. */
  let shown = false;
  /** The files "Move to Recycle Bin" was chosen for, while the question is up. */
  let asking: Item[] | null = null;
  /** Ends the rename that is open. */
  let endRename: (() => void) | null = null;
  /** The item the preview window shows, and what keeps the island open meanwhile. */
  let previewing: string | null = null;
  let unhold: (() => void) | null = null;
  /** After a Delete: the item to stand on once the list has come back. */
  let standOn: string | null = null;
  const thumbs = new Map<string, Thumb>();
  /** Pictures asked for and not answered yet: one question for each, however often the row is drawn. */
  const wanted = new Set<string>();
  const thumbKey = (it: Item) => `${it.id}:${it.mtime ?? 0}`;

  const files = h("div", { class: "f-shelf-files", role: "listbox", "aria-multiselectable": "true" });
  const empty = h(
    "button",
    { class: "f-shelf-empty", onclick: () => void cx.call("pick") },
    icon("shelf", 20),
    h("span", { text: t("Drop files, text or links here, or click to choose files") }),
  );
  const meta = h("span", { class: "isl-meta" });
  const small = (name: string, label: string, kind: "secondary" | "ghost" | "danger", run: () => void) =>
    h("button", { class: `btn sm ${kind}`, onclick: run }, name ? icon(name, 14) : null, h("span", { text: label }));
  const copyBtn = small("copy", t("Copy"), "secondary", () => copy(target()));
  const shareBtn = small("share", t("Share"), "secondary", () => void share(target()));
  const zipBtn = small("zip", t("Zip"), "secondary", () => zip(target()));
  const clearBtn = small("", t("Clear"), "ghost", () => void cx.call("remove", { ids: ids(items) }));
  const actions = h("div", { class: "isl-row" }, meta, h("span", { class: "isl-gap" }), copyBtn, shareBtn, zipBtn, clearBtn);
  // "Move to Recycle Bin" acts on the user's own file: asked every time, never remembered.
  const question = h("span", { class: "isl-meta" });
  const confirm = h(
    "div",
    { class: "isl-row f-shelf-confirm", hidden: true },
    icon("trash", 14),
    question,
    h("span", { class: "isl-gap" }),
    small("", t("Cancel"), "ghost", () => ask(null)),
    small("", t("Move to Recycle Bin"), "danger", () => void recycle()),
  );
  const el = h("div", { class: "f-shelf-page" }, files, empty, actions, confirm);

  const ids = (list: Item[]) => list.map((it) => it.id);
  const chosen = () => items.filter((it) => selected.has(it.id));
  /** What the buttons act on: the selection, or everything when nothing is selected. */
  const target = () => (selected.size > 0 ? chosen() : items);
  const cellOf = (id: string) => [...files.children].find((c) => (c as HTMLElement).dataset.id === id) as HTMLElement | undefined;

  // ── Drawing ─────────────────────────────────────────────────────────────────

  /** Selection and the bottom row: what changes without the items changing. */
  const paint = () => {
    for (const cell of files.children) {
      const on = selected.has((cell as HTMLElement).dataset.id ?? "");
      cell.classList.toggle("on", on);
      cell.setAttribute("aria-selected", String(on));
    }
    const size = items.reduce((sum, it) => sum + (it.size ?? 0), 0);
    const count = items.length === 1 ? t("1 item") : t("{count} items", { count: items.length });
    meta.textContent =
      note || (items.length === 0 ? "" : [count, size > 0 ? bytes(size) : "", t("drag one out to move it")].filter(Boolean).join(" · "));
    copyBtn.disabled = shareBtn.disabled = !target().some((it) => !it.missing);
    clearBtn.disabled = items.length === 0;
    zipBtn.disabled = !target().some((it) => it.kind === "file" && !it.missing);
    actions.hidden = asking != null;
    confirm.hidden = asking == null;
  };

  const setThumb = (box: HTMLElement, thumb: Thumb) => {
    clear(box);
    box.classList.toggle("icon", thumb.icon);
    box.append(h("img", { src: thumb.url, alt: "", draggable: "false" }));
  };

  /**
   * The picture of a file, asked of Rust once. The answer goes to the cell the
   * item has by then (the row may have been drawn again), and nowhere when the
   * item has gone or changed meanwhile. A file that has only just landed can
   * still be held by whoever wrote it and gets its icon: asked again, a few
   * times, it gets its picture.
   */
  const askThumb = (it: Item, tries = 0) => {
    const key = thumbKey(it);
    wanted.add(key);
    void cx.call<Thumb>("thumb", { id: it.id }).then((thumb) => {
      wanted.delete(key);
      if (!thumb || !items.some((x) => thumbKey(x) === key)) return;
      thumbs.set(key, thumb);
      const box = cellOf(it.id)?.querySelector<HTMLElement>(".f-shelf-thumb");
      if (box) setThumb(box, thumb);
      if (!thumb.again || tries >= 4) return;
      wanted.add(key);
      window.setTimeout(() => {
        wanted.delete(key);
        if (shown && items.some((x) => thumbKey(x) === key)) askThumb(it, tries + 1);
      }, 1200 * (tries + 1));
    });
  };

  const cell = (it: Item) => {
    const box = h("span", { class: "f-shelf-thumb" }, icon(it.kind === "link" ? "link" : it.kind === "text" ? "text" : "file", 20));
    if (it.kind === "file" && !it.missing) {
      const known = thumbs.get(thumbKey(it));
      if (known) setThumb(box, known);
      if ((!known || known.again) && !wanted.has(thumbKey(it))) askThumb(it);
    }
    // Not a <button>: the field of a rename sits inside it.
    const node = h(
      "div",
      {
        class: `f-shelf-file${it.missing ? " missing" : ""}`,
        role: "option",
        tabindex: "0",
        title: it.path ?? it.text ?? it.name,
        "data-id": it.id,
      },
      box,
      h("b", { text: it.name }),
      h("small", { text: detailOf(it, t) }),
    );
    node.addEventListener("pointerdown", (e) => press(e, it));
    node.addEventListener("click", (e) => {
      e.stopPropagation();
      pick(e, it);
    });
    node.addEventListener("dblclick", () => open([it]));
    node.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      if (!selected.has(it.id)) only(it);
      itemMenu(it, { x: e.clientX, y: e.clientY });
    });
    return node;
  };

  const draw = () => {
    endRename = null;
    if (standOn && items.some((it) => it.id === standOn)) {
      selected.clear();
      selected.add(standOn);
      anchor = cursor = standOn;
    }
    standOn = null;
    for (const id of [...selected]) if (!items.some((it) => it.id === id)) selected.delete(id);
    for (const key of [...thumbs.keys()]) if (!items.some((it) => thumbKey(it) === key)) thumbs.delete(key);
    if (asking && !asking.every((a) => items.some((it) => it.id === a.id && !it.missing))) asking = null;
    // The item in the preview has left the shelf: so does the preview.
    if (previewing && !items.some((it) => it.id === previewing)) preview(null);
    clear(files);
    for (const it of items) files.append(cell(it));
    files.hidden = items.length === 0;
    empty.hidden = items.length > 0;
    paint();
    more();
  };

  /** The row fades out at its right end while there is more to scroll to. */
  const more = () => files.classList.toggle("f-shelf-more", files.scrollLeft + files.clientWidth < files.scrollWidth - 1);
  files.addEventListener("scroll", more);

  /** The list as Rust has it now. Of two questions under way, only the later one's answer is drawn. */
  let asked = 0;
  const refresh = async () => {
    const mine = ++asked;
    const got = (await cx.call<Item[]>("list")) ?? [];
    if (mine !== asked) return;
    items = got;
    draw();
  };

  const say = (text: string, ms = 0) => {
    window.clearTimeout(noteTimer);
    note = text;
    sticky = text !== "" && ms === 0;
    if (ms > 0) noteTimer = window.setTimeout(() => say(""), ms);
    paint();
  };

  // ── Selection ───────────────────────────────────────────────────────────────

  const only = (it: Item) => {
    selected.clear();
    selected.add(it.id);
    anchor = cursor = it.id;
    paint();
  };

  /** From the anchor to `it`, both included. */
  const range = (it: Item) => {
    const from = items.findIndex((x) => x.id === anchor);
    const to = items.indexOf(it);
    selected.clear();
    for (const x of items.slice(Math.min(from, to), Math.max(from, to) + 1)) selected.add(x.id);
    cursor = it.id;
    paint();
  };

  const pick = (e: MouseEvent, it: Item) => {
    if (e.shiftKey && items.some((x) => x.id === anchor)) range(it);
    else if (e.ctrlKey) {
      if (!selected.delete(it.id)) selected.add(it.id);
      anchor = cursor = it.id;
      paint();
    } else only(it);
  };

  /** One step along the row. It stops at the ends; from nowhere it starts at the near end. */
  const move = (step: 1 | -1, extend: boolean) => {
    if (items.length === 0) return;
    const at = items.findIndex((x) => x.id === cursor);
    const it = items[at < 0 ? (step > 0 ? 0 : items.length - 1) : Math.max(0, Math.min(items.length - 1, at + step))];
    if (extend && items.some((x) => x.id === anchor)) range(it);
    else only(it);
    // Into view, by the row's own scroll.
    const node = cellOf(it.id);
    if (node) {
      const [a, b] = [node.getBoundingClientRect(), files.getBoundingClientRect()];
      if (a.left < b.left) files.scrollLeft -= b.left - a.left + 2;
      else if (a.right > b.right) files.scrollLeft += a.right - b.right + 2;
    }
    if (previewing) preview(it);
  };

  // ── Out: drag, menu, actions ────────────────────────────────────────────────

  /** A press that moves 3 px becomes a drag of the cell, or of the selection it is part of. */
  const press = (e: PointerEvent, it: Item) => {
    if (e.button !== 0 || dragging || it.missing) return;
    const { clientX: x, clientY: y } = e;
    const moved = (m: PointerEvent) => {
      if (Math.hypot(m.clientX - x, m.clientY - y) < 3) return;
      done();
      void dragOut(selected.has(it.id) ? chosen() : [it]);
    };
    const done = () => {
      window.removeEventListener("pointermove", moved);
      window.removeEventListener("pointerup", done);
      window.removeEventListener("pointercancel", done);
    };
    window.addEventListener("pointermove", moved);
    window.addEventListener("pointerup", done);
    window.addEventListener("pointercancel", done);
  };

  const dragOut = async (what: Item[]) => {
    dragging = true;
    const release = cx.island.hold();
    // Out of the island for a moment: fold it, so it does not cover the drop target. A pinned island stays (close() refuses).
    let away: ReturnType<typeof setTimeout> | undefined;
    const unlisten = onEvent<{ x: number; y: number }>("cursor", ({ x, y }) => {
      const r = document.getElementById("island")?.getBoundingClientRect();
      const out = !!r && (x < r.left || x > r.right || y < r.top || y > r.bottom);
      if (!out) clearTimeout(away), (away = undefined);
      else away ??= setTimeout(() => cx.island.close(), 350);
    });
    try {
      // Returns when the button is released, wherever that is.
      await cx.call("drag", { ids: ids(what) });
    } finally {
      clearTimeout(away);
      void unlisten.then((u) => u());
      dragging = false;
      release();
    }
    // A file that was moved somewhere is not here any more.
    void refresh();
  };

  /** Files and links open; a text goes to the clipboard. */
  const open = (what: Item[]) => {
    for (const it of what) {
      if (it.missing) say(t("That file is not there any more."), 3000);
      else if (it.kind === "text") copy([it]);
      else void cx.call("open", { id: it.id });
    }
  };

  const copy = (what: Item[]) => {
    void cx.call<number>("copy", { ids: ids(what) }).then((n) => say(n ? t("Copied") : t("There was nothing to copy."), 1500));
  };

  const share = async (what: Item[]) => {
    if (!(await cx.call<boolean>("share", { ids: ids(what) }))) say(t("Windows did not show the Share sheet."), 4000);
  };

  const zip = (what: Item[]) => {
    say(t("Zipping…"));
    // The zip joins the shelf when it is made (`changed`), or `failed` says it was not.
    void cx.call("zip", { ids: ids(what) }).then((started) => {
      if (!started) say("");
    });
  };

  /** Off the shelf; the files stay where they are. The selection goes on to the neighbour. */
  const remove = (what: Item[]) => {
    if (what.length === 0) return;
    const gone = new Set(ids(what));
    const last = items.findIndex((it) => it.id === what[what.length - 1].id);
    standOn = (items.slice(last + 1).find((it) => !gone.has(it.id)) ?? [...items].reverse().find((it) => !gone.has(it.id)))?.id ?? null;
    void cx.call("remove", { ids: [...gone] });
  };

  const paste = async () => {
    const n = await cx.call<number>("paste");
    if (n === 0) say(t("There is nothing on the clipboard to add."), 2500);
    else if (n == null) say(t("The clipboard could not be read."), 2500);
  };

  // The question before the Recycle Bin, in place of the row of actions.
  const ask = (what: Item[] | null) => {
    const real = what?.filter((it) => it.kind === "file" && !it.missing) ?? [];
    asking = real.length > 0 ? real : null;
    if (asking) {
      question.textContent =
        asking.length === 1
          ? t("Move “{name}” to the Recycle Bin?", { name: asking[0].name })
          : tn("Move {count} file to the Recycle Bin?", "Move {count} files to the Recycle Bin?", asking.length);
    }
    paint();
  };

  const recycle = async () => {
    const what = asking;
    ask(null);
    if (!what) return;
    const done = await cx.call<{ moved: number; asked: number }>("recycle", { ids: ids(what) });
    const left = done ? done.asked - done.moved : what.length;
    if (left > 0) {
      say(tn("{count} file could not go to the Recycle Bin and is still where it was.", "{count} files could not go to the Recycle Bin and are still where they were.", left), 6000);
    } else say(t("Moved to the Recycle Bin"), 2500);
  };

  /** Why a name was refused (svc_shell::NameProblem), as a sentence. */
  const whyNot = (why?: string) =>
    why === "empty" ? t("Type a name.")
    : why === "characters" ? t("A name cannot contain \\ / : * ? \" < > | or end with a dot.")
    : why === "reserved" ? t("Windows keeps that name for itself. Choose another.")
    : why === "long" ? t("That name is too long.")
    : why === "taken" ? t("Another file there already has that name.")
    : why === "missing" ? t("That file is not there any more.")
    : t("Windows did not rename it. Close the file if a program has it open, then try again.");

  /** The name under the cell becomes a field: Enter renames the file itself, Esc or a click elsewhere gives up. */
  const rename = (it: Item) => {
    endRename?.();
    const node = cellOf(it.id);
    const label = node?.querySelector<HTMLElement>("b");
    const under = node?.querySelector<HTMLElement>("small");
    if (!node || !label || !under) return;
    const box = field({ value: it.name, onEnter: (name) => void commit(name) });
    box.classList.add("f-shelf-rename");
    const input = box.input as HTMLInputElement;
    const end = () => {
      if (endRename !== end) return;
      endRename = null;
      box.remove();
      label.hidden = under.hidden = false;
    };
    const commit = async (name: string) => {
      if (name.trim() === it.name) return end();
      const done = await cx.call<{ ok: boolean; why?: string }>("rename", { id: it.id, name });
      // Renamed: `changed` draws the row again.
      if (done?.ok) return end();
      say(whyNot(done?.why), 5000);
      input.focus();
    };
    // The field is the cell's guest: a click in it is not a click on the cell.
    for (const name of ["pointerdown", "click", "dblclick", "contextmenu"]) box.addEventListener(name, (e) => e.stopPropagation());
    input.addEventListener("blur", () => window.setTimeout(end, 150));
    endRename = end;
    label.hidden = under.hidden = true;
    node.append(box);
    input.focus();
    // The name without its extension, as Explorer selects it.
    const dot = it.dir ? -1 : it.name.lastIndexOf(".");
    input.setSelectionRange(0, dot > 0 ? dot : it.name.length);
  };

  /** Space: the preview window on `it`; with null it closes. Rust answers with `preview`. */
  const preview = (it: Item | null) => void cx.call("preview", it ? { id: it.id } : {});

  /** The menu that is up, to take it down with the section. */
  let openMenu: HTMLElement | null = null;

  // The menu has the island's height to live in: six entries and a line. So it
  // holds what only it can do; Copy, Share and Zip are the buttons under the
  // row, which act on the same selection, and Space previews.
  const itemMenu = (it: Item, at: { x: number; y: number }) => {
    const all = chosen();
    const there = all.filter((x) => !x.missing);
    const realFiles = there.filter((x) => x.kind === "file");
    const list: (MenuItem | null)[] = [];
    if (all.length === 1 && there.length === 1) {
      if (it.kind !== "text") list.push({ label: t("Open"), icon: "external", kbd: t("Enter"), onClick: () => open([it]) });
      if (it.kind === "file") {
        if (!it.dir) list.push({ label: t("Open with…"), icon: "grid", onClick: () => void openWithMenu(it, at) });
        list.push({ label: t("Show in folder"), icon: "search", onClick: () => void cx.call("reveal", { id: it.id }) });
        list.push({ label: t("Rename…"), icon: PENCIL, onClick: () => rename(it) });
      } else {
        // A note or a link: there is room, and Copy is the thing to do with one.
        list.push({ label: t("Preview"), icon: "eye", kbd: t("Space"), onClick: () => preview(it) });
        list.push({ label: t("Copy"), icon: "copy", onClick: () => copy([it]) });
      }
    } else if (there.length > 1) {
      list.push({ label: t("Copy"), icon: "copy", onClick: () => copy(there) });
      if (realFiles.length > 0) list.push({ label: t("Zip"), icon: "zip", onClick: () => zip(realFiles) });
    }
    list.push({ label: t("Remove from the shelf"), icon: "close", kbd: t("Del"), onClick: () => remove(all) });
    // The one entry that touches the user's own file: apart, last, and asked about.
    if (realFiles.length > 0) list.push(null, { label: t("Move to Recycle Bin…"), icon: "trash", danger: true, onClick: () => ask(realFiles) });
    openMenu = menu(list, at);
  };

  const openWithMenu = async (it: Item, at: { x: number; y: number }) => {
    const names = (await cx.call<string[]>("handlers", { id: it.id })) ?? [];
    // Late: the section has gone, or the file has left the shelf.
    if (!shown || !items.some((x) => x.id === it.id)) return;
    const list: (MenuItem | null)[] = names
      .slice(0, 5)
      .map((name) => ({ label: name, onClick: () => void cx.call("open_with", { id: it.id, name }) }));
    if (list.length > 0) list.push(null);
    list.push({ label: t("Choose another app…"), icon: "grid", onClick: () => void cx.call("open_with", { id: it.id }) });
    openMenu = menu(list, at);
  };

  // ── Keys ────────────────────────────────────────────────────────────────────
  //
  // Heard on `window` like any other key, whether the island holds the keyboard
  // or reads `KEYS` under the pointer. Ctrl+C and Ctrl+A are what one types in
  // one's own window too: they work here once a click gave the island the keyboard.

  const onKey = (e: KeyboardEvent) => {
    const at = e.target instanceof Element ? e.target : null;
    // A field has its own keys (a rename is open), a menu that is up has Esc, the question is answered by a click.
    if (e.defaultPrevented || e.altKey || e.metaKey || at?.closest("input, textarea") || endRename || openMenu?.isConnected || asking) return;
    const plain = !e.ctrlKey && !e.shiftKey;
    const letter = e.ctrlKey && !e.shiftKey ? e.key.toLowerCase() : "";
    if (!e.ctrlKey && (e.key === "ArrowLeft" || e.key === "ArrowRight")) move(e.key === "ArrowRight" ? 1 : -1, e.shiftKey);
    else if (plain && e.key === "Enter" && selected.size > 0) open(chosen());
    else if (plain && e.key === "Delete" && selected.size > 0) remove(chosen());
    else if (plain && e.key === " " && (previewing || items.length > 0)) {
      // The selection's first, or the first of all: Space without a click still shows something.
      const it = items.find((x) => x.id === cursor && selected.has(x.id)) ?? chosen()[0] ?? items[0];
      if (previewing) preview(null);
      else if (it) preview(it);
    } else if (letter === "v") void paste();
    else if (letter === "c" && selected.size > 0) copy(chosen());
    else if (letter === "a" && items.length > 0) {
      for (const it of items) selected.add(it.id);
      paint();
    } else return;
    // Used: not also the cell's own click (Enter, Space) or the page's select-all.
    e.preventDefault();
  };

  // ── In: drops ───────────────────────────────────────────────────────────────
  //
  // Taken in the capture phase and kept from going further: while the shelf is
  // on screen a drop is the shelf's, not the base drop's (which would hand the
  // file to the chat).

  let depth = 0;
  const over = (on: boolean) => el.classList.toggle("f-shelf-over", on);

  const claim = (e: DragEvent): boolean => {
    e.stopPropagation();
    const data = e.dataTransfer;
    if (dragging || !data) return false;
    const allowed = data.effectAllowed;
    const effect = /copy|all|uninit/i.test(allowed) ? "copy" : /link/i.test(allowed) ? "link" : "none";
    if (effect === "none") return false;
    e.preventDefault();
    data.dropEffect = effect;
    return true;
  };
  const onEnter = (e: DragEvent) => {
    if (claim(e) && depth++ === 0) over(true);
  };
  const onOver = (e: DragEvent) => void claim(e);
  // Not kept from the base drop: there it only undoes an enter it saw before the shelf came up.
  const onLeave = () => {
    if (depth > 0 && --depth === 0) over(false);
  };
  const onDrop = (e: DragEvent) => {
    const ours = claim(e);
    depth = 0;
    over(false);
    const data = e.dataTransfer;
    if (!ours || !data) return;
    if (data.files.length > 0) {
      // Only WebView2 knows where a dropped File lives: Rust turns them into paths
      // and, the message naming this feature, hands them to its `dropped`
      // (src-tauri/src/webview_drop.rs). `changed` follows. A file that was
      // only offered (an attachment dragged out of a mail) comes the same way:
      // WebView2 writes it to the temp folder first, and Rust keeps a copy.
      const webview = (window as unknown as { chrome?: { webview?: WebView2 } }).chrome?.webview;
      webview?.postMessageWithAdditionalObjects(dropMessage("shelf"), data.files);
      return;
    }
    // A link's own line of text/uri-list (lines starting with # are comments), else the text.
    const address = data
      .getData("text/uri-list")
      .split(/\r?\n/)
      .find((line) => line && !line.startsWith("#"));
    const text = address || data.getData("text/plain");
    if (text.trim()) void cx.call("add", { text });
  };

  // A click beside the items lets go of the selection.
  el.addEventListener("click", (e) => {
    if (e.target !== el && e.target !== files) return;
    selected.clear();
    paint();
  });
  // The cells go out through Rust, never as an HTML drag.
  el.addEventListener("dragstart", (e) => e.preventDefault());
  // One row: the wheel moves along it.
  files.addEventListener("wheel", (e) => {
    if (e.deltaX !== 0 || files.scrollWidth <= files.clientWidth) return;
    files.scrollLeft += e.deltaY;
    e.preventDefault();
  });

  let off: (() => void)[] = [];
  return {
    el,
    show() {
      shown = true;
      const drag: [string, (e: DragEvent) => void][] = [
        ["dragenter", onEnter],
        ["dragover", onOver],
        ["dragleave", onLeave],
        ["drop", onDrop],
      ];
      for (const [name, fn] of drag) window.addEventListener(name, fn as EventListener, true);
      window.addEventListener("keydown", onKey);
      off = [
        () => drag.forEach(([name, fn]) => window.removeEventListener(name, fn as EventListener, true)),
        () => window.removeEventListener("keydown", onKey),
        cx.on("changed", () => {
          if (sticky) say("");
          void refresh();
        }),
        cx.on("failed", () => say(t("The zip could not be made. Check that the files are still there."), 5000)),
        // The preview came up, moved on or closed (Space, or a click on its window).
        // While it is up the island stays: it is where Space and Esc are read.
        cx.on<Item | null>("preview", (it) => {
          previewing = it?.id ?? null;
          if (previewing && !unhold) unhold = cx.island.hold();
          if (!previewing) {
            unhold?.();
            unhold = null;
          }
        }),
      ];
      void refresh();
    },
    hide() {
      shown = false;
      for (const fn of off) fn();
      off = [];
      depth = 0;
      over(false);
      openMenu?.remove();
      endRename?.();
      asking = null;
      if (previewing) preview(null);
      previewing = null;
      unhold?.();
      unhold = null;
    },
    sync() {},
    // Esc steps back through what is open: the preview, a rename, the question, the selection.
    command(command) {
      if (command !== "back") return false;
      if (previewing) preview(null);
      else if (endRename) endRename();
      else if (asking) ask(null);
      else if (selected.size > 0) {
        selected.clear();
        paint();
      } else return false;
      return true;
    },
  };
}

/** The preview window (Space): one item, as large as the window. A click anywhere closes it. */
function buildPreview(root: HTMLElement, cx: FeatureCtx) {
  const t = cx.t;
  const close = () => void cx.call("preview", {});
  const title = h("span");
  const body = h("div", { class: "f-shelf-pv-body" });
  const foot = h("div", { class: "f-shelf-pv-foot" });
  root.append(
    h(
      "div",
      { class: "win f-shelf-pv", onclick: close },
      h("div", { class: "win-bar" }, icon("eye", 14), title, h("button", { class: "close", title: t("Close"), "aria-label": t("Close"), onclick: close, text: "×" })),
      body,
      foot,
    ),
  );
  const words = (text: string) => h("pre", { class: "f-shelf-pv-text", text: text.slice(0, 20_000) });
  let showing: string | null = null;
  const show = (it: Item | null) => {
    // Null is the window closing: Rust takes it down.
    if (!it) return;
    showing = it.id;
    title.textContent = it.name;
    foot.textContent = [detailOf(it, t), it.kind === "file" ? it.path : ""].filter(Boolean).join(" · ");
    clear(body);
    if (it.kind !== "file") return void body.append(words(it.text ?? ""));
    if (it.missing) return void body.append(h("p", { class: "f-shelf-pv-none", text: t("That file is not there any more.") }));
    void cx.call<Picture>("picture", { id: it.id }).then((p) => {
      // Late: the preview has moved on to another item, or its item has left the shelf.
      if (!p || showing !== it.id) return;
      clear(body);
      body.append(p.text != null ? words(p.text) : h("img", { class: p.icon ? "icon" : "", src: p.url ?? "", alt: "", draggable: "false" }));
    });
  };
  cx.on<Item | null>("preview", show);
  void cx.call<Item | null>("previewing").then(show);
}

export default defineFeature({
  id: "shelf",
  voice: () => import("./voice"),
  sections: [{ id: "shelf", title: "Shelf", icon: "shelf", letter: "s", height: 192, keys: KEYS, build: buildShelf }],
  windows: { preview: buildPreview },
  settings: (cx) => {
    const detect = toggle(true, (on) => void cx.settings.patch({ dragDetect: on }));
    // Off unless the user turns it on: it writes to Explorer's menus.
    const explorer = toggle(false, (on) => void cx.settings.patch({ explorerMenu: on }));
    void cx.settings.get().then((s) => {
      detect.setAttribute("aria-checked", String(s.dragDetect !== false));
      explorer.setAttribute("aria-checked", String(s.explorerMenu === true));
    });
    return card([
      row(
        cx.t("Open the shelf when something is dragged to the top of the screen"),
        detect,
        cx.t("Follows the mouse only while a button is held, and keeps nothing."),
      ),
      row(
        cx.t("“Add to Shelf” in Explorer's right-click menu and in Send to"),
        explorer,
        cx.t("For files and folders. On Windows 11 the right-click entry is under “Show more options”. Turning this off takes both entries out again."),
      ),
    ]);
  },
  start: (cx) => {
    // Something is being dragged toward the island (Rust's drag watcher): the
    // shelf opens and stays until the drag is over.
    let release: (() => void) | null = null;
    let timer = 0;
    const letGo = () => {
      window.clearTimeout(timer);
      release?.();
      release = null;
    };
    const offNear = cx.on("drag-near", () => {
      letGo();
      release = cx.island.hold();
      // Not while the Drop tab is up: a file dropped there is the chat's.
      cx.island.open("shelf", { dragged: true });
      // Should the end of the drag never be reported.
      timer = window.setTimeout(letGo, 30_000);
    });
    // A moment more, so that what was dropped is seen landing.
    const offEnd = cx.on("drag-end", () => {
      window.clearTimeout(timer);
      timer = window.setTimeout(letGo, 800);
    });
    // Files came by another road than the island (the file picker, Explorer's
    // menus): the shelf opens on them.
    const offPicked = cx.on<number>("picked", (count) => {
      if (count > 0) cx.island.open("shelf");
    });
    return () => {
      offNear();
      offEnd();
      offPicked();
      letGo();
    };
  },
});
