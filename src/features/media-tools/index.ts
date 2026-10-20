// Media tools — the page side of the feature (docs/ARCHITECTURE.md §2.3).
// Nothing runs at import: declare surfaces here, begin work in `start`.
//
// One section, Media tools: four tools (compress a video, make a GIF, convert pictures, read the text in
// a picture) on files the user dropped on it or chose. The work and every pixel of it live in Rust; a job
// answers with `progress`, then `done` or `failed`. A result is handed to the shelf (its `added` call),
// so it can be dragged on from there. The full options are in Settings; the section has the common ones.

import "./style.css";
import { Bridge, dropMessage } from "../../core/bridge";
import { defineFeature, type FeatureCtx, type SectionHost } from "../../core/feature";
import { tn } from "../../i18n/i18n";
import { btn, card, meter, row, setMeter, toggle } from "../../ui/components";
import { icon } from "../../ui/icons";
import { clear, h } from "../../views/dom";

type Tool = "compress" | "gif" | "audio" | "images" | "text";
type Settings = Record<string, unknown>;

interface Info {
  kind: "video" | "image";
  w: number;
  h: number;
  secs?: number;
  bytes: number;
}

interface Done {
  kind: Tool;
  path?: string;
  paths?: string[];
  failed?: string[];
  from?: number;
  to?: number;
}

const VIDEO = /\.(mp4|mov|m4v|wmv|avi|mkv|webm|mpe?g|3gp)$/i;
const AUDIO = /\.(mp3|m4a|wav|wma|aac|flac)$/i;
const name = (path: string) => path.split(/[\\/]/).pop() ?? path;

const bytes = (n: number) => (n >= 1e6 ? `${(n / 1e6).toFixed(1)} MB` : `${Math.max(1, Math.round(n / 1e3))} KB`);

interface WebView2 {
  postMessageWithAdditionalObjects(message: unknown, objects: ArrayLike<unknown>): void;
}

/** A job finished: its results join the shelf. Always, even with the section closed. */
function toShelf(paths: string[]) {
  if (paths.length) void Bridge.featureCall("shelf", "added", { paths });
}

/** A select that saves to the settings. */
function pick(cx: FeatureCtx, s: Settings, key: string, options: [string | number, string][], after?: () => void) {
  const sel = h("select", {}, ...options.map(([value, label]) => h("option", { value: String(value), text: label })));
  sel.value = String(s[key]);
  sel.addEventListener("change", () => {
    const old = s[key];
    s[key] = typeof old === "number" ? Number(sel.value) : sel.value;
    void cx.settings.patch({ [key]: s[key] });
    after?.();
  });
  return h("label", { class: "field f-media-tools-select" }, sel);
}

function build(cx: FeatureCtx): SectionHost {
  const t = cx.t;
  let s: Settings = {};
  let tool: Tool = "compress";
  let files: string[] = [];
  let info: Info | null = null;
  let busy = false;
  let shown = false;
  let ocr = true;
  let text = "";
  let note = "";

  const el = h("div", { class: "f-media-tools-page" });
  const tools: [Tool, string, string][] = [
    ["compress", t("Compress"), "record"],
    ["gif", t("GIF"), "image"],
    ["audio", t("MP3"), "music"],
    ["images", t("Pictures"), "shot"],
    ["text", t("Text"), "text"],
  ];

  const trim = { start: "", end: "" };

  function options(): HTMLElement {
    const field = (label: string, input: HTMLElement) => h("span", { class: "f-media-tools-opt" }, h("span", { text: label }), input);
    const num = (key: "start" | "end", ph: string) => {
      const input = h("input", { type: "number", min: "0", step: "0.1", placeholder: ph, value: trim[key] });
      input.addEventListener("input", () => (trim[key] = input.value));
      return h("label", { class: "field f-media-tools-num" }, input);
    };
    const trimmed = [field(t("From (s)"), num("start", "0")), field(t("To (s)"), num("end", info?.secs ? info.secs.toFixed(1) : ""))];
    const redo = () => paint();
    if (tool === "compress") {
      const bySize = s.videoSizing === "size";
      return h(
        "div",
        { class: "f-media-tools-opts" },
        field(t("Sizing"), pick(cx, s, "videoSizing", [["resolution", t("Resolution")], ["size", t("File size")]], redo)),
        bySize
          ? field(t("Target"), pick(cx, s, "videoTargetMb", [5, 10, 20, 50, 100, 250].map((n) => [n, `${n} MB`]) as [number, string][]))
          : field(t("Longest side"), pick(cx, s, "videoMaxDim", [480, 720, 1280, 1920, 2560, 3840].map((n) => [n, `${n} px`]) as [number, string][])),
        ...(bySize ? [] : [field(t("Compression"), pick(cx, s, "videoCompression", [["low", t("Low")], ["medium", t("Medium")], ["high", t("High")]]))]),
        ...trimmed,
      );
    }
    if (tool === "gif") {
      const bySize = s.gifSizing === "size";
      return h(
        "div",
        { class: "f-media-tools-opts" },
        field(t("Sizing"), pick(cx, s, "gifSizing", [["resolution", t("Resolution")], ["size", t("File size")]], redo)),
        bySize
          ? field(t("Target"), pick(cx, s, "gifTargetMb", [2, 5, 10, 20, 50].map((n) => [n, `${n} MB`]) as [number, string][]))
          : field(t("Frames per second"), pick(cx, s, "gifFps", [5, 8, 12, 15, 20, 30].map((n) => [n, String(n)]) as [number, string][])),
        ...(bySize ? [] : [field(t("Width"), pick(cx, s, "gifWidth", [320, 480, 720, 1080, 1600].map((n) => [n, `${n} px`]) as [number, string][]))]),
        ...trimmed,
      );
    }
    if (tool === "audio") return h("div", { class: "f-media-tools-opts" }, ...trimmed);
    if (tool === "images") {
      const wm = h("input", { type: "text", placeholder: t("Watermark text"), value: String(s.wmText ?? ""), spellcheck: "false" });
      wm.addEventListener("change", () => {
        s.wmText = wm.value;
        void cx.settings.patch({ wmText: wm.value });
      });
      return h(
        "div",
        { class: "f-media-tools-opts" },
        field(t("Format"), pick(cx, s, "imgFormat", [["jpeg", "JPEG"], ["png", "PNG"], ["pdf", "PDF"]])),
        field(t("Longest side"), pick(cx, s, "imgMax", [800, 1280, 1600, 2048, 3840].map((n) => [n, `${n} px`]) as [number, string][], () => {
          s.imgResize = "max";
          void cx.settings.patch({ imgResize: "max" });
        })),
        field(t("Watermark"), h("label", { class: "field f-media-tools-wide" }, wm)),
      );
    }
    return h("div", { class: "f-media-tools-opts" });
  }

  function summary(): string {
    if (!files.length) return "";
    if (files.length > 1) return tn("{count} file", "{count} files", files.length);
    if (!info) return name(files[0]);
    const dims = `${info.w}×${info.h}`;
    return [name(files[0]), dims, info.secs ? `${info.secs.toFixed(1)} s` : "", bytes(info.bytes)].filter(Boolean).join(" · ");
  }

  const bar = meter(0);
  const status = h("span", { class: "isl-meta f-media-tools-status" });
  let go: HTMLButtonElement;

  function paint() {
    clear(el);
    const row1 = h(
      "div",
      { class: "isl-row" },
      ...tools.map(([id, label, ic]) =>
        h("button", { class: `btn sm ${id === tool ? "primary" : "secondary"}`, disabled: busy, onclick: () => {
          tool = id;
          text = "";
          note = "";
          paint();
        } }, icon(ic, 14), h("span", { text: label })),
      ),
      h("span", { class: "isl-gap" }),
      h("button", { class: "btn sm ghost", disabled: busy, onclick: () => void choose() }, icon("plus", 14), h("span", { text: t("Add files") })),
    );
    const names = h("div", { class: "f-media-tools-files" }, ...(files.length ? [icon(tool === "images" || tool === "text" ? "image" : "record", 16), h("span", { text: summary() })] : [icon("drop", 16), h("span", { text: t("Drop files here, or choose them") })]));
    const out = tool === "text" && text !== "" ? h("div", { class: "f-media-tools-text", text }) : null;
    status.textContent = note;
    go = btn(busy ? t("Cancel") : tool === "compress" ? t("Compress") : tool === "gif" ? t("Make GIF") : tool === "audio" ? t("Save as MP3") : tool === "images" ? t("Convert") : t("Read text"), busy ? "secondary" : "primary", () => (busy ? void cx.call("cancel") : void run()));
    go.disabled = !busy && (!files.length || (tool === "text" && !ocr));
    const copy = out ? btn(t("Copy"), "secondary", () => void navigator.clipboard?.writeText(text).then(() => ((note = t("Copied")), (status.textContent = note)))) : null;
    el.append(row1, names, out ?? options(), h("div", { class: "isl-row" }, busy ? bar : null, status, h("span", { class: "isl-gap" }), copy, go));
  }

  async function setFiles(paths: string[]) {
    files = paths;
    info = null;
    note = "";
    text = "";
    // The tool follows what was given, unless it already fits: video for the first two, pictures otherwise.
    const video = files.every((f) => VIDEO.test(f));
    const sound = files.every((f) => VIDEO.test(f) || AUDIO.test(f));
    if (video && (tool === "images" || tool === "text")) tool = "compress";
    if (sound && !video) tool = "audio";
    if (!sound && (tool === "compress" || tool === "gif" || tool === "audio")) tool = "images";
    if (!video && (tool === "compress" || tool === "gif")) tool = "images";
    paint();
    if (files.length === 1) {
      info = await cx.call<Info>("info", { path: files[0] });
      if (shown && files[0]) paint();
    }
  }

  async function choose() {
    const picked = await cx.call<string[]>("pick");
    if (picked?.length) void setFiles(picked);
  }

  async function run() {
    if (!files.length) return;
    const first = files[0]!;
    note = "";
    if (tool === "text") {
      busy = true;
      note = t("Reading…");
      paint();
      const r = await cx.call<{ text: string }>("text", { path: first });
      busy = false;
      text = r?.text ?? "";
      note = r ? (text ? "" : t("No text found.")) : t("The text could not be read.");
      paint();
      return;
    }
    const args = tool === "images" ? { paths: files } : { path: first, start: Number(trim.start) || 0, end: Number(trim.end) || 0 };
    setMeter(bar, 0);
    const started = await cx.call(tool, args);
    if (started === null) {
      note = t("The job could not start.");
    } else {
      busy = true;
    }
    paint();
  }

  const onDrop = (e: DragEvent) => {
    if (!shown) return;
    e.stopPropagation();
    const data = e.dataTransfer;
    if (!data || !data.types.includes("Files")) return;
    e.preventDefault();
    if (e.type === "drop" && data.files.length) {
      const webview = (window as unknown as { chrome?: { webview?: WebView2 } }).chrome?.webview;
      webview?.postMessageWithAdditionalObjects(dropMessage("media-tools"), data.files);
    } else data.dropEffect = "copy";
    el.classList.toggle("f-media-tools-over", e.type === "dragenter" || e.type === "dragover");
  };
  const drag = ["dragenter", "dragover", "dragleave", "drop"];
  let off: (() => void)[] = [];

  paint();
  return {
    el,
    show() {
      shown = true;
      for (const n of drag) window.addEventListener(n, onDrop as EventListener, true);
      off = [
        () => drag.forEach((n) => window.removeEventListener(n, onDrop as EventListener, true)),
        cx.on<string[]>("dropped", (paths) => void setFiles(paths)),
        cx.on<{ fraction: number }>("progress", (p) => setMeter(bar, p.fraction)),
        cx.on<Done>("done", (d) => {
          busy = false;
          const made = d.paths ?? (d.path ? [d.path] : []);
          note = d.failed?.length ? tn("{count} saved, {failed} failed", "{count} saved, {failed} failed", made.length, { failed: d.failed.length }) : d.from ? `${bytes(d.from)} → ${bytes(d.to ?? 0)}${(d.to ?? 0) > d.from ? ` · ${t("larger than the original")}` : ""}` : tn("{count} file saved", "{count} files saved", made.length);
          paint();
        }),
        cx.on<{ message: string; cancelled?: boolean }>("failed", (f) => {
          busy = false;
          note = f.cancelled ? t("Cancelled") : f.message;
          paint();
        }),
      ];
      void cx.settings.get().then((v) => {
        s = { ...v };
        paint();
      });
      void cx.call<{ ocr: boolean; busy: boolean }>("status").then((st) => {
        ocr = st?.ocr ?? true;
        busy = st?.busy ?? false;
        if (!ocr) note = t("Windows has no text recognition for your languages installed.");
        paint();
      });
    },
    hide() {
      shown = false;
      off.forEach((f) => f());
      off = [];
      el.classList.remove("f-media-tools-over");
    },
    sync() {},
  };
}

function settingsBody(cx: FeatureCtx): HTMLElement {
  const t = cx.t;
  const el = h("div", { class: "f-media-tools-settings" });
  void cx.settings.get().then((s) => {
    const sw = (key: string) => toggle(s[key] !== false, (on) => void cx.settings.patch({ [key]: on }));
    const sel = (key: string, o: [string | number, string][]) => pick(cx, { ...s }, key, o);
    const nums = (key: string, label: string, list: number[], unit: string) => row(label, sel(key, list.map((n) => [n, `${n}${unit}`])));
    clear(el);
    el.append(
      card([
        row(t("Where it runs"), h("span", { class: "f-media-tools-hint", text: t("On this computer") }), t("Nothing is downloaded and nothing is sent anywhere. Videos and GIFs use the media components of Windows (on N editions of Windows they need the Media Feature Pack); text is read by the text recognition of Windows, in the languages you have installed.")),
      ]),
      card([
        row(t("Video: sizing"), sel("videoSizing", [["resolution", t("Resolution")], ["size", t("File size")]])),
        nums("videoMaxDim", t("Video: longest side"), [480, 720, 1280, 1920, 2560, 3840], " px"),
        row(t("Video: compression"), sel("videoCompression", [["low", t("Low")], ["medium", t("Medium")], ["high", t("High")]])),
        nums("videoFps", t("Video: frames per second"), [15, 24, 30, 60], ""),
        nums("videoTargetMb", t("Video: target size"), [5, 10, 20, 50, 100, 250, 512], " MB"),
        row(t("Video: keep the sound"), sw("videoKeepAudio")),
      ]),
      card([
        row(t("GIF: sizing"), sel("gifSizing", [["resolution", t("Resolution")], ["size", t("File size")]])),
        nums("gifFps", t("GIF: frames per second"), [5, 8, 12, 15, 20, 30], ""),
        nums("gifWidth", t("GIF: width"), [320, 480, 720, 1080, 1600], " px"),
        nums("gifTargetMb", t("GIF: target size"), [2, 5, 10, 20, 50], " MB"),
        row(t("GIF: repeat"), sw("gifLoops")),
      ]),
      card([
        row(t("Pictures: format"), sel("imgFormat", [["jpeg", "JPEG"], ["png", "PNG"], ["pdf", "PDF"]])),
        row(t("Pictures: quality"), sel("imgQuality", [0.5, 0.6, 0.72, 0.85, 0.95].map((n) => [n, `${Math.round(n * 100)} %`]))),
        row(t("Pictures: resize"), sel("imgResize", [["none", t("Keep the size")], ["max", t("Longest side")], ["width", t("Width")], ["height", t("Height")], ["exact", t("Exact size")]])),
        nums("imgMax", t("Pictures: longest side"), [800, 1280, 1600, 2048, 3840], " px"),
        nums("imgW", t("Pictures: width"), [320, 640, 1024, 1600, 1920, 3840], " px"),
        nums("imgH", t("Pictures: height"), [240, 480, 768, 1200, 1080, 2160], " px"),
        row(t("Pictures: exact size"), sel("imgFit", [["fit", t("Fit inside")], ["fill", t("Fill and crop")], ["stretch", t("Stretch")]])),
        row(t("Pictures: background"), sel("imgBg", [["white", t("White")], ["black", t("Black")]]), t("Behind transparent parts, for JPEG and PDF.")),
        row(t("Pictures: save in a subfolder named Converted"), sw("imgSubfolder")),
        row(t("Pictures: name"), sel("imgPattern", [["", t("Same name")], ["{name} {width}x{height}", t("Name and size")], ["{name} {date}", t("Name and date")], ["{index:03}", t("Number")]]), t("Metadata (place, camera) is not copied to the new file.")),
      ]),
      card([
        row(t("Watermark: position"), sel("wmPos", [[0, t("Top left")], [1, t("Top right")], [2, t("Bottom left")], [3, t("Bottom right")], [4, t("Center")]])),
        row(t("Watermark: strength"), sel("wmOpacity", [0.2, 0.45, 0.7, 1].map((n) => [n, `${Math.round(n * 100)} %`]))),
        row(t("Watermark: size"), sel("wmScale", [0.1, 0.18, 0.3, 0.5].map((n) => [n, `${Math.round(n * 100)} %`]))),
      ]),
      h("p", { class: "f-media-tools-note", text: t("Results are saved beside the original under a new name, never over it, and added to the shelf.") }),
    );
  });
  return el;
}

export default defineFeature({
  id: "media-tools",
  sections: [{ id: "tools", title: "Media tools", icon: "image", letter: "j", height: 232, build }],
  settings: settingsBody,
  start: (cx) => {
    // A finished job joins the shelf even when its section is not on screen; the island says so then.
    const off = cx.on<Done>("done", (d) => {
      const made = d.paths ?? (d.path ? [d.path] : []);
      toShelf(made);
      if (made.length) cx.island.notice({ kind: "download", title: cx.t("Saved"), detail: name(made[0]!), icon: "check", tone: "ok", section: "tools" });
    });
    return off;
  },
});
