// The editor's window: one picture, the marks on it, and what leaves.
//
// The picture is worked on in its own pixels. `compose` draws what will be
// saved (the backdrop, the picture cut to its crop, the marks, the watermark)
// in pixels of the output; the live canvas calls it under the zoom and Copy
// and Save call it on a canvas of exactly the output's size, so the display's
// scaling never reaches the file. 100 % is one pixel of the picture on one
// pixel of the display.
//
// The backdrop's padding and corners and the watermark's size and place are
// ported from reference/vorssaint-utils/Sources/Vorssaint/Services/QuickTools/ScreenshotSupport.swift
// (backdropPadding, cardCornerRadius, watermarkFontSize, watermarkPlacement).

import type { FeatureCtx } from "../../core/feature";
import { btn, field, iconBtn, slider } from "../../ui/components";
import { icon } from "../../ui/icons";
import { h } from "../../views/dom";
import { Board, fontOf, fontPx, ICONS, palette, sep, sizeButtons, swatches, token, toolRail, typeText, windowLabel, type Box, type Tool } from "./draw";

/** The settings keep the shape the recorder will share. */
interface Backdrop {
  kind: "none" | "preset";
  presetID: string;
  /** 0…1 */
  padding: number;
  cornerRadius: number;
}

interface Watermark {
  kind: "none" | "text";
  text: string;
  /** A place in the palette. */
  color: number;
  /** One of nine places, row by row from the top left. */
  anchor: number;
  /** 0…1 */
  size: number;
  opacity: number;
}

/** Backdrops: two colours of the design language, from the top left to the bottom right. */
const PRESETS: [id: string, name: string, from: string, to: string][] = [
  ["indigo", "Indigo", "--accent", "--info"],
  ["sunset", "Sunset", "--bad", "--warn"],
  ["mint", "Mint", "--ok", "--info"],
  ["candy", "Candy", "--accent", "--bad"],
  ["graphite", "Graphite", "--card-2", "--dim"],
];

/** What will be saved: its size, and where the picture sits in it. */
interface Out {
  w: number;
  h: number;
  pad: number;
  radius: number;
  crop: Box;
}

const clamp = (v: number, lo: number, hi: number) => Math.max(lo, Math.min(hi, v));

export function mountEditor(root: HTMLElement, cx: FeatureCtx) {
  const t = cx.t;
  const label = windowLabel();
  const colors = palette();

  let image: ImageBitmap | null = null;
  let backdrop: Backdrop = { kind: "none", presetID: PRESETS[0][0], padding: 0.5, cornerRadius: 0.3 };
  let stamp: Watermark = { kind: "none", text: "", color: 5, anchor: 8, size: 0.3, opacity: 0.4 };
  /** Pixels of the display per pixel of the output, and where the output's corner is on the canvas. */
  let [zoom, ox, oy] = [1, 0, 0];
  /** The picture is kept whole in view until the user zooms or pans. */
  let fitted = true;
  /** The crop being chosen (the crop tool is in hand). */
  let pending: Box | null = null;
  let typing = false;
  /** The open panel (backdrop, watermark) and the button that owns it. */
  let panel: { el: HTMLElement; owner: HTMLElement } | null = null;

  const board = new Board(() => changed());

  // ── The window ──────────────────────────────────────────────────────────────

  const view = h("canvas", { class: "f-editor-view" });
  const cropSize = h("span", { class: "f-editor-dims" });
  const cropBar = h(
    "div",
    { class: "f-editor-crop", hidden: true },
    cropSize,
    h("button", { class: "btn sm ghost", text: t("Whole picture"), onclick: () => setPending(whole()) }),
    h("button", { class: "btn sm ghost", text: t("Add space"), onclick: () => addSpace() }),
    h("button", { class: "btn sm ghost", text: t("Cancel"), onclick: () => setTool("select") }),
    h("button", { class: "btn sm primary", onclick: () => applyCrop() }, h("span", { text: t("Crop") }), h("span", { class: "kbd", text: "Enter" })),
  );
  const empty = h("div", { class: "f-editor-empty", hidden: true }, icon("image", 20), h("span", { text: t("The picture could not be opened.") }));
  const stage = h("div", { class: "f-editor-stage" }, view, cropBar, empty);

  const title = h("span", { text: t("Editor") });
  const undoBtn = iconBtn(ICONS.undo, `${t("Undo")} (Ctrl+Z)`, () => board.undo());
  const redoBtn = iconBtn(ICONS.redo, `${t("Redo")} (Ctrl+Shift+Z)`, () => board.redo());
  const copyBtn = btn(t("Copy"), "secondary", () => void send("copy"), "Ctrl+C");
  const saveBtn = btn(t("Save"), "primary", () => void send("save"), "Ctrl+S");
  const bar = h(
    "div",
    { class: "win-bar" },
    icon("image", 14),
    title,
    h("div", { class: "f-editor-acts" }, undoBtn, redoBtn, sep(), copyBtn, saveBtn),
    h("button", { class: "close", title: t("Close"), "aria-label": t("Close"), onclick: () => void cx.call("close", { label }), text: "×" }),
  );

  const rail = toolRail(t, ["select", null, "arrow", "curve", "line", "ruler", "rect", "ellipse", null, "pen", "highlight", "text", null, "blur", "spot", "magnify", "crop", null, "eraser"], setTool);
  const colorRow = swatches(t, (i) => {
    board.color = colors[i];
    board.restyle({ color: colors[i] });
    colorRow.show(i);
    void cx.settings.patch({ color: i });
  });
  const sizeRow = sizeButtons(t, (size) => {
    board.size = size;
    board.restyle({ size });
    sizeRow.show(size);
    void cx.settings.patch({ size });
  });
  const fillBtn = h("button", { class: "tab icon f-editor-tool", title: t("Fill the shapes"), "aria-label": t("Fill the shapes"), onclick: () => setFill(!board.fill) }, icon(ICONS.fill, 16));
  const small = (el: HTMLElement) => (el.classList.add("sm"), el);
  const backdropBtn = h("button", { class: "btn sm ghost", onclick: () => toggle(backdropBtn, backdropPanel) }, icon(ICONS.backdrop, 14), h("span", { text: t("Backdrop") }));
  const stampBtn = h("button", { class: "btn sm ghost", onclick: () => toggle(stampBtn, stampPanel) }, icon(ICONS.watermark, 14), h("span", { text: t("Watermark") }));
  const note = h("span", { class: "f-editor-note", role: "status" });
  const revealBtn = h("button", { class: "btn sm ghost", hidden: true, text: t("Show in folder"), onclick: () => void cx.call("reveal", { label }) });
  const dims = h("span", { class: "f-editor-dims", title: t("The size of the picture that is copied or saved") });
  const zoomBtn = h("button", { class: "btn sm ghost f-editor-zoom", title: `${t("Actual size")} (Ctrl+1)`, onclick: () => zoomAt(1) });
  const foot = h(
    "div",
    { class: "f-editor-foot" },
    colorRow.el,
    sep(),
    ...sizeRow.els,
    fillBtn,
    sep(),
    backdropBtn,
    stampBtn,
    h("span", { class: "isl-gap" }),
    note,
    revealBtn,
    dims,
    sep(),
    small(iconBtn(ICONS.minus, `${t("Zoom out")} (Ctrl+-)`, () => zoomAt(zoom / 1.25))),
    zoomBtn,
    small(iconBtn("plus", `${t("Zoom in")} (Ctrl++)`, () => zoomAt(zoom * 1.25))),
    small(iconBtn(ICONS.fit, `${t("Fit the window")} (Ctrl+0)`, () => fit())),
  );
  const win = h("div", { class: "win f-editor-win" }, bar, h("div", { class: "f-editor-body" }, h("div", { class: "f-editor-rail" }, ...rail.els), stage), foot);
  root.append(win);

  // ── What is drawn ───────────────────────────────────────────────────────────

  const whole = (): Box => ({ x: 0, y: 0, w: image?.width ?? 1, h: image?.height ?? 1 });
  /** The picture and the crop, which may reach beyond it (space added around): what the crop tool works on. */
  function area(): Box {
    const [a, c] = [whole(), board.crop ?? whole()];
    const [x, y] = [Math.min(a.x, c.x), Math.min(a.y, c.y)];
    return { x, y, w: Math.max(a.x + a.w, c.x + c.w) - x, h: Math.max(a.y + a.h, c.y + c.h) - y };
  }

  /** The output as it is on screen; `final` is as it leaves (no crop being chosen). */
  function out(final = false): Out {
    const choosing = !final && !!pending;
    const crop = choosing ? area() : (board.crop ?? whole());
    const short = Math.min(crop.w, crop.h);
    const on = backdrop.kind !== "none" && !choosing;
    // Even no padding keeps a frame, and a small picture keeps a visible one.
    const pad = on ? Math.round(Math.max(24, short * (0.035 + 0.14 * clamp(backdrop.padding, 0, 1)))) : 0;
    const radius = on ? Math.round(clamp(backdrop.cornerRadius, 0, 1) * short * 0.2) : 0;
    return { w: crop.w + pad * 2, h: crop.h + pad * 2, pad, radius, crop };
  }

  /** Paints the output in its own pixels. `k` is the scale it is seen at, for what does not scale by itself. */
  function compose(c: CanvasRenderingContext2D, o: Out, k: number, final: boolean) {
    if (!image) return;
    const { pad, crop, radius } = o;
    const card = () => {
      c.beginPath();
      if (radius) c.roundRect(pad, pad, crop.w, crop.h, radius);
      else c.rect(pad, pad, crop.w, crop.h);
    };
    if (pad) {
      const preset = PRESETS.find((p) => p[0] === backdrop.presetID) ?? PRESETS[0];
      const fill = c.createLinearGradient(0, 0, o.w, o.h);
      fill.addColorStop(0, token(preset[2]));
      fill.addColorStop(1, token(preset[3]));
      c.fillStyle = fill;
      c.fillRect(0, 0, o.w, o.h);
      // The shadow under the picture.
      c.save();
      c.globalAlpha = 0.45;
      c.shadowColor = c.fillStyle = token("--bg");
      c.shadowBlur = pad * 0.6 * k;
      c.shadowOffsetY = pad * 0.18 * k;
      card();
      c.fill();
      c.restore();
    }
    c.save();
    card();
    c.clip();
    // The padding is whole pixels: the picture's own are not resampled.
    c.translate(pad - crop.x, pad - crop.y);
    c.drawImage(image, 0, 0);
    board.paint(c, image);
    c.restore();
    if (stamp.kind === "text" && stamp.text.trim() && (final || !pending)) watermark(c, o);
  }

  /** The watermark on one of nine places of the picture, inside a margin that clears rounded corners. */
  function watermark(c: CanvasRenderingContext2D, o: Out) {
    const { crop, pad, radius } = o;
    const short = Math.min(crop.w, crop.h);
    const px = Math.max(8, Math.round(short * (0.02 + 0.14 * clamp(stamp.size, 0, 1))));
    const text = stamp.text.trim().slice(0, 120);
    c.save();
    c.font = fontOf(px);
    const [bw, bh] = [c.measureText(text).width, px * 1.2];
    const margin = Math.min(short * 0.45, Math.max(short * 0.05, radius ? Math.ceil(radius * (1 - Math.SQRT1_2)) + 1 : 0));
    const [aw, ah] = [crop.w - margin * 2, crop.h - margin * 2];
    // A text wider than the picture is made to fit it.
    const fits = Math.min(1, aw / bw, ah / bh);
    const [ux, uy] = [(stamp.anchor % 3) / 2, Math.floor(stamp.anchor / 3) / 2];
    c.translate(pad + margin + (aw - bw * fits) * ux + (bw * fits) / 2, pad + margin + (ah - bh * fits) * uy + (bh * fits) / 2);
    c.scale(fits, fits);
    c.globalAlpha = clamp(stamp.opacity, 0.05, 1);
    c.fillStyle = colors[stamp.color] ?? token("--ink");
    c.textAlign = "center";
    c.textBaseline = "middle";
    c.fillText(text, 0, 0);
    c.restore();
  }

  let queued = false;
  function paint() {
    if (queued) return;
    queued = true;
    requestAnimationFrame(() => {
      queued = false;
      if (!image) return;
      const c = view.getContext("2d")!;
      const o = out();
      c.setTransform(1, 0, 0, 1, 0, 0);
      c.clearRect(0, 0, view.width, view.height);
      c.setTransform(zoom, 0, 0, zoom, ox, oy);
      // Smaller than life it is smoothed; at 100 % and above a pixel is a pixel.
      c.imageSmoothingEnabled = zoom < 1;
      c.imageSmoothingQuality = "high";
      compose(c, o, zoom, false);
      // What is not part of the picture: the selected mark, the crop, the edge of the output.
      c.translate(o.pad - o.crop.x, o.pad - o.crop.y);
      board.outline(c, zoom);
      if (pending) {
        const all = area();
        c.save();
        c.globalAlpha = 0.6;
        c.fillStyle = token("--bg");
        c.beginPath();
        c.rect(all.x, all.y, all.w, all.h);
        c.rect(pending.x, pending.y, pending.w, pending.h);
        c.fill("evenodd");
        c.restore();
        c.strokeStyle = token("--ink");
        c.lineWidth = 1 / zoom;
        c.strokeRect(pending.x, pending.y, pending.w, pending.h);
      }
      c.setTransform(1, 0, 0, 1, 0, 0);
      c.strokeStyle = token("--outline");
      c.lineWidth = 1;
      c.strokeRect(ox - 0.5, oy - 0.5, Math.round(o.w * zoom) + 1, Math.round(o.h * zoom) + 1);
    });
  }

  // ── The view: zoom and place ────────────────────────────────────────────────

  /** A view that shows less than the whole output keeps it from leaving; one that shows all of it centres it. */
  function settle() {
    const o = out();
    const [w, h] = [o.w * zoom, o.h * zoom];
    ox = w <= view.width ? Math.round((view.width - w) / 2) : clamp(ox, Math.round(view.width - w), 0);
    oy = h <= view.height ? Math.round((view.height - h) / 2) : clamp(oy, Math.round(view.height - h), 0);
  }

  function fit() {
    const o = out();
    const room = 24 * (window.devicePixelRatio || 1);
    // Never larger than life: a small picture is not blown up.
    zoom = clamp(Math.min((view.width - room * 2) / o.w, (view.height - room * 2) / o.h), 0.02, 1);
    fitted = true;
    settle();
    sync();
    paint();
  }

  /** A new zoom that keeps the pixel at (vx, vy) of the canvas where it is. */
  function zoomAt(next: number, vx = view.width / 2, vy = view.height / 2) {
    next = clamp(next, 0.05, 32);
    // Near 100 % is 100 %: only there is a pixel a pixel.
    if (Math.abs(next - 1) < 0.04) next = 1;
    [ox, oy] = [Math.round(vx - ((vx - ox) / zoom) * next), Math.round(vy - ((vy - oy) / zoom) * next)];
    zoom = next;
    fitted = false;
    settle();
    sync();
    paint();
  }

  /** The canvas has exactly the pixels of the display it covers, at any scaling. */
  new ResizeObserver(([entry]) => {
    const box = entry.devicePixelContentBoxSize?.[0];
    const k = window.devicePixelRatio || 1;
    const [w, h] = box ? [box.inlineSize, box.blockSize] : [Math.round(entry.contentRect.width * k), Math.round(entry.contentRect.height * k)];
    if (!w || !h || (view.width === w && view.height === h)) return;
    [view.width, view.height] = [w, h];
    if (fitted) fit();
    else {
      settle();
      paint();
    }
  }).observe(view, { box: "device-pixel-content-box" });

  let shape = "";
  /** Something of the document changed: the output may have a new size. */
  function changed() {
    const o = out();
    const now = `${o.w}×${o.h}`;
    if (now !== shape && image) {
      shape = now;
      if (fitted) return fit();
      settle();
    }
    sync();
    paint();
  }

  function sync() {
    undoBtn.disabled = !board.canUndo;
    redoBtn.disabled = !board.canRedo;
    const o = out(true);
    dims.textContent = image ? `${o.w} × ${o.h}` : "";
    zoomBtn.textContent = `${Math.round(zoom * 100)} %`;
    cropSize.textContent = pending ? `${pending.w} × ${pending.h}` : "";
  }

  // ── Tools ───────────────────────────────────────────────────────────────────

  function setTool(tool: Tool) {
    board.tool = tool;
    if (tool !== "select") board.selected = -1;
    rail.show(tool);
    view.style.cursor = tool === "select" ? "default" : tool === "text" ? "text" : "crosshair";
    // The crop is chosen on the whole picture, without the backdrop.
    pending = tool === "crop" ? { ...(board.crop ?? whole()) } : null;
    cropBar.hidden = !pending;
    changed();
  }

  function setPending(box: Box) {
    pending = box;
    sync();
    paint();
  }

  /** Grows the picture's frame on every side; the new space is see-through. debt: no choice of fill colour. */
  function addSpace() {
    const c = board.crop ?? whole();
    const n = Math.max(32, Math.round(Math.min(c.w, c.h) * 0.1));
    const next = { x: c.x - n, y: c.y - n, w: c.w + n * 2, h: c.h + n * 2 };
    board.set(board.marks, next);
    setPending(next);
  }

  function setFill(on: boolean) {
    board.fill = on;
    board.restyle({ fill: on });
    fillBtn.classList.toggle("on", on);
    void cx.settings.patch({ fill: on });
  }

  function applyCrop() {
    if (!pending) return;
    const all = whole();
    const next = pending.w < 1 || pending.h < 1 || (pending.w === all.w && pending.h === all.h) ? null : pending;
    if (JSON.stringify(next) !== JSON.stringify(board.crop)) board.set(board.marks, next);
    setTool("select");
  }

  /** The text tool: a field on the canvas where the click was, in the size and colour it will have. */
  function write(x: number, y: number) {
    const o = out();
    const k = view.width / view.getBoundingClientRect().width;
    const [color, size] = [board.color, board.size];
    const css = (v: number, origin: number, shift: number) => (origin + (v + shift) * zoom) / k;
    typing = true;
    typeText(stage, { left: css(x, ox, o.pad - o.crop.x), top: css(y, oy, o.pad - o.crop.y), px: (fontPx(size) * zoom) / k, color }, (text) => {
      typing = false;
      if (text) board.set([...board.marks, { tool: "text", color, size, pts: [x, y], text }]);
    });
  }

  // ── The pointer ─────────────────────────────────────────────────────────────

  /** The pointer as a pixel of the picture. */
  function at(e: PointerEvent): [number, number] {
    const r = view.getBoundingClientRect();
    const k = view.width / r.width;
    const o = out();
    return [((e.clientX - r.left) * k - ox) / zoom - o.pad + o.crop.x, ((e.clientY - r.top) * k - oy) / zoom - o.pad + o.crop.y];
  }
  /** How near a mark a click counts as on it: six pixels of the display. */
  const reach = () => 6 / zoom;
  const inside = (x: number, y: number): [number, number] => {
    const a = area();
    return [clamp(Math.round(x), a.x, a.x + a.w), clamp(Math.round(y), a.y, a.y + a.h)];
  };

  let pan: { x: number; y: number; ox: number; oy: number } | null = null;
  let cropFrom: [number, number] | null = null;
  let space = false;

  view.addEventListener("pointerdown", (e) => {
    if (!image || typing) return;
    closePanel();
    if (e.button === 1 || (e.button === 0 && space)) {
      pan = { x: e.clientX, y: e.clientY, ox, oy };
      view.setPointerCapture(e.pointerId);
      return e.preventDefault();
    }
    if (e.button !== 0) return;
    const [x, y] = at(e);
    if (board.tool === "text") {
      e.preventDefault();
      return write(Math.round(x), Math.round(y));
    }
    view.setPointerCapture(e.pointerId);
    if (board.tool === "crop") {
      cropFrom = inside(x, y);
      return setPending({ x: cropFrom[0], y: cropFrom[1], w: 0, h: 0 });
    }
    board.down(x, y, reach());
  });
  view.addEventListener("pointermove", (e) => {
    if (pan) {
      const k = view.width / view.getBoundingClientRect().width;
      [ox, oy] = [Math.round(pan.ox + (e.clientX - pan.x) * k), Math.round(pan.oy + (e.clientY - pan.y) * k)];
      fitted = false;
      settle();
      paint();
    } else if (cropFrom) {
      const [x, y] = inside(...at(e));
      setPending({ x: Math.min(x, cropFrom[0]), y: Math.min(y, cropFrom[1]), w: Math.abs(x - cropFrom[0]), h: Math.abs(y - cropFrom[1]) });
    } else board.move(...at(e), reach());
  });
  const release = () => {
    // A click without a drag leaves the crop as it was.
    if (cropFrom && pending && (pending.w < 2 || pending.h < 2)) setPending({ ...(board.crop ?? whole()) });
    [pan, cropFrom] = [null, null];
    board.up();
  };
  view.addEventListener("pointerup", release);
  view.addEventListener("pointercancel", release);
  view.addEventListener(
    "wheel",
    (e) => {
      e.preventDefault();
      const r = view.getBoundingClientRect();
      const k = view.width / r.width;
      if (e.ctrlKey) return zoomAt(zoom * Math.exp(-e.deltaY / 400), (e.clientX - r.left) * k, (e.clientY - r.top) * k);
      [ox, oy] = [ox - Math.round((e.shiftKey ? e.deltaY : e.deltaX) * k), oy - Math.round((e.shiftKey ? 0 : e.deltaY) * k)];
      fitted = false;
      settle();
      paint();
    },
    { passive: false },
  );

  // Ctrl+V with a picture on the clipboard: a layer in the middle of the frame, to move and size by its marks.
  window.addEventListener("paste", (e) => {
    const file = [...(e.clipboardData?.files ?? [])].find((f) => f.type.startsWith("image/"));
    if (!file || !image || typing || (e.target as HTMLElement).closest?.("input, textarea")) return;
    e.preventDefault();
    void createImageBitmap(file).then((img) => {
      const c = board.crop ?? whole();
      const k = Math.min(1, (c.w * 0.6) / img.width, (c.h * 0.6) / img.height);
      const [w, h] = [Math.max(1, Math.round(img.width * k)), Math.max(1, Math.round(img.height * k))];
      const [x, y] = [Math.round(c.x + (c.w - w) / 2), Math.round(c.y + (c.h - h) / 2)];
      const marks = [...board.marks, { tool: "image" as const, color: "", size: 1, pts: [x, y, x + w, y + h], img }];
      board.set(marks);
      setTool("select");
      board.selected = marks.length - 1;
      paint();
    });
  });

  // ── Keys ────────────────────────────────────────────────────────────────────

  window.addEventListener("keyup", (e) => {
    if (e.key === " ") space = false;
  });
  window.addEventListener("keydown", (e) => {
    // A field of a panel is being typed in.
    if ((e.target as HTMLElement).closest?.("input, textarea")) return;
    const key = e.key.toLowerCase();
    if (e.ctrlKey || e.metaKey) {
      if (key === "z") e.shiftKey ? board.redo() : board.undo();
      else if (key === "y") board.redo();
      else if (key === "c") void send("copy");
      else if (key === "s") void send("save");
      else if (key === "0") fit();
      else if (key === "1") zoomAt(1);
      else if (key === "=" || key === "+") zoomAt(zoom * 1.25);
      else if (key === "-") zoomAt(zoom / 1.25);
      else return;
      return e.preventDefault();
    }
    if (e.altKey) return;
    const tool = rail.byKey(key);
    if (key === " ") space = true;
    // Esc backs out one level: a panel, the crop, the selection.
    else if (key === "escape") {
      if (panel) closePanel();
      else if (pending) setTool("select");
      else {
        board.selected = -1;
        paint();
      }
    } else if (key === "enter" && pending) applyCrop();
    else if (key === "delete" || key === "backspace") board.remove();
    else if (tool) setTool(tool);
    else return;
    e.preventDefault();
  });

  // ── Backdrop and watermark ──────────────────────────────────────────────────

  function closePanel() {
    panel?.el.remove();
    panel?.owner.classList.remove("is-active");
    panel = null;
  }

  /** Opens a panel above the button that owns it, or closes it when it is that button's. */
  function toggle(owner: HTMLElement, build: () => HTMLElement[]) {
    const mine = panel?.owner === owner;
    closePanel();
    if (mine) return;
    const el = h("div", { class: "f-editor-panel fx-in" }, ...build());
    const [b, w] = [owner.getBoundingClientRect(), win.getBoundingClientRect()];
    el.style.left = `${b.left - w.left}px`;
    el.style.bottom = `${w.bottom - b.top + 6}px`;
    win.append(el);
    owner.classList.add("is-active");
    panel = { el, owner };
    // Marks what is on in the panel that is now there.
    setBackdrop({}, false);
    setStamp({}, false);
  }

  const option = (name: string, control: HTMLElement) => h("label", { class: "f-editor-option" }, h("span", { text: name }), control);
  const dial = (name: string, value: number, set: (v: number, keep: boolean) => void) =>
    option(name, slider(value, { label: name, onInput: (v) => set(v, false), onChange: (v) => set(v, true) }));

  /** `keep` is false while a slider is dragged: the view follows, the settings wait. */
  function setBackdrop(patch: Partial<Backdrop>, keep = true) {
    backdrop = { ...backdrop, ...patch };
    if (keep) void cx.settings.patch({ backdrop });
    backdropBtn.classList.toggle("on", backdrop.kind !== "none");
    for (const el of panel?.el.querySelectorAll<HTMLElement>(".f-editor-fill") ?? []) {
      el.classList.toggle("on", el.dataset.id === (backdrop.kind === "none" ? "" : backdrop.presetID));
    }
    changed();
  }

  function backdropPanel() {
    const fills = h(
      "div",
      { class: "f-editor-fills" },
      h("button", { class: "f-editor-fill none", title: t("None"), "aria-label": t("None"), "data-id": "", onclick: () => setBackdrop({ kind: "none" }) }),
      ...PRESETS.map(([id, name, from, to]) =>
        h("button", {
          class: "f-editor-fill",
          title: t(name),
          "aria-label": t(name),
          "data-id": id,
          style: `--a:var(${from});--b:var(${to})`,
          onclick: () => setBackdrop({ kind: "preset", presetID: id }),
        }),
      ),
    );
    return [
      fills,
      dial(t("Padding"), backdrop.padding, (v, keep) => setBackdrop({ padding: v }, keep)),
      dial(t("Corners"), backdrop.cornerRadius, (v, keep) => setBackdrop({ cornerRadius: v }, keep)),
    ];
  }

  function setStamp(patch: Partial<Watermark>, keep = true) {
    stamp = { ...stamp, ...patch };
    stamp.kind = stamp.text.trim() ? "text" : "none";
    if (keep) void cx.settings.patch({ watermark: stamp });
    stampBtn.classList.toggle("on", stamp.kind !== "none");
    panel?.el.querySelectorAll(".f-editor-place").forEach((el, i) => el.classList.toggle("on", i === stamp.anchor));
    paint();
  }

  function stampPanel() {
    const text = field({ value: stamp.text, placeholder: t("Your name or address"), onInput: (v) => setStamp({ text: v.slice(0, 120) }, false) });
    text.input.maxLength = 120;
    text.input.addEventListener("change", () => setStamp({}));
    const places = h(
      "div",
      { class: "f-editor-places", role: "radiogroup", "aria-label": t("Place") },
      ...Array.from({ length: 9 }, (_, i) => h("button", { class: "f-editor-place", role: "radio", "aria-label": t("Position {n} of 9, from the top left", { n: i + 1 }), onclick: () => setStamp({ anchor: i }) })),
    );
    const ink = swatches(t, (i) => {
      setStamp({ color: i });
      ink.show(i);
    });
    ink.show(stamp.color);
    return [
      text,
      h("div", { class: "f-editor-option" }, h("span", { text: t("Place") }), places),
      dial(t("Size"), stamp.size, (v, keep) => setStamp({ size: v }, keep)),
      dial(t("Opacity"), stamp.opacity, (v, keep) => setStamp({ opacity: v }, keep)),
      h("div", { class: "f-editor-option" }, h("span", { text: t("Color") }), ink.el),
    ];
  }

  // ── Out ─────────────────────────────────────────────────────────────────────

  let noteTimer = 0;
  function say(text: string, ms = 0) {
    window.clearTimeout(noteTimer);
    note.textContent = text;
    if (ms) noteTimer = window.setTimeout(() => say(""), ms);
  }

  let busy = false;
  /** Copy or Save: the output at exactly its own size, as a PNG, handed to Rust. */
  async function send(action: "copy" | "save") {
    if (!image || busy) return;
    busy = true;
    // A text still being typed is part of the picture.
    (document.activeElement as HTMLElement | null)?.blur?.();
    const o = out(true);
    const canvas = h("canvas", { width: o.w, height: o.h });
    compose(canvas.getContext("2d")!, o, 1, true);
    say(action === "copy" ? t("Copying…") : t("Saving…"));
    const done = await cx.call<{ path?: string; w: number; h: number }>("export", { label, action, png: canvas.toDataURL("image/png").split(",")[1] }).finally(() => (busy = false));
    if (!done) return say(action === "copy" ? t("It could not be copied.") : t("It could not be saved. Check the folder in Settings › Capture."), 6000);
    if (action === "copy") return say(t("Copied"), 2500);
    revealBtn.hidden = false;
    say(t("Saved to {folder}", { folder: (done.path ?? "").replace(/[\\/][^\\/]*$/, "") }));
  }

  // ── In ──────────────────────────────────────────────────────────────────────

  setTool("arrow");
  sync();
  void (async () => {
    const [settings, doc] = await Promise.all([cx.settings.get(), cx.call<{ name: string; url: string }>("doc", { label })]);
    backdrop = { ...backdrop, ...(settings.backdrop as Partial<Backdrop> | undefined) };
    stamp = { ...stamp, ...(settings.watermark as Partial<Watermark> | undefined) };
    const color = clamp(Number(settings.color) || 0, 0, colors.length - 1);
    [board.color, board.size] = [colors[color], Number(settings.size) || 4];
    colorRow.show(color);
    sizeRow.show(board.size);
    board.fill = settings.fill === true;
    fillBtn.classList.toggle("on", board.fill);
    backdropBtn.classList.toggle("on", backdrop.kind !== "none");
    stampBtn.classList.toggle("on", stamp.kind !== "none");
    try {
      if (!doc) throw new Error("no picture");
      const picture = new Image();
      picture.src = doc.url;
      await picture.decode();
      // The font of the text tool and the watermark, before either is drawn or saved.
      await document.fonts.load(fontOf(20)).catch(() => {});
      image = await createImageBitmap(picture);
      title.textContent = doc.name;
      changed();
    } catch {
      empty.hidden = false;
      view.hidden = true;
    }
  })();
}
