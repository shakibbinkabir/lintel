// Marks — what the editor and the drawing on the screen are both made of.
//
// A mark's geometry is in pixels of its document (the picture, or the screen).
// One `drawMark` paints the live canvas under any zoom and the export at 1:1,
// so what is saved is what was drawn. A line of an odd width sits on pixel
// centres and one of an even width on pixel edges: a one-pixel line is one
// pixel in the file, whatever the display's scaling.
//
// The arrow's shape and the size of the mosaic's blocks are ported from
// reference/vorssaint-utils/Sources/Vorssaint/Services/QuickTools/ScreenshotSupport.swift
// (arrowHead, arrowSilhouette, pixelBlockSize).

import type { FeatureCtx } from "../../core/feature";
import { icon } from "../../ui/icons";
import { h } from "../../views/dom";

export type Shape = "arrow" | "curve" | "line" | "ruler" | "rect" | "ellipse" | "pen" | "highlight" | "text" | "blur" | "spot" | "magnify";
export type Tool = Shape | "select" | "crop" | "eraser";

export interface Mark {
  /** "image" is a pasted picture: it has no tool in hand, it is placed and moved. */
  tool: Shape | "image";
  color: string;
  /** Line width in pixels of the document; a text's size follows from it. */
  size: number;
  /** x, y pairs: two ends, two corners, a text's top-left corner, or a path. */
  pts: number[];
  text?: string;
  /** A rectangle or an ellipse filled as well as outlined. */
  fill?: boolean;
  /** The pasted picture of an "image", in the box of `pts`. */
  img?: ImageBitmap;
}

export interface Box {
  x: number;
  y: number;
  w: number;
  h: number;
}

/** The picture under the marks: what the blur reads. */
export type Base = ImageBitmap | HTMLCanvasElement;
type Ctx = CanvasRenderingContext2D;
type T = FeatureCtx["t"];

// ── The look: icons of the feature's own, colours and sizes ───────────────────

/** 24×24, one path, drawn with the shared stroke (src/ui/icons.ts). */
export const ICONS = {
  select: "M5.5 4l13 5.4-5.5 2-2 5.6z",
  arrow: "M5 19L19 5M10.5 5H19v8.5",
  line: "M5 19L19 5",
  rect: "M5 6h14a1 1 0 0 1 1 1v10a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1z",
  ellipse: "M12 5.5c4.7 0 8.5 2.9 8.5 6.5s-3.8 6.5-8.5 6.5S3.5 15.6 3.5 12 7.3 5.5 12 5.5z",
  pen: "M4 20l1-4.5L16.5 4a2.1 2.1 0 0 1 3 3L8 18.5zM14.5 6l3 3",
  highlight: "M14.5 4l5.5 5.5-8 8H7.5l-1.5-1.5v-4zM4.5 20.5h15",
  text: "M5 7V5h14v2M12 5v14M9.5 19h5",
  blur: "M4 4h5.3v5.3H4zM14.7 4H20v5.3h-5.3zM9.3 9.3h5.4v5.4H9.3zM4 14.7h5.3V20H4zM14.7 14.7H20V20h-5.3z",
  curve: "M5 19C6 8 14 5 19 5M13.5 4.5L19 5l-1 5.5",
  spot: "M3.5 3.5h17v17h-17zM8.5 8.5h7v7h-7z",
  ruler: "M3.5 8h17v8h-17zM7 8v3.5M10.5 8v2.5M14 8v3.5M17.5 8v2.5",
  magnify: "M10.5 4a6.5 6.5 0 1 1 0 13 6.5 6.5 0 0 1 0-13zM15.5 15.5L20 20",
  fill: "M5 6h14a1 1 0 0 1 1 1v10a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1z",
  crop: "M7 3.5V17h13.5M3.5 7H17v13.5",
  eraser: "M9 19.5l-4.5-4.5a1.4 1.4 0 0 1 0-2l8-8a1.4 1.4 0 0 1 2 0l5 5a1.4 1.4 0 0 1 0 2L12 19.5zM9 9l6 6M5 19.5h15",
  undo: "M9 14.5L4.5 10 9 5.5M4.5 10H14a5 5 0 0 1 0 10h-3",
  redo: "M15 14.5l4.5-4.5L15 5.5M19.5 10H10a5 5 0 0 0 0 10h3",
  backdrop: "M4 5h16a1 1 0 0 1 1 1v12a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1V6a1 1 0 0 1 1-1zM8 9h8v6H8z",
  watermark: "M10 13.5c0-2-1.5-3-1.5-5a3.5 3.5 0 0 1 7 0c0 2-1.5 3-1.5 5M5.5 17.5v-3a1 1 0 0 1 1-1h11a1 1 0 0 1 1 1v3zM5.5 20.5h13",
  minus: "M5 12h14",
  fit: "M4 9V5a1 1 0 0 1 1-1h4M15 4h4a1 1 0 0 1 1 1v4M20 15v4a1 1 0 0 1-1 1h-4M9 20H5a1 1 0 0 1-1-1v-4",
  scroll: "M7 4h10a1 1 0 0 1 1 1v14a1 1 0 0 1-1 1H7a1 1 0 0 1-1-1V5a1 1 0 0 1 1-1zM12 8v8M9.5 13.5L12 16l2.5-2.5",
} as const;

/** Each tool's name (a key of strings.json) and the key that picks it. */
const TOOL: Record<Tool, [title: string, key: string]> = {
  select: ["Select and move", "V"],
  arrow: ["Arrow", "A"],
  line: ["Line", "L"],
  rect: ["Rectangle", "R"],
  ellipse: ["Ellipse", "O"],
  pen: ["Pen", "P"],
  highlight: ["Highlighter", "H"],
  text: ["Text", "T"],
  blur: ["Blur", "B"],
  crop: ["Crop", "C"],
  eraser: ["Eraser", "E"],
  curve: ["Curved arrow", "D"],
  ruler: ["Ruler", "U"],
  spot: ["Spotlight", "S"],
  magnify: ["Magnifier", "M"],
};

const tokens = new Map<string, string>();

/** A colour of the design language as the canvas takes it (src/ui/tokens.css). */
export function token(name: string): string {
  let value = tokens.get(name);
  if (value === undefined) tokens.set(name, (value = getComputedStyle(document.documentElement).getPropertyValue(name).trim()));
  return value;
}

/** The colours to draw in, and what each is called. */
const PALETTE: [token: string, name: string][] = [
  ["--bad", "Red"],
  ["--warn", "Amber"],
  ["--ok", "Green"],
  ["--info", "Cyan"],
  ["--accent", "Blue"],
  ["--ink", "White"],
  ["--bg", "Black"],
];

export const palette = (): string[] => PALETTE.map(([name]) => token(name));

/** Line widths, in pixels of the document. */
export const SIZES = [1, 2, 4, 8];

/** Where a line of this width has to sit to cover whole pixels. */
export const snap = (v: number, size: number) => (size % 2 ? Math.floor(v) + 0.5 : Math.round(v));

/** A text is as tall as its line is thick. */
export const fontPx = (size: number) => 12 + size * 4;
/** The height of a line of text, in font sizes. */
const LEADING = 1.25;
const markerPx = (size: number) => size * 4 + 8;

export const fontOf = (px: number) => `600 ${px}px ${token("--font") || "sans-serif"}`;

/** The label of the window this page runs in. */
export function windowLabel(): string {
  const tauri = (window as unknown as { __TAURI_INTERNALS__?: { metadata?: { currentWindow?: { label?: string } } } }).__TAURI_INTERNALS__;
  return tauri?.metadata?.currentWindow?.label ?? "";
}

// ── Drawing a mark ────────────────────────────────────────────────────────────

/** One outline for the whole arrow: shaft, round tail and head. */
function arrowPath(c: Ctx, p: number[], width: number) {
  const [tx, ty, x, y] = p;
  const angle = Math.atan2(y - ty, x - tx);
  const distance = Math.hypot(x - tx, y - ty);
  // The head grows with the line, and never outgrows a short arrow.
  const length = Math.min(Math.max(10, width * 3.4), Math.max(distance * 0.72, Math.min(width * 1.2, distance)));
  const spread = Math.PI / 7;
  const [lx, ly] = [x - length * Math.cos(angle - spread), y - length * Math.sin(angle - spread)];
  const [rx, ry] = [x - length * Math.cos(angle + spread), y - length * Math.sin(angle + spread)];
  const [mx, my] = [(lx + rx) / 2, (ly + ry) / 2];
  const [px, py] = [(-Math.sin(angle) * width) / 2, (Math.cos(angle) * width) / 2];
  c.beginPath();
  c.moveTo(tx + px, ty + py);
  c.lineTo(mx + px, my + py);
  c.lineTo(lx, ly);
  c.lineTo(x, y);
  c.lineTo(rx, ry);
  c.lineTo(mx - px, my - py);
  c.lineTo(tx - px, ty - py);
  c.arc(tx, ty, width / 2, angle - Math.PI / 2, angle + Math.PI / 2, true);
  c.closePath();
}

/** A mosaic of the picture is kept with its mark: a mark never changes, it is replaced. */
const mosaics = new WeakMap<Mark, HTMLCanvasElement>();

function mosaic(c: Ctx, m: Mark, base: Base) {
  const b = bounds(m);
  if (b.w < 1 || b.h < 1) return;
  // Blocks large enough that nothing under them can be read, on a small crop as on a whole screen.
  const block = Math.max(10, Math.floor(Math.min(base.width, base.height) / 55));
  let tile = mosaics.get(m);
  if (!tile) {
    tile = h("canvas", { width: Math.ceil(b.w / block), height: Math.ceil(b.h / block) });
    const tc = tile.getContext("2d")!;
    tc.imageSmoothingQuality = "high";
    // One pixel of the tile for each block of the picture.
    tc.drawImage(base, b.x, b.y, tile.width * block, tile.height * block, 0, 0, tile.width, tile.height);
    mosaics.set(m, tile);
  }
  c.beginPath();
  c.rect(b.x, b.y, b.w, b.h);
  c.clip();
  c.imageSmoothingEnabled = false;
  c.drawImage(tile, b.x, b.y, tile.width * block, tile.height * block);
}

/** The control point that bends a curved arrow to one side of its chord. */
function bend(p: number[]): [number, number] {
  return [(p[0] + p[2]) / 2 - (p[3] - p[1]) * 0.25, (p[1] + p[3]) / 2 + (p[2] - p[0]) * 0.25];
}

/** A round window on the picture, twice as large. */
function magnify(c: Ctx, m: Mark, base: Base) {
  const [cx, cy] = [(m.pts[0] + m.pts[2]) / 2, (m.pts[1] + m.pts[3]) / 2];
  const [rx, ry] = [Math.abs(m.pts[2] - m.pts[0]) / 2, Math.abs(m.pts[3] - m.pts[1]) / 2];
  if (rx < 1 || ry < 1) return;
  c.save();
  c.beginPath();
  c.ellipse(cx, cy, rx, ry, 0, 0, Math.PI * 2);
  c.clip();
  c.drawImage(base, -cx, -cy, base.width * 2, base.height * 2);
  c.restore();
  c.beginPath();
  c.ellipse(cx, cy, rx, ry, 0, 0, Math.PI * 2);
  c.stroke();
}

/** A measured line: ticks at both ends and its length in pixels of the picture. */
function measure(c: Ctx, m: Mark) {
  const [x0, y0, x1, y1] = m.pts;
  const len = Math.hypot(x1 - x0, y1 - y0);
  const a = Math.atan2(y1 - y0, x1 - x0) + Math.PI / 2;
  const tick = 5 + m.size * 1.5;
  c.beginPath();
  c.moveTo(x0, y0);
  c.lineTo(x1, y1);
  for (const [x, y] of [[x0, y0], [x1, y1]]) {
    c.moveTo(x + Math.cos(a) * tick, y + Math.sin(a) * tick);
    c.lineTo(x - Math.cos(a) * tick, y - Math.sin(a) * tick);
  }
  c.stroke();
  if (len < 2) return;
  c.font = fontOf(10 + m.size * 2);
  c.textAlign = "center";
  c.textBaseline = "bottom";
  c.lineWidth = 4;
  c.strokeStyle = token("--bg");
  const [mx, my] = [(x0 + x1) / 2, (y0 + y1) / 2 - tick - 2];
  c.strokeText(`${Math.round(len)} px`, mx, my);
  c.fillText(`${Math.round(len)} px`, mx, my);
}

/** Paints one mark. `base` is the picture under it: none on the screen. */
export function drawMark(c: Ctx, m: Mark, base?: Base | null) {
  const p = m.pts;
  c.save();
  c.lineWidth = m.size;
  c.lineCap = c.lineJoin = "round";
  c.strokeStyle = c.fillStyle = m.color;
  if (m.tool === "rect") {
    c.lineJoin = "miter";
    const [x, y, w, h] = [Math.min(p[0], p[2]), Math.min(p[1], p[3]), Math.abs(p[2] - p[0]), Math.abs(p[3] - p[1])];
    if (m.fill) {
      c.globalAlpha = 0.3;
      c.fillRect(x, y, w, h);
      c.globalAlpha = 1;
    }
    c.strokeRect(x, y, w, h);
  } else if (m.tool === "ellipse") {
    c.beginPath();
    c.ellipse((p[0] + p[2]) / 2, (p[1] + p[3]) / 2, Math.abs(p[2] - p[0]) / 2, Math.abs(p[3] - p[1]) / 2, 0, 0, Math.PI * 2);
    if (m.fill) {
      c.globalAlpha = 0.3;
      c.fill();
      c.globalAlpha = 1;
    }
    c.stroke();
  } else if (m.tool === "image") {
    if (m.img) c.drawImage(m.img, Math.min(p[0], p[2]), Math.min(p[1], p[3]), Math.abs(p[2] - p[0]), Math.abs(p[3] - p[1]));
  } else if (m.tool === "magnify") {
    if (base) magnify(c, m, base);
  } else if (m.tool === "ruler") {
    measure(c, m);
  } else if (m.tool === "curve") {
    const [c1x, c1y] = bend(p);
    const head = Math.min(Math.max(10, m.size * 3.4), Math.hypot(p[2] - p[0], p[3] - p[1]) * 0.5) * 1.4;
    const a = Math.atan2(p[3] - c1y, p[2] - c1x);
    // The shaft stops inside the head; the head is the arrow's own.
    c.beginPath();
    c.moveTo(p[0], p[1]);
    c.quadraticCurveTo(c1x, c1y, p[2] - Math.cos(a) * head * 0.6, p[3] - Math.sin(a) * head * 0.6);
    c.stroke();
    arrowPath(c, [p[2] - Math.cos(a) * head, p[3] - Math.sin(a) * head, p[2], p[3]], m.size);
    c.fill();
  } else if (m.tool === "spot") {
    // The dimming is the board's: one for all the spotlights.
  } else if (m.tool === "arrow") {
    arrowPath(c, p, m.size);
    c.fill();
  } else if (m.tool === "text") {
    const px = fontPx(m.size);
    c.font = fontOf(px);
    // Each line sits in its box as a line of the text field does.
    const { fontBoundingBoxAscent: up, fontBoundingBoxDescent: down } = c.measureText("M");
    const top = (px * LEADING - (up + down)) / 2 + up;
    (m.text ?? "").split("\n").forEach((line, i) => c.fillText(line, p[0], p[1] + i * px * LEADING + top));
  } else if (m.tool === "blur") {
    if (base) mosaic(c, m, base);
  } else {
    // A line, the pen, the highlighter: one path through the points, smoothed between them.
    if (m.tool === "highlight") {
      c.globalAlpha = 0.42;
      // Over a picture the ink darkens what is under it, as a marker does.
      if (base) c.globalCompositeOperation = "multiply";
      c.lineWidth = markerPx(m.size);
    }
    c.beginPath();
    c.moveTo(p[0], p[1]);
    for (let i = 2; i < p.length - 2; i += 2) c.quadraticCurveTo(p[i], p[i + 1], (p[i] + p[i + 2]) / 2, (p[i + 1] + p[i + 3]) / 2);
    c.lineTo(p[p.length - 2], p[p.length - 1]);
    c.stroke();
  }
  c.restore();
}

// ── Where a mark is ───────────────────────────────────────────────────────────

let ruler: Ctx | null = null;

/** The rectangle around a mark's geometry (a line's width not counted). */
export function bounds(m: Mark): Box {
  if (m.tool === "text") {
    const c = (ruler ??= document.createElement("canvas").getContext("2d")!);
    const px = fontPx(m.size);
    c.font = fontOf(px);
    const lines = (m.text ?? "").split("\n");
    return { x: m.pts[0], y: m.pts[1], w: Math.max(...lines.map((line) => c.measureText(line).width)), h: lines.length * px * LEADING };
  }
  let [x0, y0, x1, y1] = [Infinity, Infinity, -Infinity, -Infinity];
  for (let i = 0; i < m.pts.length; i += 2) {
    [x0, x1] = [Math.min(x0, m.pts[i]), Math.max(x1, m.pts[i])];
    [y0, y1] = [Math.min(y0, m.pts[i + 1]), Math.max(y1, m.pts[i + 1])];
  }
  return { x: x0, y: y0, w: x1 - x0, h: y1 - y0 };
}

/** Whether (x, y) is within `reach` of the path through `p`. */
function near(p: number[], x: number, y: number, reach: number): boolean {
  for (let i = 0; i < Math.max(2, p.length - 2); i += 2) {
    const [ax, ay, bx, by] = [p[i], p[i + 1], p[i + 2] ?? p[i], p[i + 3] ?? p[i + 1]];
    const [dx, dy] = [bx - ax, by - ay];
    const along = dx || dy ? Math.max(0, Math.min(1, ((x - ax) * dx + (y - ay) * dy) / (dx * dx + dy * dy))) : 0;
    if (Math.hypot(x - ax - along * dx, y - ay - along * dy) <= reach) return true;
  }
  return false;
}

/** A click at (x, y) means this mark: on its line, or inside a text or a blur. */
export function hit(m: Mark, x: number, y: number, reach: number): boolean {
  const p = m.pts;
  if (m.tool === "text" || m.tool === "blur" || m.tool === "spot" || m.tool === "magnify" || m.tool === "image") {
    const b = bounds(m);
    return x >= b.x - reach && y >= b.y - reach && x <= b.x + b.w + reach && y <= b.y + b.h + reach;
  }
  if (m.tool === "rect") return near([p[0], p[1], p[2], p[1], p[2], p[3], p[0], p[3], p[0], p[1]], x, y, reach + m.size / 2);
  if (m.tool === "ellipse") {
    const [rx, ry] = [Math.abs(p[2] - p[0]) / 2 || 1, Math.abs(p[3] - p[1]) / 2 || 1];
    const d = Math.hypot((x - (p[0] + p[2]) / 2) / rx, (y - (p[1] + p[3]) / 2) / ry);
    return Math.abs(d - 1) * Math.min(rx, ry) <= reach + m.size / 2;
  }
  if (m.tool === "curve") {
    const [c1x, c1y] = bend(p);
    const path = Array.from({ length: 13 }, (_, i) => {
      const u = i / 12;
      return [(1 - u) ** 2 * p[0] + 2 * u * (1 - u) * c1x + u * u * p[2], (1 - u) ** 2 * p[1] + 2 * u * (1 - u) * c1y + u * u * p[3]];
    }).flat();
    return near(path, x, y, reach + m.size / 2);
  }
  return near(p, x, y, reach + (m.tool === "highlight" ? markerPx(m.size) : m.size) / 2);
}

// ── The board: marks, the tool in hand, undo ──────────────────────────────────

interface Snap {
  marks: Mark[];
  crop: Box | null;
}

const freehand = (tool: Tool | "image") => tool === "pen" || tool === "highlight";

/**
 * The marks of one document and the gestures that make them. The page turns
 * the pointer into pixels of the document and calls `down`, `move` and `up`;
 * text and the crop need the page itself and are the page's to start.
 */
export class Board {
  marks: Mark[] = [];
  /** The part of the picture that is kept; none is all of it. */
  crop: Box | null = null;
  tool: Tool = "arrow";
  color = "";
  size = 4;
  /** New rectangles and ellipses are filled. */
  fill = false;
  /** The mark being drawn. */
  draft: Mark | null = null;
  /** The mark the select tool holds, by its place in `marks`. */
  selected = -1;
  private past: Snap[] = [];
  private future: Snap[] = [];
  private drag: { x: number; y: number; from: Mark[] } | null = null;

  constructor(private readonly changed: () => void) {}

  get canUndo() {
    return this.past.length > 0;
  }
  get canRedo() {
    return this.future.length > 0;
  }

  /** A new state of the document, one step of undo. */
  set(marks: Mark[], crop: Box | null = this.crop) {
    this.past.push({ marks: this.marks, crop: this.crop });
    this.future = [];
    [this.marks, this.crop] = [marks, crop];
    this.changed();
  }

  private step(from: Snap[], to: Snap[]) {
    const snap = from.pop();
    if (!snap) return;
    to.push({ marks: this.marks, crop: this.crop });
    [this.marks, this.crop, this.selected] = [snap.marks, snap.crop, -1];
    this.changed();
  }
  undo() {
    this.step(this.past, this.future);
  }
  redo() {
    this.step(this.future, this.past);
  }

  /** The selected mark goes. */
  remove() {
    const gone = this.selected;
    if (gone < 0) return;
    this.selected = -1;
    this.set(this.marks.filter((_, i) => i !== gone));
  }

  /** A new colour or size for the selected mark. */
  restyle(patch: Partial<Pick<Mark, "color" | "size" | "fill">>) {
    if (this.marks[this.selected]) this.set(this.marks.map((m, i) => (i === this.selected ? { ...m, ...patch } : m)));
  }

  /** The topmost mark at a pixel, or -1. */
  private at(x: number, y: number, reach: number) {
    for (let i = this.marks.length - 1; i >= 0; i--) if (hit(this.marks[i], x, y, reach)) return i;
    return -1;
  }

  private erase(x: number, y: number, reach: number) {
    const gone = this.at(x, y, reach);
    if (gone >= 0) this.marks = this.marks.filter((_, i) => i !== gone);
  }

  private place = (v: number) => (this.tool === "blur" || this.tool === "spot" || this.tool === "magnify" ? Math.round(v) : snap(v, this.size));

  /** The pointer went down at a pixel of the document; `reach` is how near a mark counts as on it. */
  down(x: number, y: number, reach: number) {
    const { tool, color, size } = this;
    this.drag = { x, y, from: this.marks };
    if (tool === "select") this.selected = this.at(x, y, reach);
    else if (tool === "eraser") this.erase(x, y, reach);
    else if (tool === "pen" || tool === "highlight") this.draft = { tool, color, size, pts: [x, y] };
    else if (tool !== "text" && tool !== "crop") this.draft = { tool, color, size, fill: this.fill && (tool === "rect" || tool === "ellipse"), pts: [this.place(x), this.place(y), this.place(x), this.place(y)] };
    this.changed();
  }

  move(x: number, y: number, reach: number) {
    const drag = this.drag;
    if (!drag) return;
    const draft = this.draft;
    if (this.tool === "select") {
      const [dx, dy] = [Math.round(x - drag.x), Math.round(y - drag.y)];
      const held = this.selected;
      // Always from where it was: a mark dragged back is not changed at all.
      this.marks = dx || dy ? drag.from.map((m, i) => (i === held ? { ...m, pts: m.pts.map((v, j) => v + (j % 2 ? dy : dx)) } : m)) : drag.from;
    } else if (this.tool === "eraser") this.erase(x, y, reach);
    else if (draft && freehand(draft.tool)) draft.pts.push(x, y);
    // A new mark each time, not the same one moved: what is kept with a mark (a mosaic) is of its shape.
    else if (draft) this.draft = { ...draft, pts: [draft.pts[0], draft.pts[1], this.place(x), this.place(y)] };
    this.changed();
  }

  up() {
    const drag = this.drag;
    if (!drag) return;
    const draft = this.draft;
    [this.drag, this.draft] = [null, null];
    let next = this.marks;
    if (draft) {
      const b = bounds(draft);
      // A click without a drag draws nothing, except a dot of the pen.
      if (freehand(draft.tool) || b.w + b.h >= 2) next = [...next, draft];
    }
    // What the gesture did becomes one step of undo.
    this.marks = drag.from;
    if (next !== drag.from) this.set(next);
    else this.changed();
  }

  /** Every mark, and the one being drawn. */
  paint(c: Ctx, base?: Base | null) {
    const all = this.draft ? [...this.marks, this.draft] : this.marks;
    const spots = all.filter((m) => m.tool === "spot");
    if (spots.length && base) {
      // Everything is dimmed except the spotlights. debt: two overlapping spotlights dim where they overlap.
      c.save();
      c.fillStyle = "rgba(0,0,0,0.6)";
      c.beginPath();
      c.rect(0, 0, base.width, base.height);
      for (const s of spots) {
        const b = bounds(s);
        c.rect(b.x, b.y, b.w, b.h);
      }
      c.fill("evenodd");
      c.restore();
    }
    for (const m of all) drawMark(c, m, base);
  }

  /** A frame around the selected mark, one pixel of the display wide at this `scale`. */
  outline(c: Ctx, scale: number) {
    const m = this.marks[this.selected];
    if (!m) return;
    const b = bounds(m);
    const pad = (m.tool === "text" || m.tool === "blur" || m.tool === "spot" || m.tool === "magnify" || m.tool === "image" ? 0 : (m.tool === "highlight" ? markerPx(m.size) : m.size) / 2) + 4 / scale;
    c.save();
    c.strokeStyle = token("--accent");
    c.lineWidth = 1 / scale;
    c.setLineDash([4 / scale, 3 / scale]);
    c.strokeRect(b.x - pad, b.y - pad, b.w + pad * 2, b.h + pad * 2);
    c.restore();
  }
}

// ── The controls both pages build ─────────────────────────────────────────────

export const sep = () => h("i", { class: "f-editor-sep" });

/** One button per tool (`null` is a gap between groups); `show` marks the one in hand. */
export function toolRail(t: T, tools: (Tool | null)[], pick: (tool: Tool) => void) {
  const buttons = new Map<Tool, HTMLButtonElement>();
  const els = tools.map((tool) => {
    if (!tool) return sep();
    const [title, key] = TOOL[tool];
    const el = h("button", { class: "tab icon f-editor-tool", title: `${t(title)} (${key})`, "aria-label": t(title), "data-tool": tool, onclick: () => pick(tool) }, icon(ICONS[tool], 16));
    buttons.set(tool, el);
    return el;
  });
  return {
    els,
    show(tool: Tool | null) {
      for (const [k, el] of buttons) el.classList.toggle("on", k === tool);
    },
    /** The tool a key picks, if it is one of these. */
    byKey: (key: string) => [...buttons.keys()].find((tool) => TOOL[tool][1].toLowerCase() === key.toLowerCase()),
  };
}

/** The colours as round swatches; `show` marks one by its place in the palette. */
export function swatches(t: T, pick: (index: number) => void) {
  const dots = PALETTE.map(([name, title], i) =>
    h("button", { class: "f-editor-swatch", role: "radio", title: t(title), "aria-label": t(title), style: `--c:var(${name})`, onclick: () => pick(i) }),
  );
  return {
    el: h("div", { class: "f-editor-swatches", role: "radiogroup", "aria-label": t("Color") }, ...dots),
    show(index: number) {
      dots.forEach((dot, i) => {
        dot.classList.toggle("on", i === index);
        dot.setAttribute("aria-checked", String(i === index));
      });
    },
  };
}

/** The line widths, each drawn as thick as it is; `times` is for a document whose pixels are small (the screen). */
export function sizeButtons(t: T, pick: (size: number) => void, times = 1) {
  const sizes = SIZES.map((size) => size * times);
  const els = sizes.map((size, i) =>
    h("button", { class: "tab icon f-editor-size", title: t("{n} px", { n: size }), "aria-label": t("{n} px", { n: size }), "data-size": String(size), onclick: () => pick(size) }, h("i", { style: `height:${SIZES[i]}px` })),
  );
  return {
    els,
    show(size: number) {
      els.forEach((el, i) => el.classList.toggle("on", sizes[i] === size));
    },
  };
}

/**
 * A text field on the canvas for the text tool, at a point of `host` in CSS
 * pixels. Enter keeps the text, Esc drops it, a click elsewhere keeps it.
 */
export function typeText(host: HTMLElement, at: { left: number; top: number; px: number; color: string }, done: (text: string) => void) {
  const area = h("textarea", {
    class: "f-editor-text",
    spellcheck: "false",
    rows: "1",
    style: `left:${at.left}px;top:${at.top}px;font-size:${at.px}px;color:${at.color}`,
  });
  let over = false;
  const end = (keep: boolean) => {
    if (over) return;
    over = true;
    const text = keep ? area.value.replace(/\s+$/, "") : "";
    area.remove();
    done(text);
  };
  area.addEventListener("keydown", (e) => {
    // The page's own keys (tools, Delete, Esc) are not for a text being typed.
    e.stopPropagation();
    if (e.key === "Escape") end(false);
    else if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      end(true);
    }
  });
  area.addEventListener("blur", () => end(true));
  host.append(area);
  // After the click that asked for it: that click would take the focus back.
  requestAnimationFrame(() => area.focus());
}
