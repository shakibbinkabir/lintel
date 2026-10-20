// Companion — the mascot on the desktop (docs/ARCHITECTURE.md §9).
//
// Three parts. `start` (island page) turns what happens on the machine into a word to
// Rust (`react`); the window page ("main", opened by Rust) draws the character, its
// outfit and its reactions and plays them through a gate; the settings page is the
// wardrobe. The character rests as one still frame: frames run only while something moves.

import "./style.css";
import { defineFeature, type FeatureCtx } from "../../core/feature";
import { Bridge, onEvent } from "../../core/bridge";
import { Sound } from "../../core/sound";
import { BOT_EMOTES, BotEngine, wipe } from "../../mascot/engine";
import { OUTFITS, drawOutfit, resolveOutfit, type OutfitId } from "../../mascot/outfits";
import { REACTIONS, companionSound, drawSleep, playReaction, reactionGate, type CompanionEvent, type ReactionId } from "../../mascot/reactions";
import { btn, card, row, tile, toggle } from "../../ui/components";
import { h } from "../../views/dom";
import { DIZZY_MS, patCounter, showPat } from "./pats";

const P = "f-companion-";

// ── The window's geometry (logical px; crates/companion/src/lib.rs keeps SIZE in step) ──

const SIZE = 112;
/** The character's centre and diameter in the window: whole device pixels at 100 % and 125 %. */
const CX = 56;
const CY = 64;
const D = 56;
/** What takes the pointer: the body; with an outfit its head too; during a reaction the whole disc. */
const BODY: Ellipse = [CX, CY, 31, 31];
const HEAD: Ellipse = [CX, 40, 36, 28];
const WHOLE: Ellipse = [56, 56, 56, 56];
type Ellipse = [number, number, number, number];

const REDUCED = matchMedia("(prefers-reduced-motion: reduce)");
const SLEEP_AFTER = 120_000;

interface Opts { outfit: string; docked: boolean; reactions: boolean; sleep: boolean; sounds: boolean }
const opts = (v: Record<string, unknown>): Opts => ({
  outfit: typeof v.outfit === "string" ? v.outfit : "auto",
  docked: v.docked === true,
  reactions: v.reactions !== false,
  sleep: v.sleep !== false,
  sounds: v.sounds !== false,
});

// ── The window ────────────────────────────────────────────────────────────────

function mountCompanion(root: HTMLElement, cx: FeatureCtx) {
  root.className = `${P}stage`;
  const canvas = h("canvas", { class: `${P}canvas` }) as HTMLCanvasElement;
  const fit = () => {
    const dpr = window.devicePixelRatio || 1;
    canvas.width = Math.round(SIZE * dpr);
    canvas.height = Math.round(SIZE * dpr);
  };
  fit();
  root.append(canvas);
  const ctx = canvas.getContext("2d")!;

  const engine = new BotEngine("idle");
  let o: Opts = opts({});
  let outfit: OutfitId | null = null;
  let playing: ReturnType<typeof playReaction> | null = null;
  let playStart = 0;
  let asleep = false;
  let dragging = false;
  let raf = 0;
  let last = 0;
  let until = 0;
  let sleepTimer: ReturnType<typeof setTimeout> | undefined;
  let loveTimer: ReturnType<typeof setTimeout> | undefined;
  let reducedEnd: ReturnType<typeof setTimeout> | undefined;
  let stillTimer: ReturnType<typeof setTimeout> | undefined;
  let master = true; // Settings, Sounds: the master switch (the tray's mute and the shortcut set it too)
  let visible = true; // false while Rust hides the window (full screen, lock)
  let lastSound = -Infinity;

  /** Plays the sound that goes with `ev`, unless companionSound holds it back; `last` is when the last one played. */
  const hear = (ev: CompanionEvent, last = lastSound) => {
    const name = companionSound(ev, { on: o.sounds, master, visible, now: performance.now(), last });
    if (!name) return;
    lastSound = performance.now();
    Sound.play(name);
    Sound.idle();
  };

  const draw = () => {
    // A display with another scale: the backing store follows it.
    if (canvas.width !== Math.round(SIZE * (window.devicePixelRatio || 1))) fit();
    wipe(ctx);
    ctx.setTransform(canvas.width / SIZE, 0, 0, canvas.height / SIZE, 0, 0);
    engine.draw(ctx, CX, CY, D);
    if (outfit) drawOutfit(ctx, outfit, engine, CX, CY, D);
    if (playing) playing.draw(ctx, CX, CY, D, REDUCED.matches ? playing.seconds * 0.3 : (performance.now() - playStart) / 1000);
    if (asleep) drawSleep(ctx, CX, CY, D);
  };

  /** The bob passes through its middle twice a cycle: stop there, so the still frame is the body at rest. */
  const atRest = () => REDUCED.matches || engine.state === "sleeping" || Math.abs(Math.sin((engine.t * Math.PI * 2) / 2.8)) < 0.07;

  const frame = (ts: number) => {
    const dt = Math.min(0.05, (ts - (last || ts)) / 1000);
    last = ts;
    engine.update(dt);
    if (playing) {
      const t = (performance.now() - playStart) / 1000;
      playing.step(t);
      if (t >= playing.seconds) endReaction();
    }
    draw();
    if (playing || performance.now() < until || !atRest()) raf = requestAnimationFrame(frame);
    else raf = 0;
  };

  /** Runs frames for at least `ms` more, then stops (a still frame at the end). */
  const kick = (ms: number) => {
    until = Math.max(until, performance.now() + ms);
    if (REDUCED.matches) {
      // No motion: the face changes, one frame now and one when the moment is over.
      engine.update(0.35);
      draw();
      clearTimeout(stillTimer);
      stillTimer = setTimeout(() => (engine.update(ms / 1000), draw()), ms);
      return;
    }
    if (!raf) {
      last = 0;
      raf = requestAnimationFrame(frame);
    }
  };

  const region = () => {
    const e: Ellipse[] = [BODY];
    if (outfit) e.push(HEAD);
    if (playing) e.push(WHOLE);
    void cx.call("region", { ellipses: e });
  };

  // ── Rest, sleep, wake ───────────────────────────────────────────────────────

  const touch = () => {
    clearTimeout(sleepTimer);
    if (o.sleep && !playing) sleepTimer = setTimeout(sleep, SLEEP_AFTER);
  };
  function sleep() {
    hear("sleep");
    asleep = true;
    engine.setState("sleeping");
    kick(900);
  }
  const wake = (yawn = false) => {
    touch();
    if (!asleep) return;
    if (yawn) hear("yawn");
    asleep = false;
    engine.setState("idle");
    if (yawn) engine.triggerEmote("yawn");
    kick((yawn ? BOT_EMOTES.yawn : 0.6) * 1000 + 200);
  };

  // ── Pats ────────────────────────────────────────────────────────────────────

  const pat = patCounter();
  let dizzyTimer: ReturnType<typeof setTimeout> | undefined;
  /** The dizzy spell is over, or a reaction takes its place: the face is the state's again. */
  const steady = () => {
    clearTimeout(dizzyTimer);
    if (engine.face === "dizzy") engine.face = null;
  };

  // ── Reactions ───────────────────────────────────────────────────────────────

  function endReaction() {
    clearTimeout(reducedEnd);
    if (!playing) return;
    playing.end();
    playing = null;
    region();
    kick(700);
    touch();
  }
  const play = (id: ReactionId) => {
    if (dragging) return;
    wake();
    clearTimeout(sleepTimer);
    endReaction();
    steady();
    hear(id);
    playing = playReaction(engine, id);
    playStart = performance.now();
    region();
    if (REDUCED.matches) {
      // The emote or face stands for the length of the reaction, with one still frame of its shapes.
      playing.step(playing.seconds * 0.6);
      engine.update(1);
      draw();
      reducedEnd = setTimeout(endReaction, playing.seconds * 1000);
    } else {
      kick(playing.seconds * 1000);
    }
  };
  const gate = reactionGate(play);

  // ── The pointer ─────────────────────────────────────────────────────────────

  let down: { x: number; y: number } | null = null;
  let moved = false;

  const look = (e: PointerEvent) => {
    engine.lookX = Math.max(-1, Math.min(1, (e.offsetX - CX) / 40));
    engine.lookY = Math.max(-1, Math.min(1, -(e.offsetY - CY) / 40));
  };
  canvas.addEventListener("pointerenter", () => {
    wake(true);
    kick(1500);
    loveTimer = setTimeout(() => {
      if (!dragging && !playing) {
        engine.triggerEmote("love");
        kick(BOT_EMOTES.love * 1000 + 300);
      }
    }, 1900);
  });
  canvas.addEventListener("pointermove", (e) => {
    look(e);
    if (down && !dragging && Math.hypot(e.clientX - down.x, e.clientY - down.y) > 4) {
      dragging = moved = true;
      clearTimeout(loveTimer);
      engine.triggerEmote("surprised");
      hear("lift");
      void cx.call("drag");
    }
    kick(dragging ? 600 : 900);
  });
  canvas.addEventListener("pointerleave", () => {
    clearTimeout(loveTimer);
    engine.lookX = engine.lookY = 0;
    kick(800);
  });
  canvas.addEventListener("pointerdown", (e) => {
    if (e.button !== 0) return;
    canvas.setPointerCapture(e.pointerId);
    down = { x: e.clientX, y: e.clientY };
    moved = false;
  });
  canvas.addEventListener("pointerup", () => {
    down = null;
    if (moved) return;
    // A pat. Three quick ones make it dizzy; while it spins a click is nothing.
    const what = pat(performance.now());
    clearTimeout(loveTimer);
    touch();
    if (!what) return;
    showPat(engine, what);
    if (what === "dizzy") {
      // The end of three pats: the gap that spaces the pats' own sounds does not hold this one back.
      hear(what, -Infinity);
      dizzyTimer = setTimeout(() => (steady(), kick(700)), DIZZY_MS);
      kick(DIZZY_MS + 700);
    } else {
      hear(what);
      kick(BOT_EMOTES.happy * 1000 + 300);
    }
  });
  canvas.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    void cx.call("home");
  });

  // ── Wiring ──────────────────────────────────────────────────────────────────

  const apply = (v: Record<string, unknown>) => {
    o = opts(v);
    if (o.sounds) void Sound.preload();
    outfit = resolveOutfit(o.outfit);
    region();
    touch();
    kick(300);
  };
  void cx.settings.get().then(apply);
  const offs = [
    cx.settings.onChange(apply),
    cx.on<{ kind: ReactionId; preview?: boolean }>("react", ({ kind, preview }) => {
      if (!REACTIONS[kind] || !o.reactions) return;
      if (preview) play(kind);
      else gate.fire(kind);
    }),
    cx.on("dragend", () => {
      dragging = false;
      hear("set");
      kick(500);
    }),
    // Back from lock, sleep or a full-screen app: a frame, and a yawn if it dozed off.
    cx.on<{ on: boolean }>("visible", ({ on }) => ((visible = on), on && (wake(true), kick(300)))),
    cx.on("landed", () => hear("land")),
    cx.on("homing", () => hear("home")),
  ];
  // The host's master switch and volume (Settings, Sounds), read now and whenever they change.
  const host = (s: { soundEnabled?: boolean; soundVolume?: number }) => {
    master = s.soundEnabled !== false;
    Sound.setEnabled(master);
    if (typeof s.soundVolume === "number") Sound.setVolume(s.soundVolume);
  };
  void Bridge.boot().then((b) => b && host(b.settings));
  void onEvent<{ soundEnabled?: boolean; soundVolume?: number }>("settings-changed", host);
  // "Show the mascot" (Settings, Mascot) hides the companion too.
  void onEvent<{ showMascot: boolean }>("settings-changed", (s) => void cx.call("mascot", { shown: s.showMascot }));
  const motion = () => void cx.call("motion", { reduced: REDUCED.matches });
  REDUCED.addEventListener("change", motion);
  motion();
  window.addEventListener("pagehide", () => {
    offs.forEach((f) => f());
    gate.clear();
    cancelAnimationFrame(raf);
    clearTimeout(sleepTimer);
  });

  region();
  kick(1200); // arrives: a blink and a settle, then a still frame
  touch();
}

// ── The wardrobe (settings page) ──────────────────────────────────────────────

/** Draws the Visor in an outfit on a small canvas: one frame, never animated. */
function swatch(id: OutfitId | null): HTMLCanvasElement {
  const dpr = window.devicePixelRatio || 1;
  const c = h("canvas", { class: `${P}swatch-art` }) as HTMLCanvasElement;
  c.width = Math.round(64 * dpr);
  c.height = Math.round(64 * dpr);
  const ctx = c.getContext("2d")!;
  const e = new BotEngine("idle");
  e.isMini = true;
  e.blinkIn = 99;
  e.update(1);
  e.t = 0; // the bob at its middle
  ctx.scale(c.width / 64, c.height / 64);
  e.draw(ctx, 32, 40, 38);
  if (id) drawOutfit(ctx, id, e, 32, 40, 38);
  return c;
}

const TRY: { id: ReactionId; label: string; icon: string }[] = [
  { id: "download", label: "Download", icon: "download" },
  { id: "agent", label: "Agent done", icon: "check" },
  { id: "charger", label: "Charger", icon: "battery" },
  { id: "timer", label: "Timer", icon: "timer" },
  { id: "screenshot", label: "Screenshot", icon: "shot" },
];

function settingsPage(cx: FeatureCtx): HTMLElement {
  const t = cx.t;
  const box = h("div", { class: `${P}settings` });
  void cx.settings.get().then((v) => {
    const s = opts(v);
    const patch = (p: object) => void cx.settings.patch(p);

    const items: { setting: string; label: string; art: OutfitId | null }[] = [
      { setting: "auto", label: t("Auto"), art: resolveOutfit("auto") },
      { setting: "none", label: t("None"), art: null },
      ...OUTFITS.map((x) => ({ setting: x.id as string, label: t(x.label), art: x.id })),
    ];
    const tiles = new Map<string, HTMLButtonElement>();
    const mark = (current: string) => tiles.forEach((el, k) => el.classList.toggle(`${P}on`, k === current));
    const shelf = h("div", { class: `${P}grid` });
    for (const it of items) {
      const el = tile("sparkle", it.label, () => (mark(it.setting), patch({ outfit: it.setting })));
      el.classList.add(`${P}swatch`);
      el.querySelector("svg")!.replaceWith(swatch(it.art));
      tiles.set(it.setting, el);
      shelf.append(el);
    }
    mark(s.outfit);

    const trying = h("div", { class: `${P}grid` });
    for (const r of TRY) {
      trying.append(tile(r.icon, t(r.label), () => void cx.call("react", { kind: r.id, preview: true })));
    }

    box.append(
      row(t("Reactions"), toggle(s.reactions, (on) => patch({ reactions: on })), t("Plays a short moment when a download or an agent finishes, the charger goes in, a timer ends or you take a screenshot")),
      row(t("Companion sounds"), toggle(s.sounds, (on) => patch({ sounds: on })), t("Short, quiet sounds for its flights, pats, naps and reactions. Follows the sound switch and volume in Settings, Sounds")),
      row(t("Sleep when idle"), toggle(s.sleep, (on) => patch({ sleep: on })), t("After two minutes without a visit it dozes off and draws nothing")),
      row(t("On the desktop"), btn(t("Send out or bring back"), "secondary", () => void cx.call("toggle")), t("Drag it anywhere, right-click to send it home")),
      h("div", { class: "section-title", text: t("Outfit") }),
      card(shelf),
      h("div", { class: "section-title", text: t("Try a reaction") }),
      card(trying),
    );
  });
  return box;
}

export default defineFeature({
  id: "companion",

  windows: { main: mountCompanion },
  settings: settingsPage,

  start(cx) {
    // The companion's window plays what it is told; this page is where the rest of the app can be heard.
    const say = (kind: ReactionId) => void cx.call("react", { kind });
    let ac: boolean | null = null;
    const offs = [
      onEvent<{ hook_event_name?: string }>("hook", (p) => p?.hook_event_name === "Stop" && say("agent")),
      onEvent<{ ac: boolean }>("huds-power:power", (p) => {
        if (ac === false && p.ac) say("charger");
        ac = p.ac;
      }),
      onEvent("timers-downloads:done", () => say("timer")),
      onEvent("capture:captured", () => say("screenshot")),
    ];
    return () => offs.forEach((o) => void o.then((f) => f()));
  },
});
