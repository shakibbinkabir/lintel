// Agent usage — the page side of the feature (docs/ARCHITECTURE.md §2.3).
// Rust reads the local logs and sends totals; this file draws the Agents section, the live-work
// activity for sessions the hooks do not report, and the "long turn ended" notice.

import "./style.css";
import { defineFeature, type FeatureCtx } from "../../core/feature";
import { State } from "../../core/state";
import { h } from "../../views/dom";
import { tn } from "../../i18n/i18n";
import { icon } from "../../ui/icons";
import { card, meter, stripRow, tab, toggle, row, field } from "../../ui/components";

// A small spark; the shared set has no agent glyph.
const SPARK = "M12 3l1.8 5.2L19 10l-5.2 1.8L12 17l-1.8-5.2L5 10l5.2-1.8zM18.5 15.5l.7 1.8 1.8.7-1.8.7-.7 1.8-.7-1.8-1.8-.7 1.8-.7z";
const IDLE = 600_000;

interface Item { name: string; tokens: number }
interface Period { input: number; output: number; cacheRead: number; cacheWrite: number; total: number; requests: number; models: Item[]; projects: Item[]; days: number[] }
interface Live { session: string; project: string; model: string; startedAt: number; last: number }
interface Prov { id: string; name: string; today: Period; week: Period; live: Live[] }
interface Usage { providers: Prov[] }

let usage: Usage | null = null;
let wasLive = "";
let idleTimer: number | undefined;
let liveOn = true;
/** Card ids the user switched off. */
let hidden: string[] = [];
/** The weekly readings kept for pacing, by card: (time ms, used percent). */
let paceData: Record<string, [number, number][]> = {};
const WEEK = 7 * 86_400_000;
const shown = () => (usage?.providers ?? []).filter((p) => !hidden.includes(p.id));

const tok = (n: number) => (n < 1e3 ? String(n) : n < 1e6 ? `${(n / 1e3).toFixed(n < 1e4 ? 1 : 0)}K` : n < 1e9 ? `${(n / 1e6).toFixed(n < 1e7 ? 1 : 0)}M` : `${(n / 1e9).toFixed(1)}B`);
const pad = (n: number) => String(n).padStart(2, "0");
const clock = (ms: number) => {
  const s = Math.max(0, Math.floor(ms / 1000));
  return s >= 3600 ? `${Math.floor(s / 3600)}:${pad(Math.floor(s / 60) % 60)}:${pad(s % 60)}` : `${Math.floor(s / 60)}:${pad(s % 60)}`;
};

/** Sessions the hooks already report are the built-in agents activity's; ours stays quiet for them. */
const hooked = (session: string) => State.tasks.some((t) => t.sessionId === session);
/** Open turns of sessions the hooks do not report. */
const mine = () =>
  shown().flatMap((p) => p.live.filter((l) => Date.now() - l.last < IDLE && !hooked(l.session)).map((l) => ({ ...l, provider: p.name })));

export default defineFeature({
  id: "agent-usage",
  voice: () => import("./voice"),

  activities: [
    {
      id: "live",
      order: 3.5, // its own slot, just after the agents the hooks report (3)
      tick: true,
      section: "agents",
      isActive: () => liveOn && mine().length > 0,
      render() {
        const all = mine().sort((a, b) => a.startedAt - b.startedAt);
        const l = all[0];
        return stripRow({
          icon: SPARK,
          tone: "info",
          title: all.length > 1 ? `${l.project} +${all.length - 1}` : l.project,
          detail: l.model,
          value: clock(Date.now() - l.startedAt),
        });
      },
      mark: () => h("span", { class: "isl-ico info" }, icon(SPARK, 14)),
    },
  ],

  sections: [{ id: "agents", title: "Agents", icon: SPARK, letter: "g", height: 300, build: (cx) => page(cx) }],

  settings(cx) {
    const box = card([]);
    cx.settings.get().then(async (s) => {
      // one switch per card: each agent, and each Claude Code profile folder found on this PC
      const off = new Set<string>(Array.isArray(s.hidden) ? (s.hidden as string[]) : []);
      const found = (await cx.call<Usage>("state"))?.providers ?? [];
      box.append(
        ...found.map((p) =>
          row(p.name, toggle(!off.has(p.id), (on) => {
            if (on) off.delete(p.id);
            else off.add(p.id);
            cx.settings.patch({ hidden: [...off] }).then(() => cx.call("poke"));
          }), p.id.startsWith("claude:") ? cx.t("A Claude Code profile folder. Switched off, it is not read.") : undefined)),
        row(cx.t("Tell me when a long turn ends"), toggle(s.finishNotice !== false, (on) => cx.settings.patch({ finishNotice: on })), cx.t("For turns the hooks do not report. Counts come from your local logs, which are only read, never changed.")),
        row(cx.t("Show running turns"), toggle(s.liveWork !== false, (on) => cx.settings.patch({ liveWork: on })), cx.t("Shows turns of sessions the hooks are not reporting.")),
        row(cx.t("Shortest turn to tell about (seconds)"), field({ value: String(s.minSeconds ?? 120), onEnter: (v) => cx.settings.patch({ minSeconds: Math.max(0, Math.min(3600, Number(v) || 120)) }) }), cx.t("Press Enter to apply.")),
      );
    });
    return box;
  },

  start(cx) {
    const apply = (s: Record<string, unknown>) => {
      liveOn = s.liveWork !== false;
      hidden = Array.isArray(s.hidden) ? (s.hidden as string[]) : [];
    };
    cx.settings.get().then((s) => (apply(s), State.notify()));
    const offS = cx.settings.onChange((s) => {
      apply(s);
      cx.island.activityChanged("live");
      State.notify();
    });
    // The weekly pacing keeps a reading each time the account's weekly gauge moves.
    const sampled: Record<string, number> = {};
    const samplePace = () => {
      for (const [id, u] of [["claude", State.planUsage], ["codex", State.codexPlanUsage]] as const) {
        const w = u?.sevenDay;
        if (!u || !w || w.resetsAt < Date.now() || sampled[id] === u.updatedAt) continue;
        sampled[id] = u.updatedAt;
        cx.call<[number, number][]>("sample", { id, pct: w.usedPct, resets: w.resetsAt, at: u.updatedAt }).then((p) => {
          if (p) {
            paceData[id] = p;
            State.notify();
          }
        });
      }
    };
    const refresh = () => {
      samplePace();
      const key = mine().map((l) => l.session).sort().join();
      if (key !== wasLive) {
        const grew = key !== "" && key.length >= wasLive.length;
        wasLive = key;
        if (grew) cx.island.activityChanged("live");
        else cx.island.redraw("live");
      }
      clearTimeout(idleTimer);
      const next = Math.min(...shown().flatMap((p) => p.live.map((l) => l.last + IDLE)));
      if (Number.isFinite(next)) idleTimer = window.setTimeout(refresh, Math.max(1000, next - Date.now() + 500));
    };
    const set = (u: Usage) => {
      usage = u;
      refresh();
      State.notify();
    };
    cx.call<Usage>("state").then((u) => u && set(u));
    const offs = [
      cx.on<Usage>("usage", set),
      State.subscribe(refresh),
      cx.on<{ provider: string; name: string; project: string; durationMs: number; session: string }>("finished", (e) => {
        // the built-in finished card covers sessions the hooks report
        if (hooked(e.session)) return;
        cx.island.notice({
          kind: "agent",
          title: cx.t("Agent finished"),
          detail: cx.t("{name} worked for {time}", { name: `${e.name} · ${e.project}`, time: clock(e.durationMs) }),
          icon: SPARK,
          tone: "ok",
          section: "agents",
        });
      }),
    ];
    return () => {
      offs.forEach((f) => f());
      offS();
      clearTimeout(idleTimer);
      usage = null;
      paceData = {};
      wasLive = "";
    };
  },
});

// ── Agents section ───────────────────────────────────────────────────────────

function page(cx: FeatureCtx) {
  const el = h("div", { class: "f-agent-usage-page" });
  const period: Record<string, "today" | "week"> = {};
  let drawn = "";

  const list = (items: Item[], total: number) =>
    items.slice(0, 4).map((i) =>
      h("div", { class: "f-agent-usage-li" }, h("span", { text: i.name }), meter(total ? i.tokens / total : 0), h("small", { text: tok(i.tokens) })));

  // the plan gauge already in the base: its stored reading, not a second computation
  const limits = (p: Prov) => {
    const u = p.id === "claude" ? State.planUsage : p.id === "codex" ? State.codexPlanUsage : null;
    if (!u || (!u.fiveHour && !u.sevenDay)) return null;
    const one = (label: string, w?: { usedPct: number }) =>
      w ? h("div", {}, h("span", { text: `${label} · ${cx.t("{n}% used", { n: Math.round(w.usedPct) })}` }), meter(w.usedPct / 100, w.usedPct >= 80 ? "warn" : undefined)) : "";
    return h("div", {}, h("div", { class: "f-agent-usage-limits" }, one(cx.t("5 h limit"), u.fiveHour), one(cx.t("Weekly limit"), u.sevenDay)), pacing(p.id, u.sevenDay) ?? "");
  };

  // Weekly pacing: what the gauge read through the week against an even spend, and what is left per day.
  const pacing = (id: string, w?: { usedPct: number; resetsAt: number }) => {
    const pts = paceData[id];
    const now = Date.now();
    if (!w || !pts?.length || w.resetsAt < now) return null;
    const start = w.resetsAt - WEEK;
    const x = (t: number) => (Math.max(0, Math.min(1, (t - start) / WEEK)) * 100).toFixed(1);
    const y = (v: number) => (38 - (Math.min(100, v) / 100) * 36).toFixed(1);
    const line = (cls: string, d: string) => {
      const el = document.createElementNS("http://www.w3.org/2000/svg", "polyline");
      el.setAttribute("class", cls);
      el.setAttribute("points", d);
      return el;
    };
    const chart = document.createElementNS("http://www.w3.org/2000/svg", "svg");
    chart.setAttribute("viewBox", "0 0 100 40");
    chart.setAttribute("preserveAspectRatio", "none");
    chart.setAttribute("class", "f-agent-usage-pace");
    chart.append(
      line("ideal", `0,${y(0)} 100,${y(100)}`),
      line("real", [...pts.filter((q) => q[0] >= start).map((q) => `${x(q[0])},${y(q[1])}`), `${x(now)},${y(w.usedPct)}`].join(" ")),
    );
    const daysLeft = Math.max((w.resetsAt - now) / 86_400_000, 0.04);
    const ideal = ((now - start) / WEEK) * 100;
    const over = Math.round(w.usedPct - ideal);
    return h("div", { class: "f-agent-usage-pacing" },
      chart,
      h("small", {
        text: `${cx.t("{n}% a day until the reset", { n: Math.max(0, (100 - w.usedPct) / daysLeft).toFixed(1) })} · ${
          Math.abs(over) < 2 ? cx.t("on an even pace") : over > 0 ? cx.t("{n} points over an even pace", { n: over }) : cx.t("{n} points under an even pace", { n: -over })}`,
      }));
  };

  const provCard = (p: Prov) => {
    const per = period[p.id] ?? "today";
    const d = p[per];
    const tabs = (["today", "week"] as const).map((k) => {
      const b = tab("", cx.t(k === "today" ? "Today" : "This week"), () => {
        period[p.id] = k;
        drawn = "";
        sync();
      });
      b.classList.toggle("on", per === k);
      return b;
    });
    const max = Math.max(1, ...p.week.days);
    return card(
      h("div", { class: "f-agent-usage-card" },
        h("div", { class: "f-agent-usage-head" }, h("b", { text: p.name }), h("small", { text: tn("{n} request", "{n} requests", d.requests, { n: d.requests }) }), h("div", { class: "f-agent-usage-tabs" }, ...tabs)),
        limits(p) ?? "",
        h("div", { class: "f-agent-usage-big" },
          h("strong", { text: tok(d.total) }), h("span", { text: cx.t("tokens") })),
        h("div", { class: "f-agent-usage-kinds" },
          ...([["Input", d.input], ["Output", d.output], ["Cache read", d.cacheRead], ["Cache write", d.cacheWrite]] as const).map(([k, v]) =>
            h("div", {}, h("span", { text: cx.t(k) }), h("b", { text: tok(v) })))),
        per === "week"
          ? h("div", { class: "f-agent-usage-days" }, ...p.week.days.map((v, i) => h("i", { class: i === 6 ? "now" : "", style: `height:${Math.max(6, (v / max) * 100)}%` })))
          : "",
        h("div", { class: "f-agent-usage-cols" },
          h("div", {}, h("h4", { text: cx.t("Models") }), ...list(d.models, d.total)),
          h("div", {}, h("h4", { text: cx.t("Projects") }), ...list(d.projects, d.total))),
      ),
    );
  };

  function draw() {
    if (!usage) return void el.replaceChildren(h("p", { class: "f-agent-usage-empty", text: cx.t("Reading your logs...") }));
    const list = shown();
    if (!list.length) return void el.replaceChildren(h("p", { class: "f-agent-usage-empty", text: cx.t("No Claude Code or Codex logs found on this PC yet.") }));
    el.replaceChildren(...list.map(provCard));
  }
  function sync() {
    const key = JSON.stringify([usage, State.planUsage, State.codexPlanUsage, paceData, hidden]) + JSON.stringify(period);
    if (key === drawn) return;
    drawn = key;
    const top = el.scrollTop;
    draw();
    el.scrollTop = top;
  }
  return { el, show: sync, hide() {}, sync };
}
