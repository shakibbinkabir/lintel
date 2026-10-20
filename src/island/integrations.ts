// Integration events → island state. Port of the `handle…` methods in the Swift
// pollers: a genuinely new item flips the pill to finished/error, badges it when
// the pill isn't focused, plays a sound, and clears itself after 60 s.

import { onEvent, Bridge, type IntegrationUpdate } from "../core/bridge";
import { ghLoginOn, pulseThroughGh } from "../core/chat-tools";
import { gitHubAlert, safeGitHubUrl, type GitHubEvent } from "../core/github";
import { t } from "../i18n/i18n";
import { availablePills } from "../core/pills";
import { Sound } from "../core/sound";
import { State } from "../core/state";
import type { Island } from "./island";

const GITHUB = "integration_github";

const clearTimers = new Map<string, number>();

export function registerIntegrationHandlers(island: Island) {
  void onEvent<IntegrationUpdate>("integration", (update) => handle(island, update));
  void onEvent<GitHubEvent[]>("github-alerts", handleGitHubAlerts);
  void onEvent<DevAlert>("dev-alert", (alert) => postDevAlert(island, alert));
  void refreshConfigured();
  State.subscribe(refreshGitHubWhenShown);
  State.subscribe(pulseWhenShown);
}

// ── Developer alerts (src-tauri/src/integrations.rs, alerts.rs) ───────────────
//
// One notice per event; Rust sends each key once. The arbiter decides when it
// shows: never over a card that waits for an answer.

interface DevAlert {
  kind: "ciFailed" | "review" | "changes" | "approved" | "deployReady" | "deployFailed";
  bad: boolean;
  subject: string;
  detail: string;
  url: string;
}

const DEV_TITLES: Record<DevAlert["kind"], () => string> = {
  ciFailed: () => t("CI failing"),
  review: () => t("Review requested"),
  changes: () => t("Changes requested"),
  approved: () => t("Pull request approved"),
  deployReady: () => t("Deploy ready"),
  deployFailed: () => t("Deploy failed"),
};

function postDevAlert(island: Island, alert: DevAlert) {
  const title = DEV_TITLES[alert.kind]?.();
  if (!title) return;
  const vercel = alert.kind.startsWith("deploy");
  // Only pages of the service itself open from a notice.
  const url = vercel
    ? /^https:\/\/[a-z0-9.-]+$/i.test(alert.url) ? alert.url : null
    : safeGitHubUrl(alert.url);
  island.api("integrations").notice({
    kind: "agent",
    title,
    detail: [alert.subject, alert.detail].filter(Boolean).join(" · "),
    icon: alert.bad ? "alert" : alert.kind === "review" || alert.kind === "changes" || alert.kind === "approved" ? "bell" : "checkc",
    tone: alert.bad ? "bad" : "ok",
    action: url ? { label: t("Open"), run: () => void Bridge.openUrl(url) } : undefined,
  });
}

// ── The local `gh` login ──────────────────────────────────────────────────────
//
// With "Use the gh login" on in Settings → Integrations → GitHub, the pollers
// ask GitHub with it when no token is saved. This one asks for the card's pull
// requests as it comes on screen, at most once a minute: Rust runs
// `gh auth token` for that one request and keeps nothing (github.rs).

const GH_FRESH_MS = 60_000;

let ghShown = false;
let ghBusy = false;
let ghAsked = 0;

async function pulseWithGhLogin() {
  if (ghBusy || Date.now() - ghAsked < GH_FRESH_MS) return;
  ghBusy = true;
  try {
    if (!(await ghLoginOn())) return;
    const info = State.integrations[GITHUB];
    try {
      const answer = await pulseThroughGh();
      // Null: a token is saved (its poller answers), or the app is paused.
      if (!answer) return;
      ghAsked = Date.now();
      State.integrations[GITHUB] = {
        data: { ...(info?.data ?? {}), pulse: answer.pulse }, error: null, loaded: true, configured: true,
      };
    } catch (err) {
      // What `gh` or GitHub said, on the card: "gh is not signed in…".
      State.integrations[GITHUB] = {
        data: info?.data ?? {}, error: String(err).replace(/^Error:\s*/, ""), loaded: info?.loaded ?? false, configured: true,
      };
    }
    State.notify();
  } finally {
    ghBusy = false;
  }
}

function pulseWhenShown() {
  const shown = State.mode === "expanded" && State.focusTask?.id === GITHUB;
  if (shown && !ghShown) void pulseWithGhLogin();
  ghShown = shown;
}

/**
 * AppState.handleGitHubEvents: the loudest event sets the badge — only while the
 * GitHub pill isn't the one on screen — and plays its sound. Unlike the other
 * integrations it leaves Mascot's state alone and the badge stays until the pill
 * is focused, as on macOS.
 */
export function handleGitHubAlerts(events: GitHubEvent[]) {
  if (State.paused) return;
  const alert = gitHubAlert(events);
  if (!alert) return;
  const task = State.tasks.find((t) => t.id === GITHUB);
  if (!task) return;
  if (State.focusId !== GITHUB) task.pillBadge = alert.badge;
  Sound.play(alert.sound);
  State.notify();
}

let gitHubShown = false;

/** The GitHub card just came on screen (focused, island opened): refresh it if stale. */
function refreshGitHubWhenShown() {
  const shown = State.mode === "expanded" && State.focusTask?.id === GITHUB;
  if (shown && !gitHubShown) void Bridge.githubRefresh("pulse", State.tasks.flatMap((x) => x.sessionCwd ?? []));
  gitHubShown = shown;
}

/**
 * Whether each pill is connected, so its idle card can say so: a key in the
 * credential store for a service or a chat provider, the hooks in place for a
 * pill fed by hook events (Mac #183), nothing at all for Claude Desktop.
 */
export async function refreshConfigured() {
  const hooks = (await Bridge.agentHooksStatus()) ?? null;
  for (const def of availablePills(State.os)) {
    let configured: boolean;
    switch (def.connect.kind) {
      case "key":
        configured = (await Bridge.secretPresent(def.connect.key)) ?? false;
        break;
      case "hooks":
        // Without an answer from Rust (a plain browser), the Claude Code pill
        // falls back to what the settings say about its hooks.
        configured = hooks?.[def.id] ??
          (def.id === "integration_claude" ? State.settings.hooksInstalled : false);
        break;
      case "server":
        // A local model server counts once the chat is connected to it.
        configured = State.settings[def.connect.field] !== "";
        break;
      case "none":
        configured = true;
        break;
    }
    const info = State.integrations[def.id] ?? { data: {}, error: null, loaded: false, configured: false };
    State.integrations[def.id] = { ...info, configured };
  }
  State.notify();
}

/** Only the hook-driven pills, for when the island opens: a few small file reads. */
export async function refreshHookPills() {
  const hooks = await Bridge.agentHooksStatus();
  if (!hooks) return;
  for (const [id, present] of Object.entries(hooks)) {
    const info = State.integrations[id] ?? { data: {}, error: null, loaded: false, configured: false };
    State.integrations[id] = { ...info, configured: present };
  }
  State.notify();
}

function handle(island: Island, update: IntegrationUpdate) {
  if (State.paused) return;

  const previous = State.integrations[update.id];
  State.integrations[update.id] = {
    data: update.error ? (previous?.data ?? {}) : update.data,
    error: update.error,
    loaded: update.error ? (previous?.loaded ?? false) : true,
    configured: previous?.configured ?? true,
  };

  const event = update.event;
  if (event) {
    const task = State.tasks.find((t) => t.id === update.id);
    if (task) {
      task.state = event.success ? "finished" : "error";
      task.steps = event.detail ? [event.label, event.detail] : [event.label];
      task.stepIndex = task.steps.length - 1;
      if (State.focusId !== update.id) {
        task.pillBadge = event.success ? "finished" : "error";
      }
      Sound.play(event.success ? "finish" : "error");
      // Same as the Swift pollers: show the compact island so the badge is seen,
      // but never steal the screen for a successful deploy.
      island.reveal();

      const existing = clearTimers.get(update.id);
      if (existing != null) window.clearTimeout(existing);
      clearTimers.set(
        update.id,
        window.setTimeout(() => {
          clearTimers.delete(update.id);
          const t = State.tasks.find((x) => x.id === update.id);
          if (!t || (t.state !== "finished" && t.state !== "error")) return;
          t.state = "idle";
          t.steps = [];
          t.stepIndex = 0;
          t.pillBadge = null;
          State.notify();
        }, 60_000),
      );
    }
  }

  State.notify();
}
