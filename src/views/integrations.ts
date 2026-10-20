// Integration cards shown in the overview's left card — DOM ports of
// IntegrationCardView and friends from IslandViewContent.swift.
//
// Cal.com is the one simplification: macOS shows a three-level calendar
// (month → day → booking); here it is the list of upcoming bookings.

import { h, svg, clear, dot } from "./dom";
import { ICONS } from "../ui/icons";
import { State, type AgentTask } from "../core/state";
import { Bridge } from "../core/bridge";
import { isComingSoon, pillDefinition } from "../core/pills";
import { refreshHookPills } from "../island/integrations";
import { GH_STRINGS, mainCISummary, readActivity, readPulse, readStats } from "../core/github";
import { githubDetail, githubPulseCard } from "./github";
import { N_, language, t, tn } from "../i18n/i18n";
import type { Tone } from "../ui/components";

/** Same shape as the Swift `timeAgo` computed properties. */
export function timeAgo(value: unknown): string {
  const date = typeof value === "number" ? new Date(value) : new Date(String(value));
  const diff = (Date.now() - date.getTime()) / 1000;
  if (!Number.isFinite(diff)) return "";
  if (diff < 60) return t("just now");
  if (diff < 3600) return t("{n}m", { n: Math.floor(diff / 60) });
  if (diff < 86400) return t("{n}h", { n: Math.floor(diff / 3600) });
  return t("{n}d", { n: Math.floor(diff / 86400) });
}

/** A service in one line, under its pill's name: the first thing its card says, and the tone to say it in. */
export interface Line {
  text: string;
  tone?: Tone;
}

/** "2h ago", or "just now". */
function ago(value: unknown): string {
  const time = timeAgo(value);
  return time && time !== t("just now") ? t("{time} ago", { time }) : time;
}

const both = (...parts: (string | null | undefined)[]) => parts.filter(Boolean).join(" · ");

function header(color: string, name: string, kind: string, extra?: Node): HTMLElement {
  const row = h("div", { class: "int-head" }, dot(color, 7), h("b", { text: name }), h("span", { text: kind }));
  if (extra) row.append(extra);
  return row;
}

/** Highlighted first row + plain rows, the layout every list card shares. */
function listRow(accent: string, first: boolean, ...children: Node[]): HTMLElement {
  const row = h("div", { class: first ? "int-row first" : "int-row" }, dot(accent, 5), ...children);
  if (first) row.style.background = `${accent}14`;
  return row;
}

function get(id: string): Record<string, unknown> {
  return (State.integrations[id]?.data ?? {}) as Record<string, unknown>;
}

function arr(id: string, key: string): Record<string, unknown>[] {
  const v = get(id)[key];
  return Array.isArray(v) ? (v as Record<string, unknown>[]) : [];
}

// ── Not configured / idle ─────────────────────────────────────────────────────

const OPEN_URLS: Record<string, string> = {
  integration_resend: "https://resend.com/emails",
  integration_vercel: "https://vercel.com/dashboard",
  integration_github: "https://github.com",
  integration_stripe: "https://dashboard.stripe.com/payments",
  integration_notion: "https://notion.so",
  integration_calcom: "https://app.cal.com/bookings",
  integration_linear: "https://linear.app/inbox",
};

/** IntegrationCardView.statusLabel on macOS. */
export function idleStatus(
  id: string,
  info: { configured: boolean; error: string | null } | undefined,
  chatModel: string,
): { label: string; color: string; ok: boolean } {
  if (isComingSoon(id)) return { label: t("Coming soon"), color: "var(--dim-2)", ok: true };
  if (info?.error) return { label: info.error, color: "var(--bad)", ok: false };
  const configured = info?.configured ?? false;
  const def = pillDefinition(id);
  const ok = (label: string) => ({ label, color: "var(--ok)", ok: true });
  const missing = (label: string) => ({ label, color: "var(--bad)", ok: false });
  // Pills driven by hooks never have a key: they are connected once the hooks
  // are in place (Mac #183). A session replaces this card; nothing is loading.
  if (def?.connect.kind === "hooks") return configured ? ok(t("Hooks installed")) : missing(t("Hooks not installed"));
  if (def?.connect.kind === "none") return ok(t("Ready · no setup needed"));
  if (def?.connect.kind === "server") return configured ? ok(t("Connected")) : missing(t("Not connected"));
  if (def?.category === "ai") {
    if (!configured) return missing(t("Key not configured"));
    return ok(id === "ai_anthropic" ? t("Key configured · {model}", { model: chatModel }) : t("Key configured"));
  }
  return configured ? ok(t("Connected · loading…")) : missing(t("Key not configured"));
}

/**
 * What an idle pill offers: open its app or site, look again, or go and set it
 * up. `short` is for under the pill's own name, where "Open" says enough.
 */
export function idleActions(task: AgentTask, openSettings: () => void, short = false): HTMLElement {
  const info = State.integrations[task.id];
  const configured = info?.configured ?? false;
  const def = pillDefinition(task.id);

  const actions = h("div", { class: "int-actions" });
  if (task.id === "integration_claude") {
    actions.append(
      h("button", {
        class: "link-btn",
        style: `color:${task.color}b3`,
        text: short ? t("Open") : t("Open Visual Studio Code"),
        onclick: () => void Bridge.openInVSCode(task.sessionCwd ?? null),
      }),
    );
  } else if (task.id === "agent_claude-desktop") {
    actions.append(
      h("button", {
        class: "link-btn",
        style: `color:${task.color}d9`,
        text: short ? t("Open") : t("Open Claude"),
        onclick: () => void Bridge.openSession(null, null, "claude-desktop"),
      }),
    );
  } else if (task.id === "integration_n8n") {
    actions.append(
      h("button", {
        class: "link-btn",
        style: `color:${task.color}d9`,
        text: short ? t("Open") : t("Open {name}", { name: "n8n" }),
        onclick: () => void Bridge.openN8n(),
      }),
    );
  } else if (OPEN_URLS[task.id]) {
    actions.append(
      h("button", {
        class: "link-btn",
        style: `color:${task.color}d9`,
        text: short ? t("Open") : t("Open {name}", { name: task.name }),
        onclick: () => void Bridge.openUrl(OPEN_URLS[task.id]),
      }),
    );
  }
  const hookPill = def?.connect.kind === "hooks";
  if (isComingSoon(task.id) || def?.connect.kind === "none") {
    // Nothing to set up, and nothing to refresh.
  } else if (configured && (hookPill || def?.category !== "ai")) {
    actions.append(
      h("button", {
        class: "link-btn",
        style: `color:${task.color}d9`,
        text: t("Refresh"),
        // A hook pill has nothing to poll: look at its hooks again instead.
        onclick: () => void (hookPill ? refreshHookPills() : Bridge.refreshIntegration(task.id)),
      }),
    );
  } else if (!configured) {
    actions.append(
      h("button", { class: "link-btn", text: t("Settings…"), onclick: openSettings }),
    );
  }
  return actions;
}

function idleCard(task: AgentTask, openSettings: () => void): HTMLElement {
  const def = pillDefinition(task.id);
  const status = idleStatus(task.id, State.integrations[task.id], State.settings.model);
  const actions = idleActions(task, openSettings);

  return h(
    "div",
    { class: "int-card" },
    header(
      task.color,
      task.id === "integration_claude" ? "VS Code" : task.name,
      t(def?.subtitle ?? N_("Integration")),
    ),
    h("div", { class: "int-status" }, dot(status.color, 5), h("span", { text: status.label })),
    actions,
  );
}

// ── Vercel ────────────────────────────────────────────────────────────────────

/** A deployment's state as the card names and colours it. */
function vercelState(d: Record<string, unknown>): { text: string; ok: boolean } {
  const ok = d.state === "READY";
  return { ok, text: ok ? t("Ready") : d.state === "CANCELED" ? t("Canceled") : t("Error") };
}

/** The latest deployment: its state and its age. */
function vercelLine(): Line {
  const d = arr("integration_vercel", "deployments")[0] ?? {};
  const state = vercelState(d);
  return { text: both(state.text, ago(d.createdAt)), tone: state.ok ? "ok" : "bad" };
}

function vercelCard(onDetail: () => void): HTMLElement {
  const deployments = arr("integration_vercel", "deployments");
  const rows = h("div", { class: "int-rows" });
  deployments.slice(0, 3).forEach((d, i) => {
    const accent = d.state === "READY" ? "#22C55E" : "#F4505E";
    const name = h("span", { class: "int-name", text: String(d.projectName ?? "") });
    const ago = h("span", { class: "int-ago", text: timeAgo(d.createdAt) });
    if (i === 0) {
      const more = h(
        "button",
        { class: "int-more", title: t("Details"), onclick: onDetail },
        svg(ICONS.ellipsis, 8),
      );
      rows.append(listRow(accent, true, name, ago, more));
    } else {
      rows.append(listRow(accent, false, name, ago));
    }
  });
  return h("div", { class: "int-card" }, header("#7C5CFF", "Vercel", t("Deployments")), rows);
}

function vercelDetail(onBack: () => void): HTMLElement {
  const d = arr("integration_vercel", "deployments")[0] ?? {};
  const { ok: success, text: status } = vercelState(d);
  const accent = success ? "#22C55E" : "#F4505E";
  const body = h("div", { class: "int-detail-body" });
  if (d.commitMessage) body.append(h("div", { class: "int-commit", text: String(d.commitMessage) }));
  const meta = h("div", { class: "int-meta" });
  if (d.branch) meta.append(h("span", { text: String(d.branch) }));
  meta.append(h("span", { text: t("{time} ago", { time: timeAgo(d.createdAt) }) }));
  body.append(meta);
  if (d.url) {
    body.append(
      h("button", {
        class: "int-link",
        text: String(d.url),
        onclick: () => void Bridge.openUrl(`https://${d.url}`),
      }),
    );
  }
  return h(
    "div",
    { class: "int-card detail" },
    h(
      "div",
      { class: "int-detail-head" },
      h("button", { class: "int-back", onclick: onBack }, svg(ICONS.chevronLeft, 10, { stroke: 2.4 })),
      dot(accent, 6),
      h("b", { text: String(d.projectName ?? t("Deployment")) }),
      h("span", { class: "int-badge", style: `color:${accent};background:${accent}24`, text: status }),
    ),
    body,
  );
}

// ── Resend ────────────────────────────────────────────────────────────────────

/** The latest email: its subject (or who it went to) and its age, red like its dot when it was not delivered. */
function resendLine(): Line {
  const e = arr("integration_resend", "emails")[0] ?? {};
  const to = Array.isArray(e.to) ? String(e.to[0] ?? "").split("@")[0] : "";
  return { text: both(String(e.subject ?? "") || to, ago(e.createdAt)), tone: e.lastEvent === "delivered" ? "ok" : "bad" };
}

function resendCard(): HTMLElement {
  const emails = arr("integration_resend", "emails");
  const total = get("integration_resend").total;
  const extra =
    total != null
      ? h("span", { class: "int-total" }, h("i", { class: "pulse" }), h("span", { text: String(total) }))
      : undefined;
  const rows = h("div", { class: "int-rows" });
  emails.slice(0, 3).forEach((e, i) => {
    const delivered = e.lastEvent === "delivered";
    const accent = delivered ? "#22C55E" : "#F4505E";
    const to = Array.isArray(e.to) ? String(e.to[0] ?? "?") : "?";
    const short = to.split("@")[0];
    const cells: Node[] = [
      h("span", { class: "int-name", text: short }),
      h("span", { class: "int-ago", text: timeAgo(e.createdAt) }),
    ];
    if (i === 0 && e.subject) cells.push(h("span", { class: "int-sub", text: String(e.subject) }));
    rows.append(listRow(accent, i === 0, ...cells));
  });
  return h("div", { class: "int-card" }, header("#22C55E", "Resend", t("Emails"), extra), rows);
}

// ── GitHub ────────────────────────────────────────────────────────────────────

function statRow(icon: string, color: string, label: string, value: string): HTMLElement {
  return h(
    "div",
    { class: "int-stat" },
    h("i", { class: "int-stat-icon", style: `color:${color}` }, svg(icon, 10)),
    h("span", { class: "int-stat-label", text: label }),
    h("span", { class: "int-stat-value", text: value }),
  );
}

/**
 * What matters first on GitHub: a default branch that fails, then what waits
 * for a review, then the open pull requests; else the calm line of its card.
 */
function githubLine(): Line {
  const d = get("integration_github");
  const pulse = readPulse(d);
  if (pulse) {
    const failing = pulse.mainCI.filter((r) => r.ci === "failure").length;
    if (failing > 0) return { text: tn("{count} failing on the default branch", "{count} failing on the default branch", failing), tone: "bad" };
    const reviews = pulse.toReview.length;
    if (reviews > 0) return { text: tn("{count} to review", "{count} to review", reviews), tone: "warn" };
    const mine = pulse.myPRs.length;
    if (mine > 0) return { text: tn("{count} open pull request", "{count} open pull requests", mine) };
    const main = mainCISummary(pulse.mainCI).value;
    if (pulse.mainCI.length > 0) return { text: main, tone: main === GH_STRINGS.allGreen ? "ok" : undefined };
  }
  const stats = readStats(d);
  return stats ? { text: GH_STRINGS.repos(stats.totalRepos) } : { text: t("Connected"), tone: "ok" };
}

function githubCard(): HTMLElement {
  const d = get("integration_github");
  const stars = Number(d.totalStars ?? 0);
  const repos = Number(d.totalRepos ?? 0);
  const fmt = (n: number) => (n >= 1000 ? `${(n / 1000).toFixed(1)}k` : String(n));
  return h(
    "div",
    { class: "int-card" },
    header("#F4505E", "GitHub", t("Overview")),
    h(
      "div",
      { class: "int-stats" },
      statRow(ICONS.star, "#F5A524", t("Total stars"), fmt(stars)),
      statRow(ICONS.stack, "#6B7079", t("Repositories"), String(repos)),
    ),
  );
}

// ── Stripe ────────────────────────────────────────────────────────────────────

/** The balance as the card writes it: the figure, then the currency. */
function stripeBalance(): [string, string] {
  const d = get("integration_stripe");
  return [(Number(d.balance ?? 0) / 100).toFixed(2), String(d.currency ?? "eur").toUpperCase()];
}

/** The balance; red when the latest payment did not go through, like its dot. */
function stripeLine(): Line {
  const last = arr("integration_stripe", "payments")[0];
  return { text: stripeBalance().join(" "), tone: last && last.status !== "succeeded" ? "bad" : undefined };
}

function stripeCard(): HTMLElement {
  const [balance, currency] = stripeBalance();
  const rows = h("div", { class: "int-rows tight" });
  for (const p of arr("integration_stripe", "payments")) {
    const success = p.status === "succeeded";
    const accent = success ? "#22C55E" : "#F4505E";
    rows.append(
      h(
        "div",
        { class: "int-row" },
        dot(accent, 5),
        h("span", { class: "int-name", text: String(p.description ?? t("Payment")) }),
        h("span", {
          class: "int-amount",
          style: "color:#22c55e",
          text: `+${(Number(p.amount ?? 0) / 100).toFixed(2)}`,
        }),
        h("span", { class: "int-ago", text: timeAgo(p.createdAt) }),
      ),
    );
  }
  return h(
    "div",
    { class: "int-card" },
    header("#0570DE", "Stripe", t("Payments")),
    h("div", { class: "int-balance" }, h("span", { text: balance }), h("i", { text: currency })),
    rows,
  );
}

// ── Notion ────────────────────────────────────────────────────────────────────

/** The page edited last, and when. */
function notionLine(): Line {
  const p = arr("integration_notion", "pages")[0] ?? {};
  return { text: both(String(p.title ?? t("Untitled")), ago(p.lastEditedAt)) };
}

function notionCard(): HTMLElement {
  const rows = h("div", { class: "int-rows tight" });
  for (const p of arr("integration_notion", "pages").slice(0, 3)) {
    rows.append(
      h(
        "button",
        {
          class: "int-page",
          onclick: () => {
            if (typeof p.url === "string") void Bridge.openUrl(p.url);
          },
        },
        p.emoji
          ? h("span", { class: "int-emoji", text: String(p.emoji) })
          : h("i", { class: "int-emoji" }, svg(ICONS.doc, 9)),
        h("span", { class: "int-name", text: String(p.title ?? t("Untitled")) }),
        h("span", { class: "int-ago", text: timeAgo(p.lastEditedAt) }),
      ),
    );
  }
  return h("div", { class: "int-card" }, header("#E8E8E8", "Notion", t("Recent")), rows);
}

// ── Linear ────────────────────────────────────────────────────────────────────

function linearLine(): Line {
  const n = arr("integration_linear", "unread").length;
  return { text: n > 0 ? t("{0} unread", { 0: n }) : t("Inbox empty") };
}

/** The unread inbox: a row opens the issue. */
function linearCard(): HTMLElement {
  const unread = arr("integration_linear", "unread");
  const rows = h("div", { class: "int-rows tight" });
  for (const n of unread.slice(0, 4)) {
    rows.append(
      h(
        "button",
        { class: "int-page", onclick: () => typeof n.url === "string" && void Bridge.openUrl(n.url) },
        h("span", { class: "int-name", text: `${n.identifier ?? ""} ${n.title ?? ""}`.trim() }),
        h("span", { class: "int-ago", text: timeAgo(n.createdAt) }),
      ),
    );
  }
  if (unread.length === 0) rows.append(h("span", { class: "int-ago", text: t("Inbox empty") }));
  return h("div", { class: "int-card" }, header("#5E6AD2", "Linear", unread.length > 0 ? t("{0} unread", { 0: unread.length }) : t("Inbox")), rows);
}

// ── Cal.com ───────────────────────────────────────────────────────────────────

/** The bookings, the next one first. */
function calcomBookings(): Record<string, unknown>[] {
  return arr("integration_calcom", "bookings")
    .slice()
    .sort((a, b) => new Date(String(a.start)).getTime() - new Date(String(b.start)).getTime());
}

/** "24/10 09:30", as the card's rows write a booking's time. */
function calcomWhen(b: Record<string, unknown>): string {
  const when = new Date(String(b.start));
  const day = when.toLocaleDateString(language(), { day: "2-digit", month: "2-digit" });
  const time = when.toLocaleTimeString(language(), { hour: "2-digit", minute: "2-digit" });
  return `${day} ${time}`;
}

/** The next call, or that none is scheduled. */
function calcomLine(): Line {
  const next = calcomBookings()[0];
  return { text: next ? both(calcomWhen(next), String(next.title ?? t("Meeting"))) : t("No calls scheduled") };
}

function calcomCard(): HTMLElement {
  const bookings = calcomBookings();
  const rows = h("div", { class: "int-rows tight" });
  if (bookings.length === 0) {
    rows.append(h("div", { class: "int-empty", text: t("No calls scheduled") }));
  }
  for (const b of bookings.slice(0, 3)) {
    rows.append(
      h(
        "div",
        { class: "int-row" },
        dot("#C9956A", 4),
        h("span", { class: "int-time", text: calcomWhen(b) }),
        h("span", { class: "int-name", text: String(b.title ?? t("Meeting")) }),
      ),
    );
  }
  return h("div", { class: "int-card" }, header("#C9956A", "Cal.com", t("Schedule")), rows);
}

// ── n8n ───────────────────────────────────────────────────────────────────────

function n8nCard(task: AgentTask, onDetail: () => void, openSettings: () => void): HTMLElement {
  const hasActivity = task.steps.length > 0 && (task.state === "finished" || task.state === "error");
  if (!hasActivity) return idleCard(task, openSettings);
  const success = task.state === "finished";
  const accent = success ? "#22C55E" : "#F4505E";
  return h(
    "div",
    { class: "int-card" },
    header("#F29B38", "n8n", t("Workflow")),
    h(
      "div",
      { class: "int-actions" },
      h(
        "button",
        {
          class: "int-pill",
          style: `background:${accent}1a;border-color:${accent}38`,
          onclick: onDetail,
        },
        dot(accent, 5),
        h("span", { class: "int-name", text: task.steps[0] ?? t("Workflow") }),
        svg(ICONS.ellipsis, 8),
      ),
    ),
  );
}

function n8nDetail(task: AgentTask, onBack: () => void): HTMLElement {
  const success = task.state === "finished";
  const accent = success ? "#22C55E" : "#F4505E";
  const detail = task.steps[1];
  return h(
    "div",
    { class: "int-card detail" },
    h(
      "div",
      { class: "int-detail-head" },
      h("button", { class: "int-back", onclick: onBack }, svg(ICONS.chevronLeft, 10, { stroke: 2.4 })),
      dot(accent, 6),
      h("b", { text: task.steps[0] ?? t("Workflow") }),
      h("span", {
        class: "int-badge",
        style: `color:${accent};background:${accent}24`,
        text: success ? t("Success") : t("Failed"),
      }),
    ),
    detail
      ? h("pre", { class: "int-detail-text", text: detail })
      : h("div", {
          class: "int-status",
          text: success ? t("Completed successfully.") : t("No error details available."),
        }),
  );
}

// ── Dispatch ──────────────────────────────────────────────────────────────────

export interface IntegrationCardHooks {
  detailOpen: boolean;
  openDetail(): void;
  closeDetail(): void;
  openSettings(): void;
}

/** True when this integration has data worth showing instead of the idle card. */
export function hasIntegrationData(id: string): boolean {
  const info = State.integrations[id];
  if (!info || info.error) return false;
  switch (id) {
    case "integration_vercel":
      return arr(id, "deployments").length > 0;
    case "integration_resend":
      return arr(id, "emails").length > 0;
    case "integration_github":
      return get(id).totalRepos != null || readPulse(get(id)) != null;
    case "integration_stripe":
      return info.loaded;
    case "integration_notion":
      return arr(id, "pages").length > 0;
    case "integration_calcom":
      return info.loaded;
    case "integration_linear":
      return info.loaded;
    default:
      return false;
  }
}

/** Each service's line, written beside its card from the same data. */
const LINES: Record<string, () => Line> = {
  integration_vercel: vercelLine,
  integration_resend: resendLine,
  integration_github: githubLine,
  integration_stripe: stripeLine,
  integration_notion: notionLine,
  integration_calcom: calcomLine,
  integration_linear: linearLine,
};

/**
 * What a service's pill says under its name when nothing more urgent is going
 * on: the latest of its card. A service that is not set up, or whose last
 * fetch failed, says that (as its idle card does); one that is set up and has
 * nothing yet says "Connected". Null for a pill that is not a service's.
 */
export function serviceLine(id: string): Line | null {
  if (pillDefinition(id)?.category !== "service") return null;
  const info = State.integrations[id];
  if (!info?.configured || info.error) {
    const status = idleStatus(id, info, State.settings.model);
    return { text: status.label, tone: status.ok ? undefined : "bad" };
  }
  return (hasIntegrationData(id) ? LINES[id]?.() : null) ?? { text: t("Connected"), tone: "ok" };
}

/**
 * The small line under the focused pill's line: project, kind, step count. A
 * service's kind is never all of it, and never what the line above already
 * says; a tool's or an agent's reads as it always did.
 */
export function metaLine(session: boolean, line: string, project: string | undefined, kind: string, count: string): string {
  const parts = [project, session || kind !== line ? kind : "", count].filter(Boolean);
  return !session && parts.length === 1 && parts[0] === kind ? "" : parts.join(" · ");
}

export function renderIntegrationCard(task: AgentTask, hooks: IntegrationCardHooks): HTMLElement {
  if (task.id === "integration_n8n") {
    const hasActivity = task.steps.length > 0 && (task.state === "finished" || task.state === "error");
    return hooks.detailOpen && hasActivity
      ? n8nDetail(task, hooks.closeDetail)
      : n8nCard(task, hooks.openDetail, hooks.openSettings);
  }
  if (task.id === "integration_vercel" && hasIntegrationData(task.id)) {
    return hooks.detailOpen ? vercelDetail(hooks.closeDetail) : vercelCard(hooks.openDetail);
  }
  if (!hasIntegrationData(task.id)) return idleCard(task, hooks.openSettings);

  // With the pulse in, GitHub gets the Mac's richer card and its lists.
  if (task.id === "integration_github") {
    const d = get(task.id);
    const pulse = readPulse(d);
    if (pulse) {
      return hooks.detailOpen
        ? githubDetail(pulse, readStats(d), readActivity(d), hooks.closeDetail)
        : githubPulseCard(pulse, readStats(d), readActivity(d), hooks.openDetail);
    }
  }

  switch (task.id) {
    case "integration_resend":
      return resendCard();
    case "integration_github":
      return githubCard();
    case "integration_stripe":
      return stripeCard();
    case "integration_notion":
      return notionCard();
    case "integration_calcom":
      return calcomCard();
    case "integration_linear":
      return linearCard();
    default:
      return idleCard(task, hooks.openSettings);
  }
}

export { clear };
