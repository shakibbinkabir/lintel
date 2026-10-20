// The wheel, drawn. No Tauri and no feature context in here: Rust decides what is
// under the pointer and tells this page (`show`, `hover`, `close`), so the same
// code draws the real window and the made-up one of the preview.
//
// The geometry (the constants, a slice's wedge and angle, the shorter way round) is in ring.ts.

import { ICON, icon } from "../../ui/icons";
import { h } from "../../views/dom";
import { at, HUB, INNER, MID, RADIUS, SIZE, sliceAngle, travel, wedge } from "./ring";

const P = "f-radial-menu-";
const NS = "http://www.w3.org/2000/svg";
const ICON_PX = 22;
/** A highlight that left less than this ago is still fading out: it travels on instead of appearing afresh. */
const LINGER_MS = 150;

export interface Item {
  kind: string;
  name: string;
  target: string;
  icon: string;
  items: Item[];
}

export interface Show {
  title: string;
  back: boolean;
  items: Item[];
  sticky: boolean;
  np?: NowPlaying | null;
}

export interface NowPlaying {
  title: string;
  artist: string;
  playing: boolean;
  rev: number;
  player?: string;
}

/** What the wheel needs from the outside. */
export interface Host {
  t(key: string, vars?: Record<string, string | number>): string;
  /** The icon of an app or a file as a data URL, or null. */
  icon(kind: string, target: string): Promise<string | null>;
  /** The cover of the track `rev` as a data URL, or null. */
  art(rev: number): Promise<string | null>;
}

export interface Wheel {
  show(p: Show): void;
  hover(index: number | null): void;
  close(): void;
  nowPlaying(np: NowPlaying | null): void;
}

const el = <K extends keyof SVGElementTagNameMap>(tag: K, attrs: Record<string, string | number> = {}) => {
  const e = document.createElementNS(NS, tag);
  for (const [k, v] of Object.entries(attrs)) e.setAttribute(k, String(v));
  return e;
};

const DEFAULT_ICON: Record<string, string> = {
  app: "grid",
  file: "file",
  link: "link",
  media: "play",
  nowplaying: "music",
  shortcut: "keyboard",
  command: "terminal",
  submenu: "chevr",
};
const MEDIA_ICON: Record<string, string> = { playPause: "play", next: "next", prev: "prev" };

export function mountWheel(root: HTMLElement, host: Host): Wheel {
  root.className = `${P}wheel`;
  root.dataset.state = "idle";
  const t = host.t;

  // Three layers of the same size: the disc and its slices, the highlight, the marks on top of it.
  const layer = (cls: string) => el("svg", { class: `${P}${cls}`, viewBox: `0 0 ${SIZE} ${SIZE}`, width: "100%", height: "100%", "aria-hidden": "true" });
  const slices = el("g", { class: `${P}slices` });
  const disc = el("circle", { class: `${P}disc`, cx: MID, cy: MID, r: RADIUS + 2 });
  const hub = el("circle", { class: `${P}hub`, cx: MID, cy: MID, r: HUB });
  const base = layer("base");
  base.append(disc, slices, hub);
  // The highlight is one shape, the wedge of slice 0: its layer turns to the slice it is on and fades.
  const glowShape = el("path");
  const glow = layer("glow");
  glow.append(glowShape);
  const marks = layer("marks");

  // The hub's words on two layers: they crossfade from one to the other.
  const hubs = [0, 1].map(() => {
    const name = h("b", { class: `${P}hub-name` });
    const sub = h("span", { class: `${P}hub-sub` });
    return { el: h("div", { class: `${P}hub-text` }, name, sub), name, sub };
  });
  let live = 0;
  let hubWords = "";
  root.append(h("div", { class: `${P}stage` }, base, glow, marks, hubs[0].el, hubs[1].el));

  let shown: Show = { title: "", back: false, items: [], sticky: false };
  let hovered: number | null = null;
  /** The highlight: its angle in degrees (never wrapped), the slice it is on or was last on, when it was left. */
  let turn = 0;
  let spot: number | null = null;
  let leftAt = 0;
  let np: NowPlaying | null = null;
  const nodes: { g: SVGGElement; mark: SVGElement }[] = [];
  /** Icons and covers already fetched, by what asked for them. */
  const pictures = new Map<string, string | null>();

  const label = (it: Item): string => {
    if (it.name) return it.name;
    switch (it.kind) {
      case "media":
        return t(it.target === "next" ? "Next track" : it.target === "prev" ? "Previous track" : "Play or pause");
      case "nowplaying":
        return np?.title || t("Now playing");
      case "file":
        return it.target.split(/[\\/]/).filter(Boolean).pop() ?? it.target;
      case "link":
        return it.target.replace(/^[a-z][a-z0-9+.-]*:\/\//i, "").split("/")[0];
      case "shortcut":
        return it.target;
      case "command":
        return it.target.replace(/^"/, "").split(/[\\/ ]/).filter(Boolean).pop() ?? it.target;
      case "submenu":
        return t("Group");
      default:
        return t("App");
    }
  };

  const detail = (it: Item): string => {
    switch (it.kind) {
      case "nowplaying":
        return np ? np.artist || np.player || "" : t("Nothing playing");
      case "submenu":
        return t("Rest to open");
      case "media":
        return it.target === "playPause" && np ? (np.playing ? t("Pause") : t("Play")) : "";
      default:
        return "";
    }
  };

  const iconName = (it: Item): string => {
    if (it.icon && it.icon in ICON) return it.icon;
    if (it.kind === "media") {
      return it.target === "playPause" && np?.playing ? "pause" : (MEDIA_ICON[it.target] ?? "play");
    }
    if (it.kind === "nowplaying") return np?.playing ? "pause" : "music";
    return DEFAULT_ICON[it.kind] ?? "grid";
  };

  /** The mark of a slice: a picture when there is one, else the icon. Drawn at the middle of the ring. */
  const drawMark = (it: Item, i: number, g: SVGGElement, old?: SVGElement): SVGElement => {
    const [cx, cy] = at((i * 2 * Math.PI) / shown.items.length, (INNER + RADIUS) / 2);
    let mark: SVGElement;
    const key = it.kind === "nowplaying" ? (np?.rev ? `art:${np.rev}` : "") : !it.icon && (it.kind === "app" || it.kind === "file") ? `${it.kind}:${it.target}` : "";
    const url = key ? pictures.get(key) : null;
    if (url) {
      const round = it.kind === "nowplaying";
      const size = round ? 38 : 30;
      mark = el("image", { href: url, x: cx - size / 2, y: cy - size / 2, width: size, height: size, class: `${P}pic${round ? " round" : ""}` });
    } else {
      const ico = icon(iconName(it), ICON_PX);
      ico.setAttribute("x", String(cx - ICON_PX / 2));
      ico.setAttribute("y", String(cy - ICON_PX / 2));
      ico.setAttribute("class", `${P}ico`);
      mark = ico;
      if (key && !pictures.has(key)) {
        pictures.set(key, null);
        const ask = it.kind === "nowplaying" ? host.art(np?.rev ?? 0) : host.icon(it.kind, it.target);
        void ask.then((u) => {
          if (!u) return;
          pictures.set(key, u);
          redrawMarks();
        });
      }
    }
    if (old) old.replaceWith(mark);
    else g.append(mark);
    return mark;
  };

  const redrawMarks = () => {
    nodes.forEach((n, i) => {
      const it = shown.items[i];
      if (it) n.mark = drawMark(it, i, n.g, n.mark);
    });
    paintHub();
  };

  /** `snap`: a new wheel or level, written in place. Otherwise new words fade in over the old ones. */
  const paintHub = (snap = false) => {
    const it = hovered == null ? null : shown.items[hovered];
    const back = !it && shown.back;
    const words = it ? [label(it), detail(it)] : [shown.title || (shown.back ? t("Group") : ""), back ? t("Click to go back") : ""];
    const key = [...words, back].join("\n");
    if (key === hubWords && !snap) return;
    hubWords = key;
    if (!snap) {
      hubs[live].el.classList.remove("on");
      live = 1 - live;
    }
    const to = hubs[live];
    to.name.textContent = words[0];
    to.sub.textContent = words[1];
    to.el.classList.toggle(`${P}hub-back`, back);
    to.el.classList.add("on");
  };

  const wheel: Wheel = {
    show(p) {
      shown = p;
      if (p.np !== undefined) np = p.np;
      hovered = null;
      spot = null;
      glow.classList.remove("on");
      glowShape.setAttribute("d", p.items.length ? wedge(0, p.items.length) : "");
      slices.replaceChildren();
      marks.replaceChildren();
      nodes.length = 0;
      p.items.forEach((it, i) => {
        const slice = el("g", { class: `${P}slice`, style: `--i:${i}`, "data-kind": it.kind });
        slice.append(el("path", { class: `${P}wedge`, d: wedge(i, p.items.length) }));
        slices.append(slice);
        // The mark lies above the highlight and flies in with its slice: the same class, the same --i.
        const fly = el("g", { class: `${P}slice`, style: `--i:${i}` });
        const g = el("g", { class: `${P}mark` });
        fly.append(g);
        marks.append(fly);
        nodes.push({ g, mark: drawMark(it, i, g) });
      });
      paintHub(true);
      // Restart the entrance: the same page serves every opening.
      root.dataset.state = "idle";
      void root.offsetWidth;
      root.dataset.state = "in";
    },
    // The pointer, the arrow keys and the scroll wheel all arrive here (Rust's `hover` event).
    hover(index) {
      const n = shown.items.length;
      if (index != null && index < n) {
        // From another slice, or one left a moment ago and still fading, the shape travels the shorter
        // way round. From rest (the hub, outside, a new wheel) it is put on the slice and fades in there.
        const from = hovered ?? (performance.now() - leftAt < LINGER_MS ? spot : null);
        glow.classList.toggle("travel", from != null);
        turn = from == null ? sliceAngle(index, n) : travel(turn, from, index, n);
        glow.style.transform = `rotate(${turn}deg)`;
        // Placed for good before anything turns it: a second slice chosen within the same frame
        // would otherwise sweep it in from wherever it last was. Once per entry, never per frame.
        if (from == null) void glow.getBoundingClientRect();
        spot = index;
      } else if (hovered != null) {
        leftAt = performance.now();
      }
      glow.classList.toggle("on", index != null);
      hovered = index;
      nodes.forEach((node, i) => node.g.classList.toggle("on", i === index));
      paintHub();
    },
    close() {
      root.dataset.state = "out";
    },
    nowPlaying(next) {
      np = next;
      // A new cover is asked for again; the old ones stay (a track may come back).
      redrawMarks();
    },
  };
  return wheel;
}
