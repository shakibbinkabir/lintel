// Capture — the page side of the feature (docs/ARCHITECTURE.md §2.3).
// Nothing runs at import: declare surfaces here, begin work in `start`.
//
// One section, Captures: the tools as a row of buttons, the last captures as a
// row of cells, and under them what can be done with the one that is selected
// (copy, save, pin, share) or what a tool just read off the screen. Two window
// pages: `selector`, the overlay a session puts over each display
// (selector.ts), and `pin`, a capture floating above other windows. The
// pictures and every pixel of them live in Rust.

import "./style.css";
import { Bridge } from "../../core/bridge";
import { defineFeature, type FeatureCtx, type Notice, type SectionHost } from "../../core/feature";
import { card, field, iconBtn, menu, row, toggle, type MenuItem } from "../../ui/components";
import { icon } from "../../ui/icons";
import { clear, h } from "../../views/dom";
import { mountSelector, RULER, windowLabel } from "./selector";

interface Shot {
  id: string;
  /** The file's name. */
  name: string;
  w: number;
  h: number;
  /** Seconds since 1970. */
  at: number;
  /** Where "Save" put a copy. */
  saved?: string | null;
}

/** What a tool read off the screen. */
interface Result {
  kind: "color" | "text" | "qr" | "none";
  text?: string;
  hex?: string;
  /** A QR code that holds one plain web address: it may be opened, by a click. */
  link?: boolean;
}

interface Pins {
  count: number;
  /** How many let clicks through. */
  through: number;
}

/** The Captures section is on screen: it shows a result itself, so no notice is needed. */
let onScreen = false;
/** The last result, for a section that is opened from its notice. */
let last: { result: Result; at: number } | null = null;

const swatchOf = (hex: string | undefined, big = false) =>
  h("i", { class: big ? "f-capture-swatch big" : "f-capture-swatch", style: `background:${/^#[0-9A-F]{6}$/.test(hex ?? "") ? hex : "transparent"}` });

const firstLine = (text: string | undefined) => (text ?? "").split("\n")[0] ?? "";

/** A result as a notice, for when the section is not there to show it. */
function noticeOf(cx: FeatureCtx, r: Result): Notice {
  const t = cx.t;
  const base = { kind: "capture" as const, section: "captures" };
  if (r.kind === "color") return { ...base, title: r.text ?? "", detail: t("Colour copied"), mark: swatchOf(r.hex, true) };
  if (r.kind === "text") return { ...base, title: t("Text copied"), detail: firstLine(r.text), icon: "text", tone: "ok" };
  if (r.kind === "qr") {
    const text = r.text ?? "";
    // Nothing is opened or copied until this button is clicked.
    const action = r.link
      ? { label: t("Open link"), run: () => void Bridge.openUrl(text) }
      : { label: t("Copy"), run: () => void cx.call("copy_text", { text }) };
    return { ...base, title: t("QR code"), detail: firstLine(text), icon: "link", tone: "info", action };
  }
  return { ...base, title: t("Nothing to read there"), icon: "text", tone: "warn" };
}

function buildCaptures(cx: FeatureCtx): SectionHost {
  const t = cx.t;
  let shots: Shot[] = [];
  let selected: string | null = null;
  let result: Result | null = null;
  let pins: Pins = { count: 0, through: 0 };
  let canRead = false;
  /** One of ours is on its way out of the island. */
  let dragging = false;
  /** A line for the bottom row instead of the capture's size ("Copied"). */
  let note = "";
  let noteTimer = 0;
  const thumbs = new Map<string, string>();
  /** The menu that is up, to take it down with the section. */
  let openMenu: HTMLElement | null = null;

  const small = (name: string, label: string, kind: "secondary" | "ghost" | "primary", run: () => void) =>
    h("button", { class: `btn sm ${kind}`, onclick: run }, name ? icon(name, 14) : null, h("span", { text: label }));
  const begin = (tool: string) => () => void cx.call("begin", { tool });

  // The tools, and at the right end the pins that are up.
  const textBtn = small("text", t("Text"), "secondary", begin("text"));
  const pinsBtn = small("pin", "", "ghost", () => {
    const at = pinsBtn.getBoundingClientRect();
    const list: MenuItem[] = [{ label: t("Close all pins"), icon: "close", onClick: () => void cx.call("pins_close") }];
    // A pin that lets clicks through cannot be reached any more: this is the way back.
    if (pins.through > 0) list.unshift({ label: t("Make pins clickable"), icon: "pin", onClick: () => void cx.call("pins_clickable") });
    openMenu = menu(list, { x: at.left, y: at.bottom + 4 });
  });
  const tools = h(
    "div",
    { class: "isl-row" },
    small("shot", t("Area"), "secondary", begin("shot")),
    small("monitor", t("Screen"), "secondary", begin("full")),
    small("drop", t("Colour"), "secondary", begin("color")),
    textBtn,
    small(RULER, t("Ruler"), "secondary", begin("ruler")),
    h("span", { class: "isl-gap" }),
    pinsBtn,
  );

  const cells = h("div", { class: "f-capture-shots", role: "listbox" });
  const empty = h("div", { class: "f-capture-empty" }, icon("shot", 20), h("span", { text: t("Nothing captured yet") }));

  // The bottom row: the selected capture and its actions, or what a tool just read.
  const mark = h("span", { class: "f-capture-mark" });
  const meta = h("span", { class: "isl-meta" });
  const current = () => shots.find((s) => s.id === selected) ?? null;
  const act = (method: string) => () => {
    const shot = current();
    if (shot) void cx.call(method, { id: shot.id });
  };
  const copyBtn = small("copy", t("Copy"), "secondary", () => copy(current()));
  const saveBtn = small("download", t("Save"), "secondary", () => save(current()));
  const pinBtn = small("pin", t("Pin"), "secondary", act("pin"));
  const ocrBtn = small("copy", t("Copy text"), "secondary", () => {
    const shot = current();
    if (!shot) return;
    say(t("Reading…"));
    void cx.call<number>("copy_ocr", { id: shot.id }).then((n) => say(n ? t("Text copied") : t("No text found"), 2000));
  });
  ocrBtn.hidden = true; // until Windows says it can read text
  const editBtn = small("M4 20l1-4.5L16.5 4a2.1 2.1 0 0 1 3 3L8 18.5zM14.5 6l3 3", t("Edit"), "secondary", () => {
    const shot = current();
    if (shot) void Bridge.featureCall("editor", "open", { capture: shot.name });
  });
  const shareBtn = small("share", t("Share"), "secondary", act("share"));
  const clearBtn = small("", t("Clear"), "ghost", () => void cx.call("clear"));
  const openBtn = small("external", t("Open link"), "primary", () => result?.text && void Bridge.openUrl(result.text));
  const copyTextBtn = small("copy", t("Copy"), "secondary", () => {
    if (!result?.text) return;
    void cx.call("copy_text", { text: result.text });
    result = null;
    say(t("Copied"), 1500);
  });
  const doneBtn = iconBtn("close", t("Dismiss"), () => {
    result = null;
    paint();
  });
  doneBtn.classList.add("sm");
  const shotBtns = [copyBtn, ocrBtn, saveBtn, pinBtn, editBtn, shareBtn, clearBtn];
  const el = h(
    "div",
    { class: "f-capture-page" },
    tools,
    cells,
    empty,
    h("div", { class: "isl-row" }, mark, meta, h("span", { class: "isl-gap" }), ...shotBtns, openBtn, copyTextBtn, doneBtn),
  );

  // ── Drawing ─────────────────────────────────────────────────────────────────

  const time = (at: number) => {
    const when = new Date(at * 1000);
    const today = when.toDateString() === new Date().toDateString();
    return today
      ? when.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })
      : when.toLocaleDateString([], { day: "numeric", month: "short" });
  };

  const resultText = (r: Result) =>
    r.kind === "color" ? t("{value} copied", { value: r.text ?? "" })
    : r.kind === "text" ? t("Text copied: {text}", { text: firstLine(r.text) })
    : r.kind === "qr" ? t("QR code: {text}", { text: firstLine(r.text) })
    : t("Nothing to read there");

  /** Selection, the bottom row and the pins: what changes without the captures changing. */
  const paint = () => {
    for (const cell of cells.children) {
      const on = (cell as HTMLElement).dataset.id === selected;
      cell.classList.toggle("on", on);
      cell.setAttribute("aria-selected", String(on));
    }
    const shot = current();
    clear(mark);
    if (result?.kind === "color") mark.append(swatchOf(result.hex));
    mark.hidden = result?.kind !== "color";
    meta.textContent = result
      ? resultText(result)
      : note || (shot ? [`${shot.w} × ${shot.h}`, time(shot.at), shot.saved ? t("saved") : ""].filter(Boolean).join(" · ") : "");
    meta.title = result?.text ?? shot?.saved ?? "";
    for (const b of shotBtns) b.hidden = !!result || (b === ocrBtn && !canRead);
    for (const b of [copyBtn, ocrBtn, saveBtn, pinBtn, editBtn, shareBtn]) b.disabled = !shot;
    clearBtn.disabled = shots.length === 0;
    (saveBtn.lastChild as HTMLElement).textContent = shot?.saved ? t("Show in folder") : t("Save");
    openBtn.hidden = !(result?.kind === "qr" && result.link);
    copyTextBtn.hidden = result?.kind !== "qr";
    doneBtn.hidden = !result;

    (pinsBtn.lastChild as HTMLElement).textContent = pins.count === 1 ? t("1 pin") : t("{count} pins", { count: pins.count });
    pinsBtn.hidden = pins.count === 0;
  };

  const cell = (s: Shot) => {
    const box = h("span", { class: "f-capture-thumb" }, icon("image", 20));
    const setThumb = (url: string) => {
      clear(box);
      box.append(h("img", { src: url, alt: "", draggable: "false" }));
    };
    const known = thumbs.get(s.id);
    if (known) setThumb(known);
    else
      void cx.call<{ url: string }>("thumb", { id: s.id }).then((thumb) => {
        if (!thumb) return;
        thumbs.set(s.id, thumb.url);
        setThumb(thumb.url);
      });
    const node = h(
      "button",
      { class: "f-capture-shot", role: "option", title: s.name, "data-id": s.id },
      box,
      h("b", { text: `${s.w} × ${s.h}` }),
      h("small", { text: time(s.at) }),
    );
    node.addEventListener("pointerdown", (e) => press(e, s));
    node.addEventListener("click", () => select(s.id));
    node.addEventListener("dblclick", () => void cx.call("open", { id: s.id }));
    node.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      select(s.id);
      shotMenu(s, { x: e.clientX, y: e.clientY });
    });
    return node;
  };

  const draw = () => {
    if (!shots.some((s) => s.id === selected)) selected = shots[0]?.id ?? null;
    for (const id of [...thumbs.keys()]) if (!shots.some((s) => s.id === id)) thumbs.delete(id);
    clear(cells);
    for (const s of shots) cells.append(cell(s));
    cells.hidden = shots.length === 0;
    empty.hidden = shots.length > 0;
    paint();
    more();
  };

  /** The row fades out at its right end while there is more to scroll to. */
  const more = () => cells.classList.toggle("f-capture-more", cells.scrollLeft + cells.clientWidth < cells.scrollWidth - 1);
  cells.addEventListener("scroll", more);
  cells.addEventListener("wheel", (e) => {
    if (e.deltaX !== 0 || cells.scrollWidth <= cells.clientWidth) return;
    cells.scrollLeft += e.deltaY;
    e.preventDefault();
  });

  const refresh = async () => {
    shots = (await cx.call<Shot[]>("list")) ?? [];
    draw();
  };

  const say = (text: string, ms = 0) => {
    window.clearTimeout(noteTimer);
    note = text;
    if (ms > 0) noteTimer = window.setTimeout(() => say(""), ms);
    paint();
  };

  const select = (id: string) => {
    selected = id;
    result = null;
    paint();
  };

  // ── What is done with a capture ─────────────────────────────────────────────

  const copy = (shot: Shot | null) => {
    if (!shot) return;
    void cx.call("copy", { id: shot.id });
    say(t("Copied"), 1500);
  };

  /** A copy in the user's folder; once it is there the button shows it in its folder. */
  const save = async (shot: Shot | null) => {
    if (!shot) return;
    if (shot.saved) return void cx.call("reveal", { id: shot.id });
    const path = await cx.call<string>("save", { id: shot.id });
    say(path ? t("Saved to {folder}", { folder: path.replace(/[\\/][^\\/]*$/, "") }) : t("It could not be saved. Check the folder in Settings."), 4000);
  };

  /** A press that moves 3 px becomes a drag of the capture's file out of the island. */
  const press = (e: PointerEvent, shot: Shot) => {
    if (e.button !== 0 || dragging) return;
    const { clientX: x, clientY: y } = e;
    const move = (m: PointerEvent) => {
      if (Math.hypot(m.clientX - x, m.clientY - y) < 3) return;
      done();
      void dragOut(shot);
    };
    const done = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", done);
      window.removeEventListener("pointercancel", done);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", done);
    window.addEventListener("pointercancel", done);
  };

  const dragOut = async (shot: Shot) => {
    dragging = true;
    const release = cx.island.hold();
    try {
      // Returns when the button is released, wherever that is.
      await cx.call("drag", { id: shot.id });
    } finally {
      dragging = false;
      release();
    }
    // A capture that was moved somewhere is not here any more.
    void refresh();
  };

  const shotMenu = (s: Shot, at: { x: number; y: number }) => {
    const list: (MenuItem | null)[] = [
      { label: t("Open"), icon: "external", onClick: () => void cx.call("open", { id: s.id }) },
      { label: t("Copy"), icon: "copy", onClick: () => copy(s) },
      { label: s.saved ? t("Show in folder") : t("Save"), icon: s.saved ? "search" : "download", onClick: () => void save(s) },
      { label: t("Pin"), icon: "pin", onClick: () => void cx.call("pin", { id: s.id }) },
      { label: t("Share"), icon: "share", onClick: () => void cx.call("share", { id: s.id }) },
      null,
      { label: t("Remove"), icon: "trash", danger: true, onClick: () => void cx.call("remove", { id: s.id }) },
    ];
    openMenu = menu(list, at);
  };

  // The cells go out through Rust, never as an HTML drag.
  el.addEventListener("dragstart", (e) => e.preventDefault());

  let off: (() => void)[] = [];
  return {
    el,
    show() {
      onScreen = true;
      // A result the notice already told of, when the section is opened from it.
      result = last && Date.now() - last.at < 60_000 ? last.result : null;
      last = null;
      off = [
        // Each listener draws from its own event: they are not called in a set order.
        cx.on<{ shot: Shot }>("captured", ({ shot }) => {
          selected = shot.id;
          result = null;
          void refresh();
        }),
        cx.on("changed", () => void refresh()),
        cx.on<Result>("result", (r) => {
          result = r;
          paint();
        }),
        cx.on<Pins>("pins", (p) => {
          pins = p;
          paint();
        }),
      ];
      void refresh();
      void cx.call<Pins>("pins").then((p) => {
        if (!p) return;
        pins = p;
        paint();
      });
      // Without a language Windows can read, the text tool only reads codes.
      void cx.call<{ ocr: boolean }>("caps").then((caps) => {
        if (!caps) return;
        (textBtn.lastChild as HTMLElement).textContent = caps.ocr ? t("Text") : t("QR code");
        canRead = caps.ocr;
        paint();
      });
    },
    hide() {
      onScreen = false;
      for (const fn of off) fn();
      off = [];
      openMenu?.remove();
    },
    sync() {},
    command(command) {
      if (command !== "back" || !result) return false;
      result = null;
      paint();
      return true;
    },
  };
}

function settingsBody(cx: FeatureCtx): HTMLElement {
  const t = cx.t;
  const el = h("div", { class: "f-capture-settings" });
  void Promise.all([cx.settings.get(), cx.call<{ folder: string }>("caps")]).then(([s, caps]) => {
    const sw = (key: string) => toggle(s[key] !== false, (on) => void cx.settings.patch({ [key]: on }));
    const folder = field({ value: String(s.folder ?? ""), placeholder: caps?.folder ?? "" });
    folder.input.addEventListener("change", () => void cx.settings.patch({ folder: folder.input.value.trim() }));
    const format = h(
      "select",
      {},
      ...[["hex", "HEX"], ["rgb", "RGB"], ["hsl", "HSL"]].map(([value, name]) => h("option", { value, text: name })),
    );
    format.value = String(s.colorFormat ?? "hex");
    format.addEventListener("change", () => void cx.settings.patch({ colorFormat: format.value }));
    clear(el);
    el.append(
      card([
        row(t("Copy each screenshot"), sw("copy"), t("It is on the clipboard as soon as it is taken.")),
        row(t("Save each screenshot"), sw("save"), t("As a PNG in the folder below.")),
        row(t("Folder"), folder, t("Leave empty for Screenshots in your Pictures.")),
      ]),
      card([
        row(t("Magnifier"), sw("loupe"), t("Shows the pixels under the pointer while you select. Z turns it on and off.")),
        row(t("Colour format"), h("label", { class: "field" }, format), t("How a picked colour is copied.")),
      ]),
      h("p", {
        class: "f-capture-note",
        text: t("The last 12 captures stay on this computer, in the island's Captures page, until you clear them. Text and QR codes are read on this computer."),
      }),
    );
  });
  return el;
}

/** A capture floating above other windows: drag moves it, a double click closes it. */
function mountPin(root: HTMLElement, cx: FeatureCtx) {
  const label = windowLabel();
  const image = h("img", { alt: "", draggable: "false" });
  root.className = "f-capture-pin";
  root.title = cx.t("Drag to move. Double-click to close. Scroll to zoom, Alt+scroll to fade, Ctrl+drag to take the file. Right-click to let clicks through.");
  root.append(image);
  void cx.call<{ url: string }>("pin_image", { label }).then((picture) => {
    if (picture) image.src = picture.url;
    else void cx.call("pin_close", { label });
  });

  let opacity = 1;
  root.addEventListener("wheel", (e) => {
    if (e.altKey) {
      opacity = Math.max(0.2, Math.min(1, opacity + (e.deltaY < 0 ? 0.1 : -0.1)));
      image.style.opacity = String(opacity);
    } else void cx.call("pin_zoom", { label, k: e.deltaY < 0 ? 1.1 : 1 / 1.1 });
  });
  root.addEventListener("mousedown", (e) => {
    if (e.button !== 0) return;
    if (e.detail >= 2) return void cx.call("pin_close", { label });
    const { clientX: x, clientY: y } = e;
    const file = e.ctrlKey;
    const move = (m: MouseEvent) => {
      if (Math.hypot(m.clientX - x, m.clientY - y) < 3) return;
      done();
      // Rust moves the window with the pointer until the button is let go; with Ctrl, the file is dragged out instead.
      void cx.call(file ? "drag" : "pin_move", { label });
    };
    const done = () => {
      window.removeEventListener("mousemove", move);
      window.removeEventListener("mouseup", done);
    };
    window.addEventListener("mousemove", move);
    window.addEventListener("mouseup", done);
  });
  // From here on the pin is only looked at; the Captures page makes it clickable again.
  root.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    void cx.call("pin_through", { label });
  });
}

export default defineFeature({
  id: "capture",
  sections: [{ id: "captures", title: "Captures", icon: "shot", letter: "p", height: 232, build: buildCaptures }],
  settings: settingsBody,
  windows: { selector: mountSelector, pin: mountPin },
  start: (cx) => {
    const offs = [
      // A new capture: the island shows it, with what can be done with it.
      cx.on("captured", () => cx.island.open("captures")),
      // The selector's Record tool: what was pointed at goes to the recorder, through the registry.
      // A recorder that refuses says so itself (its own notice).
      cx.on<object>("record", (target) => void Bridge.featureCall("recorder", "start", target)),
      cx.on<Result>("result", (result) => {
        if (onScreen) return;
        last = { result, at: Date.now() };
        cx.island.notice(noticeOf(cx, result));
      }),
    ];
    return () => offs.forEach((off) => off());
  },
});
