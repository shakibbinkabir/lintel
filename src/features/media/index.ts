// Media — the page side of the feature (docs/ARCHITECTURE.md §2.3): what plays
// on this computer, on the strip and as a section of the open island.
// Nothing runs at import: surfaces are declared here, work begins in `start`.
//
// Rust (crates/media over crates/svc-media) sends `media:state` whenever
// something a listener can see has changed. Position is not sent as it moves:
// it is carried on here from the last report, and only while the section is on
// screen.

import "./style.css";
import { defineFeature, type FeatureCtx, type SectionHost, type Value } from "../../core/feature";
import { btn, card, iconBtn, menu, row, setMeter, slider, stripRow, toggle } from "../../ui/components";
import { ICON, icon } from "../../ui/icons";
import { h } from "../../views/dom";

interface Source {
  aumid: string;
  name: string;
  /** What the session plays: tells two tabs of one browser apart. */
  title: string;
  playing: boolean;
}

interface Track {
  source: { aumid: string; name: string };
  /** Not the preferred player: the one that stands in for it while it is quiet. */
  fallback: boolean;
  title: string;
  artist: string;
  album: string;
  /** Changes with the cover; 0 = none. The image itself comes from `call("art", { rev })`. */
  art: number;
  playing: boolean;
  /** Seconds at `at` (Unix ms); moves on at `rate` while playing. */
  position: number;
  /** Seconds; 0 = the player publishes no timeline. */
  duration: number;
  rate: number;
  at: number;
  canPlayPause: boolean;
  canNext: boolean;
  canPrev: boolean;
  canSeek: boolean;
  canShuffle: boolean;
  canRepeat: boolean;
  shuffle: boolean;
  repeat: "off" | "track" | "list";
}

interface Snapshot {
  track: Track | null;
  sources: Source[];
  /** The player the user prefers; null = whoever plays. */
  picked: string | null;
}

/** A song that was just paused stays on the strip this long. */
const LINGER_MS = 3000;
/** A new song is announced once the player has settled on it (and had time to send its cover). */
const SETTLE_MS = 500;
/** The last cover stays this long while the next song's is on its way. */
const ART_GRACE_MS = 1500;
/** The repeat icon with a 1 in it: repeat this track. */
const REPEAT_ONE = `${ICON.repeat}M11 10.4l1.3-.9v5`;
const NONE: Snapshot = { track: null, sources: [], picked: null };

let snap = NONE;
/** The cover on screen, as a data URL. */
let art = "";
let artRev = 0;
/** The cover's colour; "" when it has none worth showing. */
let tint = "";
let cfg = { trackNotice: true, artTint: true, artGlow: true, player: "", playerName: "", fallback: false };
let lingering = false;
let lingerTimer = 0;
let noticeTimer = 0;
let artTimer = 0;
/** The section, once built. */
let repaint: (() => void) | null = null;
/** The island, while the feature runs. */
let island: FeatureCtx["island"] | null = null;

// ── For the lyrics feature ────────────────────────────────────────────────────
//
// Lyrics has no surface of its own. The song's text shows in the Music section,
// right of the player, and the line being sung takes the title's place on the
// strip row while the song plays: one activity (this one) carries both, and a
// new line never slides the island out. Lyrics builds the column and keeps the
// time; this feature gives it the room, says when anyone is looking, and draws
// the row.

/** How long a line takes to give way to the next on the strip (--d-base). */
const LINE_MS = 180;
/** The line on the strip; "" gives the title back. */
let lyric = "";
/** The row as it was last drawn, what it said, and the line change under way on it. */
let stripEl: HTMLElement | null = null;
let stripText = { title: "", detail: "" };
let lineSwap: { title: string; detail: string; at: number } | null = null;
/** The column's place once the section is built, what lyrics put there, and whom to tell who is looking. */
let lyricSlot: HTMLElement | null = null;
let lyricBody: HTMLElement | null = null;
let lyricWatch: (() => void) | null = null;
let modeWatch: (() => void) | null = null;
let sectionUp = false;
/** The section measures its title again: the column takes room from it. */
let refit: (() => void) | null = null;

/** Where a lyric can be seen right now: the Music section, the strip while it shows the music row, or nowhere. */
export function lyricSeen(): "section" | "strip" | "" {
  if (sectionUp) return "section";
  return stripEl?.isConnected && island?.mode() === "strip" ? "strip" : "";
}

/** `fn` is called whenever `lyricSeen()` may have changed; `null` ends it. */
export function watchLyric(fn: (() => void) | null) {
  lyricWatch = fn;
  modeWatch?.();
  modeWatch = fn && island ? island.onMode(fn) : null;
}

/** The song's text in the section: an element of the lyrics feature, or nothing. */
export function mountLyric(el: HTMLElement | null) {
  if (el === lyricBody) return;
  lyricBody = el;
  lyricSlot?.replaceChildren(...(el ? [el] : []));
  refit?.();
}

/**
 * The line being sung, for the strip row. `fade`: it follows a line that was on
 * screen, which lifts out as this one comes in; without it the row is drawn as
 * it stands (nobody saw the line before).
 */
export function setLyric(text: string, fade = false) {
  if (text === lyric) return;
  lyric = text;
  lineSwap = fade && lyricSeen() === "strip" ? { ...stripText, at: performance.now() } : null;
  island?.redraw("playing");
}

/** A track with an end to it: only that one has a slider, and only in it can one seek. */
const timed = (tr: Track) => Number.isFinite(tr.duration) && tr.duration > 0;

/** Where the track is now: the reported position carried on from the time of the report. */
function positionOf(tr: Track): number {
  const moved = tr.playing ? tr.position + ((Date.now() - tr.at) / 1000) * tr.rate : tr.position;
  return Math.max(0, timed(tr) ? Math.min(moved, tr.duration) : moved);
}

/** 3:07, 1:02:45. */
function clock(seconds: number): string {
  const s = Math.max(0, Math.floor(seconds));
  const pad = (n: number) => String(n).padStart(2, "0");
  const hours = Math.floor(s / 3600);
  const minutes = Math.floor(s / 60) % 60;
  return hours > 0 ? `${hours}:${pad(minutes)}:${pad(s % 60)}` : `${minutes}:${pad(s % 60)}`;
}

// ── Cover and its colour ──────────────────────────────────────────────────────

function paintCover(el: HTMLElement) {
  el.style.backgroundImage = art ? `url("${art}")` : "";
  el.classList.toggle("none", !art);
}

/** Marks an element as showing what the strip shows under the same name. */
function shared<T extends HTMLElement>(el: T, name: string): T {
  el.dataset.shared = name;
  return el;
}

/** The cover, or a note where there is none. 22 px unless its parent sizes it. */
function coverEl(iconSize = 14): HTMLElement {
  const el = h("span", { class: "f-media-art" }, icon("music", iconSize));
  paintCover(el);
  return el;
}

/**
 * The cover's average colour, lifted until it reads on black; "" for a cover
 * that is nearly black or nearly grey. After `averageColor` in
 * NSImage+Extensions.swift, with the brightness floor the player's artist line
 * uses there.
 */
function tintOf(img: HTMLImageElement): string {
  const canvas = document.createElement("canvas");
  canvas.width = canvas.height = 16;
  const g = canvas.getContext("2d", { willReadFrequently: true });
  if (!g) return "";
  g.drawImage(img, 0, 0, 16, 16);
  const px = g.getImageData(0, 0, 16, 16).data;
  let [r, gr, b] = [0, 0, 0];
  for (let i = 0; i < px.length; i += 4) {
    r += px[i];
    gr += px[i + 1];
    b += px[i + 2];
  }
  const n = (px.length / 4) * 255;
  [r, gr, b] = [r / n, gr / n, b / n];
  const max = Math.max(r, gr, b);
  const saturation = max === 0 ? 0 : (max - Math.min(r, gr, b)) / max;
  if (max < 0.05 || saturation < 0.12) return "";
  const lift = Math.max(1, 0.6 / max);
  const to = (v: number) => Math.round(Math.min(1, v * lift) * 255);
  return `rgb(${to(r)} ${to(gr)} ${to(b)})`;
}

/**
 * Everything of the feature that is tinted reads this one property: the bars,
 * the slider, the artist's name. Switched off in Settings, they keep the accent.
 */
function applyTint() {
  const root = document.documentElement.style;
  if (tint && cfg.artTint) root.setProperty("--f-media-tint", tint);
  else root.removeProperty("--f-media-tint");
}

function setArt(url: string, colour: string) {
  art = url;
  tint = colour;
  applyTint();
  document.querySelectorAll<HTMLElement>(".f-media-art").forEach(paintCover);
}

/**
 * The cover numbered `rev`. Rust answers null once the track has moved on, and
 * an answer or a picture that arrives after it has is dropped here: a cover is
 * never shown on a song it does not belong to.
 */
function loadArt(cx: FeatureCtx, rev: number) {
  void cx.call<string>("art", { rev }).then((url) => {
    if (rev !== artRev) return;
    // None for the number that is still current: the report of the next one is
    // on its way. Should it not come, the old cover goes rather than stay.
    if (!url) return void (artTimer = window.setTimeout(() => rev === artRev && setArt("", ""), ART_GRACE_MS));
    const img = new Image();
    img.onload = () => rev === artRev && setArt(url, tintOf(img));
    img.onerror = () => rev === artRev && setArt("", "");
    img.src = url;
  });
}

// ── The strip ─────────────────────────────────────────────────────────────────

/** Cover, title, artist, and the bars: moving while it plays, still when paused. A line being sung is the row. */
function stripLine(): HTMLElement {
  const tr = snap.track;
  const line = tr?.playing ? lyric : "";
  const title = line || (tr ? tr.title || tr.source.name : "");
  const detail = (line ? "" : tr?.artist) || "";
  const row = stripRow({ mark: coverEl(), title, detail: detail || undefined, bars: tr?.playing ? true : "paused" });
  row.classList.add("f-media-row");
  row.classList.toggle("f-media-sung", line !== "");
  // A line change under way: the line before lifts out over the one coming in.
  // Drawn again meanwhile (a cover that arrived), it goes on from where it was.
  const age = lineSwap ? performance.now() - lineSwap.at : LINE_MS;
  if (lineSwap && age < LINE_MS && lineSwap.title !== title) {
    row.classList.add("f-media-swap");
    row.style.setProperty("--f-media-age", `${-Math.round(age)}ms`);
    row.append(
      h(
        "span",
        { class: "f-media-ghost", "aria-hidden": "true" },
        h("b", { text: lineSwap.title }),
        lineSwap.detail ? h("span", { class: "d", text: lineSwap.detail }) : null,
      ),
    );
  }
  stripEl = row;
  stripText = { title, detail };
  // Once the row is in place: the strip may just have come to the music row.
  if (lyricWatch) queueMicrotask(lyricWatch);
  return row;
}

/** The new song as a notice, so that it shows whatever else is on the strip. */
function announce(cx: FeatureCtx) {
  const tr = snap.track;
  if (!tr?.playing || !tr.title) return;
  cx.island.notice({
    kind: "track",
    title: tr.title,
    detail: tr.artist || tr.album || tr.source.name,
    mark: coverEl(),
    bars: true,
    section: "music",
  });
}

const isActive = () => snap.track != null && (snap.track.playing || lingering);

/** A report from Rust. `seed` is the first one: it sets the scene and announces nothing. */
function apply(cx: FeatureCtx, next: Snapshot, seed = false) {
  const before = snap.track;
  const wasActive = isActive();
  snap = next;
  const tr = next.track;

  const rev = tr?.art ?? 0;
  if (rev !== artRev) {
    artRev = rev;
    window.clearTimeout(artTimer);
    if (rev) loadArt(cx, rev);
    else if (tr) artTimer = window.setTimeout(() => setArt("", ""), ART_GRACE_MS);
    else setArt("", "");
  }

  const wasPlaying = before?.playing ?? false;
  const playing = tr?.playing ?? false;
  if (playing || !tr) {
    lingering = false;
    window.clearTimeout(lingerTimer);
  } else if (wasPlaying) {
    lingering = true;
    window.clearTimeout(lingerTimer);
    lingerTimer = window.setTimeout(() => {
      lingering = false;
      cx.island.activityChanged("playing");
    }, LINGER_MS);
  }

  const newTrack =
    tr != null &&
    tr.title !== "" &&
    (before?.title !== tr.title || before.artist !== tr.artist || before.source.aumid !== tr.source.aumid);
  window.clearTimeout(noticeTimer);
  if (!seed && playing && newTrack && cfg.trackNotice) {
    noticeTimer = window.setTimeout(() => announce(cx), SETTLE_MS);
  }

  // Starting and pausing slide the strip out; anything else is drawn where it stands.
  if (wasActive !== isActive() || (!seed && wasPlaying !== playing)) cx.island.activityChanged("playing");
  else cx.island.redraw("playing");
  repaint?.();
}

// ── The section ───────────────────────────────────────────────────────────────

function buildSection(cx: FeatureCtx): SectionHost {
  const t = cx.t;
  let shown = false;
  let ticking = 0;
  let releaseHold: (() => void) | null = null;
  /** A click on play or pause shows at once, before the player says so. */
  let asked: boolean | null = null;
  let askedTimer = 0;

  const send = (method: string, args?: object) => void cx.call(method, args);

  const cover = h(
    "button",
    { class: "f-media-cover", title: t("Open the player"), "aria-label": t("Open the player"), onclick: () => send("open") },
    h("span", { class: "f-media-art f-media-glow", "aria-hidden": "true" }),
    shared(coverEl(28), "mark"),
  );

  // The cover and the title are on the strip too: they travel from there when
  // the island opens on this page (docs/ARCHITECTURE.md §4).
  const title = shared(h("span"), "title");
  const titleBox = h("div", { class: "f-media-title" }, title);
  const artist = h("div", { class: "f-media-artist" });
  const source = h("button", {
    class: "btn sm ghost f-media-source",
    onclick: () => {
      // On to the next player; after the last one, back to following whoever plays.
      const list = snap.sources;
      const following = list[list.findIndex((s) => s.aumid === snap.track?.source.aumid) + 1];
      send("pick", { aumid: following ? following.aumid : cfg.player ? null : (list[0]?.aumid ?? null) });
    },
  });

  const elapsed = h("span", { class: "f-media-time" });
  const total = h("span", { class: "f-media-time" });
  /** Jumps there, inside the track, and shows it at once; the player's next report takes over. */
  const seekTo = (seconds: number) => {
    const tr = snap.track;
    // No end to the track: nowhere to jump to.
    if (!tr || !tr.canSeek || !timed(tr) || !Number.isFinite(seconds)) return;
    tr.position = Math.max(0, Math.min(seconds, tr.duration));
    tr.at = Date.now();
    send("seek", { position: tr.position });
    paintTime();
  };
  const seek = slider(0, {
    label: t("Position"),
    onInput: (fraction) => (elapsed.textContent = clock(fraction * (snap.track?.duration ?? 0))),
    onChange: (fraction) => seekTo(fraction * (snap.track?.duration ?? 0)),
    // The pointer may leave the island while it drags: the island stays.
    onDrag: (on) => {
      releaseHold?.();
      releaseHold = on ? cx.island.hold() : null;
      if (!on) paintTime();
    },
  });
  const seekRow = h("div", { class: "f-media-seek" }, elapsed, seek, total);

  const shuffle = iconBtn("shuffle", t("Shuffle"), () => send("shuffle", { on: !snap.track?.shuffle }));
  const prev = iconBtn("prev", t("Previous"), () => send("prev"));
  const back = iconBtn("replay", t("Back 15 seconds"), () => snap.track && seekTo(positionOf(snap.track) - 15));
  const play = h("button", {
    class: "btn secondary icon f-media-play",
    onclick: () => {
      if (!snap.track) return;
      asked = !(asked ?? snap.track.playing);
      window.clearTimeout(askedTimer);
      askedTimer = window.setTimeout(() => {
        asked = null;
        paint();
      }, 1500);
      // Said as what is wanted, not as "the other state": a second click
      // cannot start again what the first one paused.
      send(asked ? "play" : "pause");
      paint();
    },
  });
  const forward = iconBtn("refresh", t("Forward 15 seconds"), () => snap.track && seekTo(positionOf(snap.track) + 15));
  const next = iconBtn("next", t("Next"), () => send("next"));
  const repeat = iconBtn("repeat", t("Repeat"), () => {
    const order = ["off", "list", "track"] as const;
    send("repeat", { mode: order[(order.indexOf(snap.track?.repeat ?? "off") + 1) % 3] });
  });

  const player = h(
    "div",
    { class: "f-media-player" },
    cover,
    h(
      "div",
      { class: "f-media-main" },
      h("div", { class: "f-media-head" }, h("div", { class: "f-media-names" }, titleBox, artist), source),
      seekRow,
      h("div", { class: "f-media-controls" }, shuffle, prev, back, play, forward, next, repeat),
    ),
    // The song's text, when the lyrics feature has one to show (`mountLyric`).
    (lyricSlot = h("div", { class: "f-media-lyric" }, lyricBody)),
  );
  // Nothing to show: every widget of the section gives way to a line that says why.
  const emptyTitle = h("b");
  const emptyHint = h("span");
  const follow = btn(t("Follow whoever plays"), "secondary", () => send("pick", { aumid: null }));
  follow.classList.add("sm");
  const empty = h("div", { class: "f-media-empty" }, icon("music", 20), emptyTitle, emptyHint, follow);
  const el = h("div", { class: "f-media-page" }, player, empty);

  /** A title too long for its line glides to its end and back. */
  function marquee() {
    const over = title.offsetWidth - titleBox.clientWidth;
    titleBox.classList.toggle("scroll", over > 2);
    titleBox.style.setProperty("--shift", `${-over}px`);
    titleBox.style.setProperty("--dur", `${Math.max(4, over / 18)}s`);
  }
  refit = () => shown && marquee();

  /** An icon of a button that changes with the state: drawn again only when it does. */
  function setIcon(button: HTMLElement, name: string, size = 14) {
    if (button.dataset.icon === name) return;
    button.dataset.icon = name;
    button.replaceChildren(icon(name, size));
  }

  function paintTime() {
    const tr = snap.track;
    if (!tr || seek.classList.contains("dragging")) return;
    // No end time (a live stream, a player that publishes no timeline): no
    // slider and no total, and the time gone by only if the player counts it.
    const known = timed(tr);
    const at = positionOf(tr);
    seek.hidden = total.hidden = !known;
    seekRow.style.visibility = known || at > 0 ? "" : "hidden";
    elapsed.textContent = clock(at);
    if (!known) return;
    setMeter(seek, at / tr.duration);
    total.textContent = clock(tr.duration);
  }

  /** Why the section has nothing to show, in the words that are true. */
  function paintEmpty() {
    const running = snap.sources.length > 0;
    const away = cfg.player !== "" && !snap.sources.some((s) => s.aumid === cfg.player);
    follow.hidden = !(away && running);
    if (away) {
      emptyTitle.textContent = t("{player} is not running", { player: cfg.playerName || t("The preferred player") });
      emptyHint.textContent = running
        ? t("Another player is open. Follow whoever plays, or let it stand in from Settings.")
        : t("Start it and it shows up here.");
    } else {
      emptyTitle.textContent = t(running ? "Nothing is playing" : "No player is running");
      emptyHint.textContent = t("Start something in any player and it shows up here.");
    }
  }

  function paint() {
    const tr = snap.track;
    player.hidden = !tr;
    empty.hidden = !!tr;
    const moving = shown && !!tr?.playing;
    if (moving && !ticking) ticking = window.setInterval(paintTime, 250);
    else if (!moving && ticking) {
      window.clearInterval(ticking);
      ticking = 0;
    }
    if (!tr) return paintEmpty();

    const playing = asked ?? tr.playing;
    el.classList.toggle("playing", playing);
    el.classList.toggle("f-media-noglow", !cfg.artGlow);
    const name = tr.title || tr.source.name;
    if (title.textContent !== name) {
      title.textContent = name;
      marquee();
    }
    artist.textContent = tr.artist || tr.album;

    // The pin: this is the preferred player. Without it while another stands in for that one.
    const preferred = cfg.player !== "" && cfg.player === tr.source.aumid;
    source.replaceChildren(...(preferred ? [icon("pin", 12)] : []), tr.source.name);
    source.classList.toggle("f-media-on", preferred);
    source.title = tr.fallback
      ? t("Standing in for {player}", { player: cfg.playerName || t("The preferred player") })
      : t("Switch player");
    source.disabled = snap.sources.length < 2 && cfg.player === "";

    // What the player shown cannot do is greyed out, whoever it is. One that
    // cannot skip but can seek (a video, a podcast) gets 15-second jumps instead.
    const jumps = !tr.canPrev && !tr.canNext && tr.canSeek;
    prev.hidden = next.hidden = jumps;
    back.hidden = forward.hidden = !jumps;
    prev.disabled = !tr.canPrev;
    next.disabled = !tr.canNext;
    play.disabled = !tr.canPlayPause;
    play.title = t(playing ? "Pause" : "Play");
    setIcon(play, playing ? "pause" : "play", 16);
    shuffle.hidden = !tr.canShuffle;
    shuffle.classList.toggle("f-media-on", tr.shuffle);
    repeat.hidden = !tr.canRepeat;
    repeat.classList.toggle("f-media-on", tr.repeat !== "off");
    setIcon(repeat, tr.repeat === "track" ? REPEAT_ONE : "repeat");
    seek.setAttribute("aria-disabled", String(!tr.canSeek));
    paintTime();
  }

  repaint = () => {
    // The player has done what was asked (or its timer gives up on it).
    if (asked === snap.track?.playing) asked = null;
    paint();
  };
  return {
    el,
    show() {
      sectionUp = shown = true;
      paint();
      marquee();
      lyricWatch?.();
    },
    hide() {
      sectionUp = shown = false;
      paint();
      lyricWatch?.();
    },
    sync() {},
  };
}

// ── Settings ──────────────────────────────────────────────────────────────────

function settingsBody(cx: FeatureCtx): HTMLElement {
  const t = cx.t;
  const look = card([]);
  const players = card([]);
  const box = h(
    "div",
    {},
    look,
    h("div", { class: "section-title", text: t("Player") }),
    players,
  );

  void cx.settings.get().then((v) => {
    const sw = (key: string, off = false) => toggle(off ? v[key] === true : v[key] !== false, (on) => void cx.settings.patch({ [key]: on }));
    look.append(
      row(t("Announce new tracks"), sw("trackNotice"), t("The island slides out with the title when the track changes.")),
      row(t("Color from the cover"), sw("artTint"), t("The bars and the artist's name take the color of the album art.")),
      row(t("Glow behind the cover"), sw("artGlow"), t("A soft glow in the cover's colors while a track plays.")),
    );

    // The preferred player: the one running players offer, or whoever plays.
    const whoever = t("Whoever plays");
    const pick = btn(String(v.playerName || "") || (v.player ? t("The preferred player") : whoever), "secondary", () => {
      void cx.call<Snapshot>("state").then((s) => {
        const at = pick.getBoundingClientRect();
        const choose = (aumid: string | null, name: string) => () => {
          void cx.call("pick", { aumid });
          pick.firstElementChild!.textContent = name;
        };
        const open = (s?.sources ?? []).map((p) => ({ label: p.title ? `${p.title} · ${p.name}` : p.name, icon: p.playing ? "play" : undefined, onClick: choose(p.aumid, p.name) }));
        menu([{ label: whoever, onClick: choose(null, whoever) }, ...(open.length ? [null, ...open] : [])], { x: at.left, y: at.bottom + 4 });
      });
    });
    players.append(
      row(t("Preferred player"), pick, t("Play, pause, next and previous go to this player, and only it is shown. Choose among the players that are open.")),
      row(
        t("Let another player stand in"),
        sw("fallback", true),
        t("While the preferred player is paused or closed, the one that plays is shown and controlled instead. What it cannot do is greyed out."),
      ),
    );
  });
  return box;
}

export default defineFeature({
  id: "media",
  voice: () => import("./voice"),
  activities: [
    {
      id: "playing",
      order: 5,
      section: "music",
      isActive,
      render: stripLine,
      mark: () => coverEl(),
    },
  ],
  sections: [{ id: "music", title: "Music", icon: "music", letter: "m", height: 184, build: buildSection }],
  settings: settingsBody,
  start: (cx) => {
    const setCfg = (v: Value) => {
      cfg = {
        trackNotice: v.trackNotice !== false,
        artTint: v.artTint !== false,
        artGlow: v.artGlow !== false,
        player: typeof v.player === "string" ? v.player : "",
        playerName: typeof v.playerName === "string" ? v.playerName : "",
        fallback: v.fallback === true,
      };
      applyTint();
      repaint?.();
    };
    island = cx.island;
    watchLyric(lyricWatch); // the lyrics feature may have started first
    const offState = cx.on<Snapshot>("state", (s) => apply(cx, s));
    const offCfg = cx.settings.onChange(setCfg);
    void cx.settings.get().then(setCfg);
    void cx.call<Snapshot>("state").then((s) => s && apply(cx, s, true));
    return () => {
      offState();
      offCfg();
      for (const timer of [lingerTimer, noticeTimer, artTimer]) window.clearTimeout(timer);
      snap = NONE;
      artRev = 0;
      lingering = false;
      island = null;
      lineSwap = null;
      setArt("", "");
    };
  },
});
