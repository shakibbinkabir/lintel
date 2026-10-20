// Calendar — the page side of the feature (docs/ARCHITECTURE.md §2.3).
// Rust reads the ICS links and expands the recurrences; this file draws the week, the reminders and the
// countdown, and adds the Cal.com bookings that the integrations feature already announces.

import "./style.css";
import { defineFeature, type FeatureCtx } from "../../core/feature";
import { Bridge, onEvent } from "../../core/bridge";
import { h } from "../../views/dom";
import { language, tn } from "../../i18n/i18n";
import { icon } from "../../ui/icons";
import { btn, card, field, iconBtn, row, stripRow, toggle } from "../../ui/components";

interface Ev {
  title: string;
  location: string;
  description?: string;
  start: number;
  end: number;
  allDay: boolean;
  cal: string;
  color: string;
  unsupported?: string | null;
  link?: string | null;
  calcom?: boolean;
}
interface Week { events: Ev[]; unsupported: { title: string; cal: string; reason: string }[] }
interface CalInfo { id: number; name: string; color: string; error: string | null; loaded: boolean; events: number; unsupported: number }
interface Rem { id: number; text: string; done: boolean }
interface State { calendars: CalInfo[]; reminders: Rem[] }
interface Booking { id: string; title: string; start: string; status: string; attendeeName?: string | null }

const HOUR = 3600e3;
const MIN = 60e3;
const CALCOM_COLOR = "#c9956a";

let C: FeatureCtx | undefined; // the island page's context; settings use their own
let opts = { countdown: true, calcom: true, description: true, lead: 60, alarm: 0 };
/** What the strip shows: events that start together make one card (Rust decides, see ics::cards). */
interface Card { start: number; inProgress: boolean; events: { title: string; link: string | null }[] }
let cards: Card[] = [];
const alarmed = new Set<string>();
const T = (s: string) => C?.t(s) ?? s;
let upcoming: Ev[] = [];
let upcomingEnd = 0;
let bookings: Ev[] = [];
let hasCals = false;
let timer: number | undefined;
const watchers = new Set<() => void>(); // sections on screen, told when data changes

// ── readings ─────────────────────────────────────────────────────────────────

const pad = (n: number) => String(n).padStart(2, "0");
const clock = (ms: number) => new Date(ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
const midnight = (d: Date) => new Date(d.getFullYear(), d.getMonth(), d.getDate());
const addDays = (d: Date, n: number) => new Date(d.getFullYear(), d.getMonth(), d.getDate() + n);
const mondayOf = (d: Date) => addDays(midnight(d), -((d.getDay() + 6) % 7));
const same = (a: Date, b: Date) => a.getFullYear() === b.getFullYear() && a.getMonth() === b.getMonth() && a.getDate() === b.getDate();
const mmss = (ms: number) => {
  const s = Math.max(0, Math.ceil(ms / 1000));
  return `${pad(Math.floor(s / 60))}:${pad(s % 60)}`;
};

const everything = () => (opts.calcom ? [...upcoming, ...bookings] : upcoming);
/** A Cal.com booking carries no end time: it is shown as 30 minutes. */
function toBooking(b: Booking): Ev | undefined {
  const start = Date.parse(b.start);
  if (!Number.isFinite(start) || /cancel|reject/i.test(b.status)) return;
  return {
    title: b.title,
    location: b.attendeeName ?? "",
    start,
    end: start + 30 * 60e3,
    allDay: false,
    cal: "Cal.com",
    color: CALCOM_COLOR,
    calcom: true,
  };
}

// ── the countdown ────────────────────────────────────────────────────────────

/** Asks Rust what the strip shows now (the page adds the Cal.com bookings, which have no link). */
async function loadCards() {
  if (!C) return;
  const now = Date.now();
  const extra = opts.calcom ? bookings.filter((e) => e.end > now && e.start - now <= opts.lead * MIN) : [];
  const r = await C.call<{ cards: Card[] }>("cards", { lead: opts.lead, extra });
  cards = r?.cards ?? [];
  C?.island.activityChanged("countdown");
}

/** Loads the next 26 hours, then sleeps until the next moment that changes the strip. */
async function loadUpcoming() {
  if (!C) return;
  const now = Date.now();
  const r = await C.call<Week>("events", { from: now, to: now + 26 * HOUR });
  upcoming = r?.events ?? [];
  upcomingEnd = now + 26 * HOUR;
  await loadCards();
  alarms();
  arm();
}

/** A notice at the default lead before an event, once per event. */
function alarms() {
  const now = Date.now();
  if (!C || !opts.alarm) return;
  for (const e of everything()) {
    const key = `${e.start}|${e.title}`;
    if (e.allDay || alarmed.has(key) || e.start <= now || e.start - now > opts.alarm * MIN + 1500) continue;
    alarmed.add(key);
    C.island.notice({
      kind: "notification",
      title: e.title,
      detail: clock(e.start),
      icon: "calendar",
      tone: "info",
      section: "calendar",
      action: e.link ? { label: C.t("Join"), run: () => join(e.link!) } : undefined,
    });
  }
}

/** Opens a link the Rust side found, through the app's open-URL path (no shell). */
const join = (url: string) => void Bridge.openUrl(url);

function arm() {
  clearTimeout(timer);
  const now = Date.now();
  const lead = opts.lead * MIN, alarm = opts.alarm * MIN;
  const marks = everything().flatMap((e) => (e.allDay ? [] : [e.start - lead, e.start, e.end, ...(alarm ? [e.start - alarm] : [])])).filter((t) => t > now + 500);
  const reload = upcomingEnd - HOUR;
  const next = Math.min(reload, ...marks);
  timer = window.setTimeout(async () => {
    if (next === reload) loadUpcoming();
    else {
      await loadCards();
      alarms();
      arm();
    }
  }, Math.min(Math.max(next - now, 1000), 2 ** 31 - 1));
}

export default defineFeature({
  id: "calendar",
  voice: () => import("./voice"),

  activities: [
    {
      id: "countdown",
      order: 4,
      tick: true,
      section: "calendar",
      isActive: () => opts.countdown && cards.length > 0,
      render() {
        // Several cards take turns, one every 5 seconds; "+N" counts the other events.
        const c = cards[Math.floor(Date.now() / 5000) % Math.max(cards.length, 1)];
        if (!c) return stripRow({ icon: "calendar", title: "" });
        const total = cards.reduce((n, k) => n + k.events.length, 0);
        const link = c.events.find((e) => e.link)?.link;
        return stripRow({
          icon: "calendar",
          tone: c.inProgress ? "ok" : "info",
          title: c.events[0].title,
          detail: `${c.inProgress ? T("In progress") : clock(c.start)}${total > 1 ? `  +${total - 1}` : ""}`,
          value: c.inProgress ? undefined : mmss(c.start - Date.now()),
          action: link ? { label: T("Join"), run: () => join(link) } : undefined,
        });
      },
      mark: () => h("span", { class: "isl-ico info" }, icon("calendar", 13)),
    },
  ],

  shortcuts: { "add-reminder": (cx) => cx.island.open("reminders") },

  sections: [
    { id: "calendar", title: "Calendar", icon: "calendar", letter: "c", height: 270, build: (cx) => weekPage(cx) },
    { id: "reminders", title: "Reminders", icon: "checkc", letter: "e", height: 250, keyboard: true, build: (cx) => remindersPage(cx) },
  ],

  settings: (cx) => settingsPage(cx),

  start(cx) {
    C = cx;
    const num = (v: unknown, d: number) => (typeof v === "number" && v >= 0 ? v : d);
    const read = (s: Record<string, unknown>) => (opts = { countdown: s.countdown !== false, calcom: s.calcom !== false, description: s.description !== false, lead: Math.min(240, Math.max(5, num(s.lead, 60))), alarm: num(s.alarm, 0) });
    cx.settings.get().then((s) => {
      read(s);
      loadUpcoming();
    });
    const offs = [
      cx.settings.onChange((s) => {
        read(s);
        loadCards().then(() => {
          alarms();
          arm();
        });
        watchers.forEach((w) => w());
      }),
      cx.on("changed", () => {
        loadUpcoming();
        watchers.forEach((w) => w());
      }),
    ];
    // The integrations feature polls Cal.com; its update is only read here, never asked for.
    let offBooking = () => {};
    onEvent<{ id: string; data?: { bookings?: Booking[] } }>("integration", (u) => {
      if (u.id !== "integration_calcom") return;
      bookings = (u.data?.bookings ?? []).map(toBooking).filter((b): b is Ev => !!b);
      loadCards();
      watchers.forEach((w) => w());
      arm();
    }).then((off) => (offBooking = off));
    return () => {
      offs.forEach((f) => f());
      offBooking();
      clearTimeout(timer);
      C = undefined;
      upcoming = bookings = [];
      cards = [];
      alarmed.clear();
      watchers.clear();
    };
  },
});

// ── Calendar section: the day wheel ──────────────────────────────────────────

function weekPage(cx: FeatureCtx) {
  const el = h("div", { class: "f-calendar-page" });
  let start = mondayOf(new Date());
  let sel = midnight(new Date());
  let week: Week = { events: [], unsupported: [] };
  let shown = false;
  let latch = 0;

  async function load() {
    const r = await cx.call<Week>("events", { from: start.getTime(), to: addDays(start, 7).getTime() });
    week = r ?? week;
    const s = await cx.call<State>("state");
    hasCals = !!s?.calendars.length;
    if (shown) draw();
  }
  const go = (day: Date) => {
    sel = midnight(day);
    const m = mondayOf(sel);
    if (!same(m, start)) {
      start = m;
      load();
    }
    draw();
  };

  const dayEvents = (d: Date) => {
    const a = d.getTime(), b = addDays(d, 1).getTime();
    const mine = [...week.events, ...(opts.calcom ? bookings : [])].filter((e) => e.start < b && e.end > a && !(e.allDay && e.end <= a));
    // a zero-length event on the day itself
    return mine.sort((x, y) => Number(y.allDay) - Number(x.allDay) || x.start - y.start);
  };

  function draw() {
    const today = midnight(new Date());
    const end = addDays(start, 6);
    // Day and month names in the language of the app; the system's own date format when that is the same language.
    const system = navigator.language.toLowerCase().startsWith(language().split("-")[0].toLowerCase());
    const fmt = (d: Date, o: Intl.DateTimeFormatOptions) => d.toLocaleDateString(system ? [] : language(), o);
    const head = h("div", { class: "f-calendar-head" },
      h("b", { text: `${fmt(start, { month: "short", day: "numeric" })} – ${fmt(end, { month: start.getMonth() === end.getMonth() ? undefined : "short", day: "numeric" })}` }),
      h("span", { class: "isl-gap" }),
      iconBtn("chevr", cx.t("Previous week"), () => go(addDays(sel, -7))),
      btn(cx.t("Today"), "ghost", () => go(new Date())),
      iconBtn("chevr", cx.t("Next week"), () => go(addDays(sel, 7))));
    head.children[2].classList.add("f-calendar-flip");

    const strip = h("div", { class: "f-calendar-days" },
      ...Array.from({ length: 7 }, (_, i) => {
        const d = addDays(start, i);
        const n = dayEvents(d).length;
        return h("button", { class: `f-calendar-day${same(d, sel) ? " on" : ""}${same(d, today) ? " today" : ""}`, onclick: () => go(d) },
          h("small", { text: fmt(d, { weekday: "short" }) }),
          h("b", { text: String(d.getDate()) }),
          h("i", { class: n ? "has" : "" }));
      }));
    // wheel over the strip turns the day; one step per gesture burst so inertia does not run away
    strip.addEventListener("wheel", (e) => {
      e.preventDefault();
      if (Math.abs(e.deltaY) < 2 && Math.abs(e.deltaX) < 2) return;
      const now = performance.now();
      if (now - latch < 140) return;
      latch = now;
      go(addDays(sel, (e.deltaY || e.deltaX) > 0 ? 1 : -1));
    }, { passive: false });

    const list = dayEvents(sel);
    const now = Date.now();
    const body = list.length
      ? list.map((e) =>
          h("div", { class: "f-calendar-ev", onclick: (ev: Event) => ((ev.currentTarget as HTMLElement).querySelector(".f-calendar-desc")?.classList.toggle("open")) },
            h("span", { class: "f-calendar-bar", style: `background:${e.color}` }),
            h("span", { class: "f-calendar-time", text: e.allDay ? cx.t("All day") : `${clock(e.start)}${e.end > e.start ? " – " + clock(e.end) : ""}` }),
            h("span", { class: "f-calendar-name" },
              h("b", { text: e.title }),
              e.unsupported ? h("small", { class: "warn", text: cx.t("Unsupported rule") }) : e.location ? h("small", { text: e.location }) : null,
              opts.description && e.description ? h("small", { class: "f-calendar-desc", text: e.description }) : null),
            e.calcom ? h("small", { class: "f-calendar-tag", text: "Cal.com" }) : null,
            e.link && e.end > now ? btn(cx.t("Join"), "primary", () => join(e.link!)) : null,
            !e.allDay && e.start <= now && e.end > now ? h("small", { class: "f-calendar-tag now", text: cx.t("Now") }) : null))
      : [h("p", { class: "f-calendar-empty", text: hasCals || bookings.length ? cx.t("Nothing planned.") : cx.t("No calendars yet. Paste an ICS link in Settings, Calendar.") })];
    const note = week.unsupported.length
      ? h("p", { class: "f-calendar-note", text: `${cx.t("Unsupported rule")}: ${[...new Set(week.unsupported.map((u) => u.title))].slice(0, 3).join(", ")}` })
      : "";
    el.replaceChildren(head, strip, h("div", { class: "f-calendar-list" }, ...body), note);
  }

  const watcher = () => load();
  return {
    el,
    show() {
      shown = true;
      start = mondayOf(new Date());
      sel = midnight(new Date());
      watchers.add(watcher);
      cx.call("seen", { visible: true });
      draw();
      load();
    },
    hide() {
      shown = false;
      watchers.delete(watcher);
      cx.call("seen", { visible: false });
    },
    sync() {
      if (shown) draw();
    },
  };
}

// ── Reminders section ────────────────────────────────────────────────────────

function remindersPage(cx: FeatureCtx) {
  const el = h("div", { class: "f-calendar-page" });
  let rems: Rem[] = [];
  const take = (s: State | null) => {
    if (s) rems = s.reminders;
    draw();
  };
  const input = field({
    placeholder: cx.t("Add a reminder"),
    icon: "plus",
    onEnter: (v) => {
      if (!v.trim()) return;
      input.input.value = "";
      cx.call<State>("add_reminder", { text: v }).then(take);
      input.input.focus();
    },
  });
  function draw() {
    const rows = rems.map((r) =>
      h("div", { class: `f-calendar-rem${r.done ? " done" : ""}` },
        h("button", { class: "f-calendar-check", role: "checkbox", "aria-checked": String(r.done), title: cx.t(r.done ? "Mark as not done" : "Mark as done"),
          onclick: () => cx.call<State>("toggle_reminder", { id: r.id }).then(take) }, r.done ? icon("check", 12) : ""),
        h("span", { class: "f-calendar-name" }, h("b", { text: r.text })),
        iconBtn("close", cx.t("Delete"), () => cx.call<State>("delete_reminder", { id: r.id }).then(take))));
    el.replaceChildren(input, h("div", { class: "f-calendar-list" }, ...(rows.length ? rows : [h("p", { class: "f-calendar-empty", text: cx.t("Nothing to remember.") })])));
  }
  return {
    el,
    show() {
      cx.call<State>("state").then(take);
      input.input.focus();
    },
    hide() {},
    sync() {},
  };
}

// ── Settings ─────────────────────────────────────────────────────────────────

function select(opts: [string, string][], value: string, on: (v: string) => void): HTMLElement {
  const sel = h("select") as HTMLSelectElement;
  for (const [v, label] of opts) sel.append(h("option", { value: v, text: label }));
  sel.value = value;
  sel.addEventListener("change", () => on(sel.value));
  return h("label", { class: "field" }, sel);
}

function settingsPage(cx: FeatureCtx) {
  const box = h("div", { class: "f-calendar-settings" });
  const calBox = card([]);
  const msg = h("span", { class: "f-calendar-note" });
  let name = "", url = "";
  const names: Record<number, string> = { 5: "5 minutes", 10: "10 minutes", 15: "15 minutes", 30: "30 minutes", 60: "1 hour", 120: "2 hours" };
  const opt = (ns: number[]): [string, string][] => ns.map((n) => [String(n), cx.t(names[n])]);

  function paint(s: State | null) {
    calBox.replaceChildren(
      ...(s?.calendars.length
        ? s.calendars.map((c) =>
            h("div", { class: "f-calendar-cal" },
              h("span", { class: "f-calendar-bar", style: `background:${c.color}` }),
              h("span", { class: "f-calendar-name" },
                h("b", { text: c.name }),
                h("small", { class: c.error ? "bad" : "",
                  text: c.error ?? (c.loaded ? `${tn("{n} event", "{n} events", c.events, { n: c.events })}${c.unsupported ? ` · ${tn("{n} with an unsupported rule", "{n} with an unsupported rule", c.unsupported, { n: c.unsupported })}` : ""}` : cx.t("Loading")) })),
              iconBtn("close", cx.t("Remove"), () => cx.call<State>("remove_calendar", { id: c.id }).then(paint))))
        : [h("p", { class: "f-calendar-empty", text: cx.t("No calendars yet.") })]),
    );
  }
  const load = () => cx.call<State>("state").then(paint);

  const add = async () => {
    msg.textContent = "";
    if (!url.trim()) return;
    try {
      const s = await cx.call<State>("add_calendar", { name, url });
      if (!s) throw new Error(cx.t("The address must start with https:// or webcal://"));
      name = url = "";
      nameF.input.value = urlF.input.value = "";
      paint(s);
      // the first fetch is asynchronous: look again shortly
      setTimeout(load, 2500);
    } catch (e) {
      msg.textContent = String((e as Error).message ?? e);
    }
  };
  const nameF = field({ placeholder: cx.t("Name (optional)"), onInput: (v) => (name = v) });
  const urlF = field({ placeholder: cx.t("https:// or webcal:// link to an ICS file"), onInput: (v) => (url = v), onEnter: add });

  cx.settings.get().then((s) => {
    box.prepend(card([
      row(cx.t("Time before a meeting"), select(opt([15, 30, 60, 120]), String(Math.min(240, Math.max(5, Number(s.lead) || 60))), (v) => cx.settings.patch({ lead: Number(v) })), cx.t("A meeting with a video link (Meet, Zoom, Teams, Webex) shows in the strip this long before it starts, with a Join button.")),
      row(cx.t("Alert before an event"), select([["0", cx.t("Off")], ...opt([5, 10, 15, 30])], String(Number(s.alarm) || 0), (v) => cx.settings.patch({ alarm: Number(v) })), cx.t("A notice shows this long before any timed event, with or without a video link.")),
      row(cx.t("Countdown"), toggle(s.countdown !== false, (on) => cx.settings.patch({ countdown: on })), cx.t("Shows the next event in the strip during the hour before it starts.")),
      row(cx.t("Event descriptions"), toggle(s.description !== false, (on) => cx.settings.patch({ description: on })), cx.t("Shows the text of an event under its title. Long text opens when you click it.")),
      row(cx.t("Cal.com bookings"), toggle(s.calcom !== false, (on) => cx.settings.patch({ calcom: on })), cx.t("Shows bookings from the Cal.com integration as events. Turn the integration on in its own page.")),
    ]));
  });
  box.append(
    calBox,
    h("div", { class: "f-calendar-add" }, nameF, urlF, btn(cx.t("Add calendar"), "primary", add), iconBtn("refresh", cx.t("Refresh now"), () => { cx.call("refresh"); setTimeout(load, 2500); })),
    msg,
    h("p", { class: "f-calendar-note", text: cx.t("The link is stored in Windows Credential Manager and is fetched only after you add it, then every 30 minutes while it matters. Reminders stay on this computer.") }),
  );
  load();
  const off = cx.on("changed", load);
  new MutationObserver(() => { if (!box.isConnected) off(); }).observe(document.body, { childList: true, subtree: true });
  return box;
}
