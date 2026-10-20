// Notifications and accessories — the page side of the feature (docs/ARCHITECTURE.md §2.3).
// Nothing runs at import: declare surfaces here, begin work in `start`.
//
// Rust tells of a Bluetooth accessory that connected or disconnected and of a
// toast that arrived (crates/notifications-accessories); this turns both into
// notices and owns the Notifications section: the inbox of mirrored toasts, kept
// in this page's memory only, and the battery of the headphones in use.

import "./style.css";
import { defineFeature, type FeatureCtx, type SectionDef, type SectionHost } from "../../core/feature";
import { btn, capsule, card, row, toggle, type Tone } from "../../ui/components";
import { h } from "../../views/dom";

// ── What Rust sends ───────────────────────────────────────────────────────────

interface Battery {
  percent: number | null;
  left: number | null;
  right: number | null;
}

interface Accessory {
  name: string;
  kind: "audio" | "keyboard" | "mouse" | "gamepad" | "other";
  /** The headphone battery is for these only. */
  audio: boolean;
  connected: boolean;
  /** Null where Windows reports none. */
  battery: Battery | null;
  /** The colour of the emptiest level. */
  tone: Tone | null;
}

interface Toast {
  id: number;
  app: string;
  /** What opens the app that sent it; empty when Windows names none. */
  aumid: string;
  title: string;
  body: string;
}

/** Where the mirror stands: what is missing is a state, never an error. */
type MirrorState = "off" | "identity" | "restart" | "consent" | "denied" | "unavailable" | "popups" | "on";

interface Mirror {
  state: MirrorState;
  /** The identity package is registered for this user. */
  registered?: boolean;
  /** What Windows answered to a click that did not work. */
  error?: string | null;
}

const DEFAULTS = { accessories: true, notifications: false };
type Options = typeof DEFAULTS;

/** The inbox keeps this many; the oldest goes first. */
const MAX = 50;

// Icons only this feature draws (src/ui/icons.ts has the shared set).
const HEADPHONES = "M4 14v-2a8 8 0 0 1 16 0v2M4 14h3v6H5.5A1.5 1.5 0 0 1 4 18.5zM20 14h-3v6h1.5a1.5 1.5 0 0 0 1.5-1.5z";
const MOUSE = "M12 3.5a5 5 0 0 1 5 5v7a5 5 0 0 1-10 0v-7a5 5 0 0 1 5-5zM12 7.5v3";
const GAMEPAD = "M7 8h10a4 4 0 0 1 0 8H7a4 4 0 0 1 0-8zM7.5 10.5v3M6 12h3M15 13h.01M17.5 11h.01";
const KIND_ICON: Record<Accessory["kind"], string> = { audio: HEADPHONES, keyboard: "keyboard", mouse: MOUSE, gamepad: GAMEPAD, other: "bluetooth" };

// ── State (set in `start`, cleared when the feature stops) ────────────────────

let opts: Options = DEFAULTS;
let mirror: MirrorState = "off";
/** Mirrored toasts, newest first. Memory only: never stored, gone with the page. */
let inbox: (Toast & { at: number })[] = [];
/** The audio accessories connected now. */
let headphones: Accessory[] = [];
/** One was connected since the start: the section stays, so it does not vanish under the user. */
let headphonesSeen = false;
/** Draws the section again while it is on screen. */
let redraw: (() => void) | null = null;

// ── Texts ─────────────────────────────────────────────────────────────────────

/** "82%", or both sides when the earbuds differ. */
function batteryText(cx: FeatureCtx, b: Battery): string {
  if (b.left != null && b.right != null && b.left !== b.right) return cx.t("L {left}% · R {right}%", { left: b.left, right: b.right });
  return cx.t("{percent}%", { percent: b.percent ?? b.left ?? b.right ?? 0 });
}

/** What the mirror is waiting for, in a sentence: the same words in Settings and in the empty inbox. */
function reason(cx: FeatureCtx, state: MirrorState): string {
  switch (state) {
    case "off":
      return cx.t("Notification mirroring is off. Turn it on in Settings, under Notifications and accessories.");
    case "identity":
      return cx.t("Windows lets an app read notifications only when it has a package identity. Set up registers one for your account: nothing else is installed and no administrator is needed.");
    case "restart":
      return cx.t("The identity is registered. Quit Lintel from the tray and start it again, then come back to Settings.");
    case "consent":
      return cx.t("Windows will ask whether Lintel may read your notifications.");
    case "denied":
      return cx.t("Windows has this turned off. Allow Lintel under Privacy & security, Notifications in Windows Settings.");
    case "unavailable":
      return cx.t("Windows did not let Lintel listen for notifications on this PC.");
    case "popups":
      return cx.t("Lintel reads the pop-up of each notification as Windows shows it, so a notification that Windows hides (Focus, Do Not Disturb) is not seen. A click brings the app forward; the buttons of the original pop-up cannot be pressed from here.");
    case "on":
      return cx.t("New notifications appear on the island and in the Notifications section.");
  }
}

// ── Notices ───────────────────────────────────────────────────────────────────

const hasSection = () => opts.notifications || headphonesSeen;

function onAccessory(cx: FeatureCtx, a: Accessory) {
  headphones = headphones.filter((d) => d.name !== a.name);
  if (a.connected) {
    headphones.push(a);
    headphonesSeen = true;
  }
  redraw?.();
  if (!opts.accessories) return;
  const battery = a.connected && a.battery ? batteryText(cx, a.battery) : null;
  cx.island.notice({
    kind: "accessory",
    title: a.name || cx.t("Bluetooth device"),
    detail: !a.connected ? cx.t("Disconnected") : battery ? cx.t("Connected · {battery}", { battery }) : cx.t("Connected"),
    icon: KIND_ICON[a.kind] ?? "bluetooth",
    // The chip takes the colour of the level; a device that is gone has none.
    tone: (a.connected && a.tone) || undefined,
    section: hasSection() ? "notifications" : undefined,
  });
}

function onToast(cx: FeatureCtx, n: Toast) {
  if (!opts.notifications) return;
  inbox = [{ ...n, at: Date.now() }, ...inbox.filter((old) => old.id !== n.id)].slice(0, MAX);
  redraw?.();
  cx.island.notice({
    kind: "notification",
    title: n.title || n.app || cx.t("Notifications"),
    detail: n.body || (n.title ? n.app : undefined),
    icon: "bell",
    section: "notifications",
    action: n.aumid || n.app ? { label: cx.t("Open"), run: () => void cx.call("open", { aumid: n.aumid, app: n.app }) } : undefined,
  });
}

// ── The Notifications section ─────────────────────────────────────────────────

const clock = (ms: number) => new Date(ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });

function buildSection(cx: FeatureCtx): SectionHost {
  const head = h("div", { class: "f-notifications-accessories-head" });
  const list = h("div", { class: "f-notifications-accessories-list" });
  const el = h("div", { class: "f-notifications-accessories-page" }, head, list);
  let on = false;

  const draw = () => {
    const clear = btn(cx.t("Clear"), "ghost", () => {
      inbox = [];
      draw();
    });
    clear.disabled = inbox.length === 0;
    head.replaceChildren(
      ...headphones.map((d) => {
        const chip = capsule({
          icon: KIND_ICON[d.kind] ?? "bluetooth",
          text: d.battery ? cx.t("{name} · {battery}", { name: d.name, battery: batteryText(cx, d.battery) }) : d.name,
          tone: d.tone ?? undefined,
        });
        chip.title = cx.t("Open Bluetooth settings");
        chip.addEventListener("click", () => void cx.call("bluetooth"));
        return chip;
      }),
      h("span", { class: "isl-gap" }),
      clear,
    );
    if (inbox.length === 0) {
      // Why it is empty, in a sentence; what to do about it is in Settings.
      const text =
        !opts.notifications || mirror === "off" ? reason(cx, "off")
        : mirror === "on" || mirror === "popups" ? cx.t("No notifications yet.")
        : cx.t("Notification mirroring is not ready yet. Settings, under Notifications and accessories, says what is missing.");
      list.replaceChildren(h("p", { class: "f-notifications-accessories-empty", text }));
      return;
    }
    list.replaceChildren(
      ...inbox.map((n) =>
        h(
          "button",
          // A click brings the app that sent it forward; one without an app only sits there.
          { class: "f-notifications-accessories-item", onclick: () => (n.aumid || n.app) && void cx.call("open", { aumid: n.aumid, app: n.app }) },
          h("b", { text: n.title || n.app }),
          h("small", { text: n.title && n.app ? cx.t("{app} · {time}", { app: n.app, time: clock(n.at) }) : clock(n.at) }),
          n.body ? h("span", { text: n.body }) : null,
        ),
      ),
    );
  };
  // The battery of the headphones is read when the section is looked at, not on a timer.
  const load = () =>
    void cx.call<Accessory[]>("devices").then((all) => {
      if (!all) return;
      headphones = all;
      if (on) draw();
    });

  return {
    el,
    show() {
      on = true;
      redraw = draw;
      draw();
      load();
    },
    hide() {
      on = false;
      redraw = null;
    },
    sync() {},
  };
}

// All 26 letters are taken (docs/ARCHITECTURE.md §5): the section is reached from the gallery.
const section: SectionDef = { id: "notifications", title: "Notifications", icon: "bell", height: 280, build: buildSection };

// ── Settings ──────────────────────────────────────────────────────────────────

const STATUS: Record<MirrorState, string> = {
  off: "Off",
  identity: "Not set up",
  restart: "Restart needed",
  consent: "Permission needed",
  denied: "Turned off in Windows",
  unavailable: "Not available",
  popups: "On, reading pop-ups",
  on: "On",
};

function settingsPage(cx: FeatureCtx): HTMLElement {
  const el = h("div", { class: "f-notifications-accessories-settings" });
  const status = h("div", { class: "f-notifications-accessories-status" });

  // The row under the switch: where the mirror stands, why, and the one click that moves it on.
  const drawStatus = (m: Mirror | null) => {
    if (!m || m.state === "off") return status.replaceChildren();
    const call = (method: string) => () => void cx.call<Mirror>(method).then(drawStatus);
    const next =
      m.state === "identity" ? btn(cx.t("Set up"), "primary", call("setup"))
      : m.state === "consent" ? btn(cx.t("Ask Windows"), "primary", call("setup"))
      : m.state === "denied" ? btn(cx.t("Open Windows settings"), "secondary", () => void cx.call("privacy"))
      : h("span");
    const hint = !m.error ? reason(cx, m.state)
      : m.state === "identity" ? cx.t("Windows did not accept the identity: {reason} A build that is not signed needs Developer Mode, in Windows Settings under System, For developers.", { reason: m.error })
      : cx.t("Windows answered: {reason}", { reason: m.error });
    status.replaceChildren(
      card([
        row(cx.t(STATUS[m.state]), next, hint),
        m.registered
          ? row(
              cx.t("Windows identity"),
              btn(cx.t("Remove"), "ghost", call("remove")),
              cx.t("Registered for your account. Removing it closes Lintel if it runs with the identity; start it again afterwards."),
            )
          : null,
      ]),
    );
  };
  // The answer to Windows' question, and every switch of the setting, arrive as this event.
  const off = cx.on<Mirror>("mirror", (m) => (el.isConnected ? drawStatus(m) : off()));

  void cx.settings.get().then((stored) => {
    const now = { ...DEFAULTS, ...stored } as Options;
    const mirrorSwitch = toggle(now.notifications, (on) => void cx.settings.patch({ notifications: on }));
    el.append(
      h("div", { class: "section-title", text: cx.t("Accessories") }),
      card([
        row(
          cx.t("Bluetooth accessories"),
          toggle(now.accessories, (on) => void cx.settings.patch({ accessories: on })),
          cx.t("A notice when a Bluetooth device connects or disconnects, with its battery when Windows reports one."),
        ),
      ]),
      h("div", { class: "section-title", text: cx.t("Notifications") }),
      card([
        row(
          cx.t("Show notifications from other apps (read as they pop up)"),
          mirrorSwitch,
          cx.t(
            "Shows the notifications of your other apps on the island and keeps the last {count} in memory until you clear them or quit Lintel. Lintel reads the app, title and text of each pop-up as Windows shows it; nothing leaves this PC and nothing is saved to disk. Off by default.",
            { count: MAX },
          ),
        ),
      ]),
      status,
    );
    void cx.call<Mirror>("mirror").then(drawStatus);
  });
  return el;
}

// ── The feature ───────────────────────────────────────────────────────────────

export default defineFeature({
  id: "notifications-accessories",

  // There only when it has something to hold: the mirror is wanted, or headphones were connected.
  get sections() {
    return hasSection() ? [section] : [];
  },

  settings: settingsPage,

  start: (cx) => {
    const applyOptions = (stored: object) => {
      opts = { ...DEFAULTS, ...stored } as Options;
      // Switched off: what was mirrored is forgotten.
      if (!opts.notifications) inbox = [];
      redraw?.();
    };
    void cx.settings.get().then(applyOptions);
    const offs = [
      cx.settings.onChange(applyOptions),
      cx.on<Accessory>("accessory", (a) => onAccessory(cx, a)),
      cx.on<Toast>("notification", (n) => onToast(cx, n)),
      cx.on<Mirror>("mirror", (m) => {
        mirror = m.state;
        redraw?.();
      }),
    ];
    // Events only tell of changes: this is where we start from.
    void cx.call<Mirror>("mirror").then((m) => {
      if (m) mirror = m.state;
      redraw?.();
    });
    void cx.call<Accessory[]>("devices").then((all) => {
      headphones = all ?? [];
      headphonesSeen ||= headphones.length > 0;
      redraw?.();
    });

    return () => {
      for (const off of offs) off();
      opts = DEFAULTS;
      mirror = "off";
      inbox = [];
      headphones = [];
      headphonesSeen = false;
      redraw = null;
    };
  },
});
