// What the chat asks of Rust besides a chat turn (src-tauri/src/chat.rs), and
// the chat's own settings: the model servers running on this computer, the
// agent CLIs installed on it, stopping an answer, tasks, and the GitHub card's
// pull requests through the local `gh` login.

import { invoke } from "@tauri-apps/api/core";
import { Bridge, IS_TAURI } from "./bridge";
import type { GitHubPulse } from "./github";

/**
 * One of chat.rs's questions (`chat::ask`).
 * debt: they ride the `local_connect` command, under names no provider has,
 * until each has a command of its own in lib.rs and an entry in bridge.ts.
 */
async function ask<T>(what: string, arg = ""): Promise<T | null> {
  if (!IS_TAURI) return null;
  const answer = await invoke<{ extra?: T | null } | null>("local_connect", { provider: what, url: arg });
  return answer?.extra ?? null;
}

/** A model server found running on this computer (chat/scan.rs). */
export interface FoundServer {
  /** The provider it connects as. */
  id: "ollama" | "lmstudio" | "custom";
  name: string;
  url: string;
  models: string[];
}

/**
 * Asks the usual ports of this computer, and nothing else, which model
 * servers answer. Call it when the user opens the picker or Settings.
 */
export const scanLocal = (): Promise<FoundServer[]> =>
  ask<FoundServer[]>("chat:scan").then((found) => found ?? [], () => []);

/** An agent CLI found on this computer (chat/cli.rs). */
export interface CliInfo {
  id: string;
  name: string;
}

/** Every CLI the chat can go through, installed or not, in the table's order. */
export const KNOWN_CLIS: readonly CliInfo[] = [
  { id: "claude", name: "Claude Code" },
  { id: "codex", name: "Codex" },
  { id: "gemini", name: "Gemini CLI" },
  { id: "cursor", name: "Cursor" },
  { id: "kiro", name: "Kiro" },
];

/** The installed ones: files are looked for, nothing is started. */
export const installedClis = (): Promise<CliInfo[]> =>
  ask<CliInfo[]>("chat:clis").then((found) => found ?? [], () => []);

/** An MCP server other apps set up (chat/mcp.rs); turned on in the chat's settings. */
export interface McpServer {
  id: string;
  source: string;
  command: string;
}

/** The servers found in other apps' config files (read only). */
export const mcpServers = (): Promise<McpServer[]> =>
  ask<McpServer[]>("chat:mcp-list").then((found) => found ?? [], () => []);

/** The click on a tool call's card. */
export const answerMcp = (id: number, allow: boolean): Promise<unknown> =>
  ask("chat:mcp-answer", `${id}:${allow ? 1 : 0}`).catch(() => null);

/** Stop: ends the CLI that is writing an answer. */
export const stopAnswer = (): Promise<unknown> => ask("chat:stop").catch(() => null);

/**
 * Hands `query` to a task that runs in `folder` (chat/tasks.rs) and resolves
 * to its id; how it ended comes later, as a `chat-task` event.
 */
export async function startTask(query: string, folder: string): Promise<number> {
  if (!IS_TAURI) throw new Error("not running inside Lintel");
  const reply = await invoke<{ task?: number } | null>("chat_send", { query, context: { kind: "task", folder } });
  if (reply?.task == null) throw new Error("the task did not start");
  return reply.task;
}

export const cancelTask = (id: number): Promise<unknown> => ask("chat:task-cancel", String(id)).catch(() => null);

/** How a task ended: the payload of the `chat-task` event. */
export interface TaskEnded {
  id: number;
  ok: boolean;
  cancelled: boolean;
  /** The answer, as Markdown, or why there is none. */
  text: string;
}

/**
 * The GitHub card's pull requests, fetched with the local `gh` login. Null
 * when that is not what the user set up; rejects with the reason when it is
 * and `gh` or GitHub said no.
 */
export const pulseThroughGh = (): Promise<{ pulse: GitHubPulse } | null> =>
  ask<{ pulse: GitHubPulse }>("github:pulse");

// ── Settings of the chat that are not in `Settings` ───────────────────────────
//
// Kept with the chat feature's own settings (`features.chat` in settings.json):
// Rust reads them there (chat.rs), and no field was added to `Settings`.

export interface ChatPrefs {
  /** Settings → Chat providers → OpenRouter → Free models only. */
  openrouterFree: boolean;
  /** The CLIs tried, in this order, after the one picked in the chat. */
  cliOrder: string[];
  /** Folders a task ran in, the latest first. */
  taskFolders: string[];
  /** An Anthropic-compatible gateway for Claude (https or this computer); empty: Anthropic. */
  gatewayUrl: string;
  /** Today's calendar agenda goes to the chat provider. Off until the user turns it on. */
  calendarAgenda: boolean;
  /** What to do when a model refuses a picture: "" asks every time. */
  imageRefusal: "" | "drop" | "switch";
  /** MCP servers (by name) the chat may use, one click per call. Off until chosen. */
  mcpServers: string[];
}

const strings = (value: unknown): string[] =>
  Array.isArray(value) ? value.filter((item): item is string => typeof item === "string") : [];

export async function chatPrefs(): Promise<ChatPrefs> {
  const stored = (await Bridge.featureSettings("chat")) ?? {};
  return {
    openrouterFree: stored.openrouterFree === true,
    cliOrder: strings(stored.cliOrder),
    taskFolders: strings(stored.taskFolders),
    gatewayUrl: typeof stored.gatewayUrl === "string" ? stored.gatewayUrl : "",
    calendarAgenda: stored.calendarAgenda === true,
    mcpServers: strings(stored.mcpServers),
    imageRefusal: stored.imageRefusal === "drop" || stored.imageRefusal === "switch" ? stored.imageRefusal : "",
  };
}

export const saveChatPrefs = (patch: Partial<ChatPrefs>): Promise<unknown> =>
  Bridge.featurePatchSettings("chat", patch);

/** "Use the gh login", kept with the integrations feature (github.rs reads it). */
export async function ghLoginOn(): Promise<boolean> {
  return ((await Bridge.featureSettings("integrations")) ?? {}).ghLogin === true;
}

export const setGhLogin = (on: boolean): Promise<unknown> =>
  Bridge.featurePatchSettings("integrations", { ghLogin: on });
