// Thin wrapper over the Tauri commands/events. Every call is a no-op when the
// page is opened in a plain browser, so the island can be iterated on with
// `npm run dev` alone.

import { BRAND } from "./brand";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { Settings } from "./state";
import type { RecapHistory, RecapPrefs } from "../recap/summary";

export const IS_TAURI =
  typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

async function call<T>(cmd: string, args?: Record<string, unknown> | Uint8Array): Promise<T | null> {
  if (!IS_TAURI) return null;
  try {
    return await invoke<T>(cmd, args);
  } catch (err) {
    console.error(`[lintel] ${cmd} failed`, err);
    return null;
  }
}

export interface BootInfo {
  settings: Settings;
  /** Logical screen rect of the monitor the island lives on. */
  screen: { x: number; y: number; width: number; height: number; scale: number };
  version: string;
  hookPath: string;
}

/** A click on a permission card; a plan card (Claude Code's ExitPlanMode) has its own three. */
export type Decision = "allow" | "deny" | "plan_auto" | "plan_manual" | "plan_change";

export const Bridge = {
  boot: () => call<BootInfo>("boot"),
  /** The system's languages as the webview sees them, for Rust's own texts (i18n.rs). */
  setSystemLanguages: (languages: string[]) => call<void>("set_system_languages", { languages }),

  saveSettings: (settings: Settings) => call<void>("save_settings", { settings }),

  /** Shrink the window down to the invisible wake strip (hidden) or back to full. */
  setCollapsed: (collapsed: boolean) => call<void>("set_collapsed", { collapsed }),

  /**
   * Pushes the island shape in window coordinates. Rust flips click-through from
   * its own cursor poll, so the flag is never a frame behind a click.
   */
  setIslandRect: (x: number, y: number, width: number, height: number) =>
    call<void>("set_island_rect", { x, y, width, height }),

  /**
   * The island takes the keyboard (true), or hands it back to the window it
   * took it from (false). Resolves to whether the island holds it now.
   */
  focusWindow: (focused: boolean) => call<boolean>("focus_window", { focused }),

  /**
   * The key combinations that are the island's own while the pointer is over it
   * and it does not hold the keyboard, as numbers (src/island/keys.ts). Rust
   * swallows exactly these and sends each back as an `island-key` event.
   */
  setIslandKeys: (keys: number[]) => call<void>("set_island_keys", { keys }),
  /** A press outside the island is reported (`outside-press`) while this is on: the mouse hook exists only then. */
  setOutsideWatch: (on: boolean) => call<void>("set_outside_watch", { on }),
  /** The strip rests (it stays at peek, the pointer is away, nothing moves): the cursor poll goes slow until the pointer is back. */
  setPollRest: (on: boolean) => call<void>("set_poll_rest", { on }),
  /** Runs a running feature's shortcut action as if its key had been pressed (the island's Utilities page). */
  featureAction: (feature: string, id: string) => call<void>("feature_action", { feature, id }),

  reposition: () => call<void>("reposition"),
  fitZoom: (dpr: number) => call<boolean>("fit_zoom", { dpr }),

  /** Every display, for the placement rules of a dragged island (src/island/place.ts). */
  islandDisplays: () => call<import("../island/place").Display[]>("island_displays"),
  /** Where the island's window hangs: the top-centre point in screen pixels, or null for the top centre of the chosen display. */
  islandPlace: (place: { x: number; y: number; floating: boolean } | null) => call<void>("island_place", { place }),
  /** A press became a drag: Rust carries the window until the button is let go, then says `island-drop`. */
  islandDrag: (lift: number) => call<void>("island_drag", { lift }),

  /** Displays the island can be pinned to: `key` is what `settings.screen` stores. */
  listMonitors: () => call<{ key: string; label: string }[]>("list_monitors"),

  openUrl: (url: string) => call<void>("open_url", { url }),

  /** Which of these sounds have a file of the user's own in the sounds folder. */
  customSounds: (names: readonly string[]) => call<string[]>("custom_sounds", { names }),
  /** The user's file for one sound, as raw bytes. */
  customSound: (name: string) => call<ArrayBuffer>("custom_sound", { name }),
  /** Your own file for one sound (raw bytes), replacing the old one. */
  setCustomSound: (name: string, ext: string, bytes: Uint8Array) =>
    IS_TAURI ? invoke<void>("set_custom_sound", bytes, { headers: { name, ext } }).then(() => true, () => false) : Promise.resolve(false),
  resetCustomSound: (name: string) => call<void>("reset_custom_sound", { name }),
  revealSoundsFolder: () => call<void>("reveal_sounds_folder"),
  /** Tells the island to read the sounds folder again ("sounds-changed"). */
  reloadSounds: () => call<void>("reload_sounds"),

  /** "Open terminal" → opens the folder in VS Code when `code` is on PATH. */
  openInVSCode: (path: string | null) => call<boolean>("open_in_vscode", { path }),

  /**
   * "Open terminal": the window the session runs in when Rust found it
   * (Windows), else the folder in VS Code. `owner` names the app a session
   * lives in when it is not a terminal ("cursor", "codex", "antigravity",
   * "claude-desktop"): Rust holds the list, and anything else is ignored.
   */
  openSession: (sessionId: string | null, path: string | null, owner: string | null = null) =>
    call<boolean>("open_session", { sessionId, path, owner }),

  /** The diff card's ↗: an existing file, in VS Code; never launched by its type. */
  openFileInVSCode: (path: string) => call<boolean>("open_file_in_vscode", { path }),

  quit: () => call<void>("quit_app"),

  openSettingsWindow: () => call<void>("open_settings_window"),

  /** Writes to %LOCALAPPDATA%\lintel\lintel.log, next to the Rust lines. */
  log: (message: string) => call<void>("log_line", { message }),

  // ── Claude Code hooks ─────────────────────────────────────────────────────
  hooksStatus: () => call<HookStatus>("hooks_status"),
  /** Pill ID → whether that agent's hooks reach Lintel (read-only, Mac #183). */
  agentHooksStatus: () => call<Record<string, boolean>>("agent_hooks_status"),
  /**
   * Diff to show before anything is written. `install: false` previews removal.
   * `launch`: the hooks that start a turn also start the app when it is closed
   * (the opt-in of Settings › Agents); left out, it stays as it is installed.
   */
  hooksPreview: (install: boolean, launch: boolean | null = null) =>
    callOrThrow<HookPreview>("hooks_preview", { install, launch }),
  /**
   * Writes ~/.claude/settings.json — only ever after an explicit click, and only
   * when the file still matches the preview the user looked at. `launch` must
   * be what the preview was asked with.
   */
  hooksApply: (install: boolean, fingerprint: string, launch: boolean | null = null) =>
    callOrThrow<string>("hooks_apply", { install, fingerprint, launch }),
  /** The relay started the app (it was closed when an agent began to work), not the user. */
  /** Lines of a file the session changed (inside its folder only), for the code view. */
  sessionFileLines: (cwd: string, path: string, from: number, count: number) =>
    call<string[] | null>("session_file_lines", { cwd, path, from, count }),
  /** Claude Code sessions already running at launch: [session id, folder]. */
  runningSessions: () => call<[string, string][]>("running_sessions"),
  autoLaunched: () => call<boolean>("auto_launched"),

  // ── Other agents (Gemini CLI, Codex, Cursor…) ─────────────────────────────
  agentHooksList: () => call<AgentHookStatus[]>("agent_hooks_list"),
  /** Diff to show before anything is written. `install: false` previews removal. */
  agentHooksPreview: (agent: string, install: boolean) =>
    callOrThrow<AgentHookPlan>("agent_hooks_preview", { agent, install }),
  /**
   * Writes the agent's config — only ever after an explicit click, and only when
   * it still matches the preview the user looked at. Returns the backups taken.
   */
  agentHooksApply: (agent: string, install: boolean, fingerprint: string) =>
    callOrThrow<string>("agent_hooks_apply", { agent, install, fingerprint }),
  // ── Plan usage: the status line relay, installed apart from the hooks ──────
  /** Diff of the status line change. `install: false` previews taking the relay out. */
  statusLinePreview: (install: boolean) => callOrThrow<HookPreview>("status_line_preview", { install }),
  /** Same rules as hooksApply: an explicit click, and only for the diff that was shown. */
  statusLineApply: (install: boolean, fingerprint: string) =>
    callOrThrow<string>("status_line_apply", { install, fingerprint }),
  /**
   * Asks the Codex CLI (`codex app-server`) for its plan limits, as Codex's
   * /status does. The raw `account/rateLimits/read` result, or null when Codex
   * is missing, not signed in or slow (15 s).
   */
  codexPlanUsage: () => call<unknown>("codex_plan_usage"),

  approvalDecision: (requestId: string, decision: Decision) =>
    call<void>("approval_decision", { requestId, decision }),
  /** "The card is up" — until this lands the relay only waits a moment. */
  approvalAck: (requestId: string) => call<void>("approval_ack", { requestId }),
  /**
   * Answers a question Claude Code asked: question text → chosen label, or the
   * words typed under "Other…" for the questions listed in `other`.
   */
  approvalAnswer: (requestId: string, answers: Record<string, string | string[]>, other: string[] = []) =>
    call<void>("approval_answer", { requestId, answers, other }),

  /** "Nobody can act on this" — Claude Code asks in the terminal right away. */
  approvalDecline: (requestId: string) => call<void>("approval_decline", { requestId }),

  // ── Chat, files, secrets ──────────────────────────────────────────────────
  /** One chat turn. The API key and any file bytes never leave Rust. */
  chatSend: (query: string, context: ChatContext | null) =>
    callOrThrow<{ text: string }>("chat_send", { query, context }),
  chatReset: () => call<void>("chat_reset"),
  /**
   * The models a provider offers, for the picker in the chat view. Rust asks
   * the provider only when it has a key (or a server address).
   */
  chatModels: (provider: string) => callOrThrow<ModelInfo[]>("chat_models", { provider }),
  /** Settings → Local models → Connect: does the server answer, and with which models? */
  /** The custom server's key, bound to the address it is entered for. */
  localSetKey: (url: string, key: string) => call<void>("local_set_key", { url, key }),

  localConnect: (provider: "ollama" | "lmstudio" | "custom", url: string) =>
    callOrThrow<LocalServer>("local_connect", { provider, url }),
  /** Copies a dropped file into the inbox. */
  ingestFile: (path: string) => callOrThrow<DroppedFile>("ingest_file", { path }),
  /** Only ever tells you whether a key exists — never its value. */
  secretPresent: (key: string) => call<boolean>("secret_present", { key }),
  secretSet: (key: string, value: string) => callOrThrow<void>("secret_set", { key, value }),
  secretClear: (key: string) => callOrThrow<void>("secret_clear", { key }),

  // ── Integrations ──────────────────────────────────────────────────────────
  refreshIntegration: (id: string) => call<void>("refresh_integration", { id }),
  /** The GitHub card is on screen: refetch that part if it is stale. */
  githubRefresh: (section: "pulse" | "activity", dirs: string[] = []) => call<void>("github_refresh", { section, dirs }),
  /** Opens the configured n8n instance in the browser. */
  openN8n: () => call<void>("open_n8n"),

  /** Tray → Pause. Stops the integration pollers, not just the island. */
  setPaused: (paused: boolean) => call<void>("set_paused", { paused }),

  /**
   * One frame of the tray's mascot, square RGBA (src/island/tray-mascot.ts). False when the host
   * cannot see the desktop (locked, a prompt of Windows): the frame is set, and nothing should follow it.
   */
  trayIcon: (rgba: Uint8Array) => call<boolean>("tray_icon", rgba),
  /** The mascot in the product's colour for the taskbar button of Lintel's windows; null: the icon the program carries. */
  windowIcon: (rgba: Uint8Array | null) => call<void>("window_icon", rgba ?? new Uint8Array(0)),
  /** The same as an .ico for Lintel's own shortcuts; null: back to the icon the program carries. */
  shortcutIcon: (ico: Uint8Array | null) => call<void>("shortcut_icon", ico ?? new Uint8Array(0)),

  // ── Global shortcuts ──────────────────────────────────────────────────────
  /** How each global shortcut went when Rust last registered them. */
  shortcutsStatus: () => call<ShortcutsReport>("shortcuts_status"),
  /** Lets go of every global shortcut while Settings records a new one. */
  shortcutsSuspend: (suspended: boolean) => call<void>("shortcuts_suspend", { suspended }),

  // ── Weekly recap ──────────────────────────────────────────────────────────
  /** Turns and decisions from `since` (Unix seconds) on, plus the recap prefs. */
  recapHistory: (since: number) => call<RecapHistory>("recap_history", { since: Math.floor(since) }),
  recapPrefs: () => call<RecapPrefs>("recap_prefs"),
  recapSetEnabled: (enabled: boolean) => call<void>("recap_set_enabled", { enabled }),
  recapSetHideProjects: (hide: boolean) => call<void>("recap_set_hide_projects", { hide }),
  /** `week` is the Monday (YYYY-MM-DD) the recap opened on its own for. */
  recapMarkShown: (week: string) => call<void>("recap_mark_shown", { week }),
  recapClear: () => call<void>("recap_clear"),
  /** Writes the PNG (a data URL) into Pictures or Downloads; returns its path. */
  recapSavePng: (data: string, week: string) => callOrThrow<string>("recap_save_png", { data, week }),
  /** Opens the folder of the image saved last. */
  recapRevealSaved: () => call<void>("recap_reveal_saved"),

  // ── Features (src-tauri/src/registry.rs): the only commands a feature uses ──
  /** Every feature with its switch and whether it runs. */
  featuresList: () => call<FeatureInfo[]>("features_list"),
  /** `Feature::call` on the Rust side; null when it failed or the feature is off. */
  featureCall: <T>(feature: string, method: string, args?: object) =>
    call<T>("feature_call", { feature, method, args: args ?? {} }),
  /** The feature's settings: manifest defaults with what is stored on top. */
  featureSettings: (feature: string) => call<Record<string, unknown>>("feature_settings", { feature }),
  /** Shallow merge; Rust saves and broadcasts `settings-changed:<id>`. */
  featurePatchSettings: (feature: string, patch: object) =>
    call<void>("feature_patch_settings", { feature, patch }),
  /** Starts or stops the feature and remembers the switch; throws what `start` reported. */
  featureSetEnabled: (feature: string, on: boolean) =>
    callOrThrow<void>("feature_set_enabled", { feature, on }),
};

export type ShortcutStatus =
  | "active" | "off" | "inUse" | "duplicate" | "invalid"
  | "typesCharacter" | "notPorted";

export interface ShortcutsReport {
  actions: { id: string; status: ShortcutStatus; typed?: string }[];
}

/** One feature, as `features_list` reports it (registry.rs FeatureInfo). */
export interface FeatureInfo {
  id: string;
  /** English; show it through `t()`. */
  name: string;
  group: "island" | "agents" | "chat" | "media" | "system" | "input" | "windows" | "files" | "capture" | "apps" | "tools";
  defaultOn: boolean;
  /** The user's switch. */
  on: boolean;
  /** Its Rust `start` succeeded and it has not been stopped. */
  running: boolean;
  uses: ("network" | "microphone" | "camera" | "clipboard" | "inputHook" | "capture" | "elevation" | "fileDelete")[];
  /** Registered as `<feature>:<id>` in `shortcuts_status` and `settings.shortcuts`. */
  shortcuts: { id: string; label: string; default: string | null }[];
  tray: { id: string; label: string }[];
}

export interface IntegrationUpdate {
  id: string;
  data: Record<string, unknown>;
  error: string | null;
  event: { success: boolean; label: string; detail: string | null } | null;
}

export type ChatContext =
  | { kind: "file"; name: string; path: string }
  | { kind: "window"; appName: string; title: string; url?: string };

export interface ModelInfo {
  id: string;
  label: string;
}

export interface LocalServer {
  /** The address as it is stored. */
  url: string;
  models: string[];
  /** The address is this machine. */
  loopback: boolean;
}

export interface DroppedFile {
  name: string;
  path: string;
  size: number;
}

export interface HookStatus {
  installed: boolean;
  /** Lintel's status line relay (plan usage) is the status line in settings.json. */
  planRelayInstalled: boolean;
  /** The installed hooks start the app when it is closed and an agent begins to work. */
  autoLaunch: boolean;
  settingsPath: string;
  hookPath: string;
  hookReady: boolean;
}

/** One agent other than Claude Code, as agents.rs reports it. */
export interface AgentHookStatus {
  /** The `--agent` name; its pill is `agent_<id>`. */
  id: string;
  name: string;
  installed: boolean;
  /** The file (or files, one per line) Lintel writes. */
  path: string;
  hookReady: boolean;
  /** The island can allow or deny this agent's permission requests. */
  approvals: boolean;
  /** What to do once it is written. */
  note: string;
}

export interface AgentHookPlan {
  diff: string;
  /** Where each existing file is copied first, one per line; "" when none. */
  backup: string;
  path: string;
  fingerprint: string;
}

export interface HookPreview {
  diff: string;
  backup: string;
  settingsPath: string;
  /** Hand back to hooksApply so only the reviewed diff is ever written. */
  fingerprint: string;
}

/** Same as `call`, but surfaces the error so the UI can show what went wrong. */
async function callOrThrow<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  if (!IS_TAURI) throw new Error("not running inside Lintel");
  return invoke<T>(cmd, args);
}

export type BridgeEvent =
  | { name: "cursor"; payload: { x: number; y: number } }
  | { name: "tray"; payload: string }
  | { name: "hook"; payload: Record<string, unknown> }
  | { name: "screen-changed"; payload: null };

export interface DragDropPayload {
  type: "enter" | "over" | "drop" | "leave";
  paths?: string[];
}

/**
 * The message that hands dropped File objects to Rust (src-tauri/src/webview_drop.rs).
 * It is shaped like one of Tauri's own IPC requests with no key: every string
 * message also reaches Tauri's IPC, which writes a JSON error to the console
 * for one it cannot read and drops one without its key in silence. `feature`
 * is the id of a feature with a drop zone of its own; none is the island's.
 */
export const dropMessage = (feature = "") =>
  JSON.stringify({ cmd: `${BRAND.slug}-file-drop`, callback: 0, error: 0, payload: { feature }, __TAURI_INVOKE_KEY__: "" });

interface WebView2Bridge {
  postMessageWithAdditionalObjects(message: unknown, objects: ArrayLike<unknown>): void;
}

/**
 * Files dragged onto the island. Only reaches us when the window takes the mouse.
 *
 * On Windows WebView2 takes the drop itself, as in Edge — Tauri's drop handling
 * never sees drags from the classic Explorer folder view (see
 * src-tauri/src/webview_drop.rs). Enter/over/leave come straight from the page;
 * the drop hands the File objects to Rust, which answers with their real paths
 * as a `file-drag` event.
 */
export async function onDragDrop(handler: (e: DragDropPayload) => void) {
  if (!IS_TAURI) return () => {};
  const webview = (window as unknown as { chrome?: { webview?: WebView2Bridge } }).chrome?.webview;
  if (!webview) return () => {};
  const hasFiles = (e: DragEvent) => e.dataTransfer?.types.includes("Files") ?? false;
  // dragenter/dragleave fire for every element crossed; only the outermost pair counts.
  let depth = 0;

  const onEnter = (e: DragEvent) => {
    if (!hasFiles(e)) return;
    e.preventDefault();
    if (depth++ === 0) handler({ type: "enter" });
  };
  const onOver = (e: DragEvent) => {
    if (!hasFiles(e)) return;
    // Without this WebView2 refuses the drop — or navigates to the file.
    e.preventDefault();
    if (e.dataTransfer) e.dataTransfer.dropEffect = "copy";
    handler({ type: "over" });
  };
  const onLeave = (e: DragEvent) => {
    if (!hasFiles(e) || depth === 0) return;
    if (--depth === 0) handler({ type: "leave" });
  };
  const onDrop = (e: DragEvent) => {
    e.preventDefault();
    depth = 0;
    const files = e.dataTransfer?.files;
    if (!files || files.length === 0) {
      handler({ type: "drop", paths: [] });
      return;
    }
    webview.postMessageWithAdditionalObjects(dropMessage(), files);
  };

  window.addEventListener("dragenter", onEnter);
  window.addEventListener("dragover", onOver);
  window.addEventListener("dragleave", onLeave);
  window.addEventListener("drop", onDrop);
  const unlisten = await listen<DragDropPayload>("file-drag", (e) => handler(e.payload));
  return () => {
    window.removeEventListener("dragenter", onEnter);
    window.removeEventListener("dragover", onOver);
    window.removeEventListener("dragleave", onLeave);
    window.removeEventListener("drop", onDrop);
    unlisten();
  };
}

export async function onEvent<T>(name: string, handler: (payload: T) => void) {
  if (!IS_TAURI) return () => {};
  return listen<T>(name, (e) => handler(e.payload));
}
