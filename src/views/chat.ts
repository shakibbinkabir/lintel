// Chat view — DOM port of PromptView / ChatBubble / TypingDotsView /
// ModelPickerView from IslandViewContent.swift.
//
// Answers are rendered as Markdown (markdown.ts). A local model, and a CLI
// that prints as it goes, stream their answer: Rust sends `chat-delta` events
// with the text visible so far. The model name above the text field opens the
// picker: provider chips, then the models of the chosen provider, asked for
// only once it is picked.
//
// From upstream pull requests: the picker's search field (471, its
// `windows/src/views/chat.ts`), model servers found on this computer offered
// as chips (400), Clear chat (368), an error that can be closed and leaves by
// itself (365; here it stays in the chat instead of taking the note view), and
// the task chip (406). Stop, for an answer a CLI is writing, is new.

import { h, svg, clear } from "./dom";
import { menu, type MenuItem } from "../ui/components";
import { ICONS, icon } from "../ui/icons";
import { codeCard, renderMarkdown } from "./markdown";
import { Bridge, onEvent, type ChatContext, type ModelInfo } from "../core/bridge";
import {
  KNOWN_CLIS, cancelTask, chatPrefs, installedClis, saveChatPrefs, scanLocal, startTask, stopAnswer,
  type FoundServer, type TaskEnded,
  answerMcp,
} from "../core/chat-tools";
import {
  CLI, SEARCH_FROM, activeModel, pickModel, providerDef, searchModels, visibleProviders, withModel,
  type PickerExtras, type ProviderDef,
} from "../core/providers";
import { Sound } from "../core/sound";
import { State, type ChatMessage, type Settings } from "../core/state";
import type { ViewHost } from "./views";
import { N_, t, tl } from "../i18n/i18n";

// Node runs this file as it is for the tests, with no bundler to take a
// stylesheet: it is only asked for where there is a page to style.
if (typeof document !== "undefined" && document.head) void import("./chat.css");

const STRINGS = {
  placeholderFirst: N_("Ask me anything…"),
  placeholderNext: N_("Continue…"),
  placeholderTask: N_("Describe a task…"),
  send: N_("Send"),
  stop: N_("Stop the answer"),
  switchModel: N_("Switch provider or model"),
  noModel: N_("Choose a model"),
  loading: N_("Loading models…"),
  searchModels: N_("Search models"),
  noMatch: N_("No model matches."),
  found: N_("Found on this computer. Click to use it."),
  noKey: N_("No API key — add it in Settings."),
  openSettings: N_("Open Settings"),
  clear: N_("Clear chat"),
  close: N_("Close"),
  task: N_("Task"),
  taskHint: N_("Hand the next message to Claude Code, in a folder you choose"),
  taskFolder: N_("Folder the task runs in"),
  taskRunning: N_("Running in {folder}…"),
  noFolder: N_("Choose a folder for the task first."),
  folderPath: N_("Full path of a folder, then Enter"),
  other: N_("Other…"),
  cancel: N_("Cancel"),
  canceled: N_("Canceled"),
  noImage: N_("This model does not take pictures."),
  withoutImage: N_("Send without the picture"),
  switchInstead: N_("Switch model"),
  remember: N_("Remember"),
  mcpAsk: N_("{tool} from {server} wants to run with:"),
  allow: N_("Allow"),
  deny: N_("Deny"),
};

/** A picture the chat can send, and what a model that cannot read one says. */
const IMAGE_FILE = /\.(png|jpe?g|gif|webp)$/i;
const NO_IMAGES = /image|vision|multimodal|modalit/i;
/** Text and code the chat shows as a card when dropped. */
const TEXT_FILE = /\.(txt|md|json|jsonc|ya?ml|toml|ini|csv|log|xml|html?|css|scss|js|mjs|cjs|jsx|ts|tsx|py|rs|go|java|kt|c|h|cpp|hpp|cs|rb|php|sh|ps1|bat|sql|lua|swift)$/i;

/** A square: "stop", next to the send arrow it replaces. Only the chat uses it. */
const STOP_ICON = "M7.5 7.5h9v9h-9z";
/** The error note leaves by itself after this long on screen. */
const NOTE_SECONDS = 10;
/** A finished task's pill stays this long, then goes like any ended session. */
const PILL_LINGER_MS = 60_000;
/** Folders remembered for the task chip. */
const MAX_FOLDERS = 8;

let nextId = 1;

/** A task that is still running: which bubble shows it, and which pill. */
interface RunningTask {
  messageId: number;
  pill: string;
}

/** By task id (chat/tasks.rs gives it). */
const running = new Map<number, RunningTask>();

function taskOf(message: ChatMessage): number | null {
  for (const [id, task] of running) if (task.messageId === message.id) return id;
  return null;
}

function bubble(message: ChatMessage): HTMLElement {
  if (message.role === "user") {
    return h(
      "div",
      { class: "chat-row user" },
      h("div", { class: "bubble", text: message.content }),
    );
  }
  const reply = h("div", { class: "reply" });
  renderMarkdown(reply, message.content);
  const task = taskOf(message);
  if (task == null) return h("div", { class: "chat-row" }, reply);
  return h(
    "div",
    { class: "chat-row task" },
    reply,
    h("button", { class: "btn sm ghost", text: tl(STRINGS.cancel), onclick: () => void cancelTask(task) }),
  );
}

function typingDots(): HTMLElement {
  return h(
    "div",
    { class: "chat-row" },
    h("div", { class: "typing" }, h("i"), h("i"), h("i")),
  );
}

/** The coloured chip showing what the question is about (a dropped file). */
function contextChip(label: string): HTMLElement {
  const chip = h("div", { class: "chip" }, h("i", { class: "chip-dot" }), h("span", { text: label }));
  requestAnimationFrame(() => chip.classList.add("settled"));
  return chip;
}

/**
 * A window the mascot was dragged onto becomes the context of a fresh chat (chat-plus).
 * False when it was not taken: an answer is on its way, or a card waits (the
 * chat must not open over it).
 */
export function attachWindow(w: { appName: string; title: string }): boolean {
  if (State.stateOverride === "thinking" || State.pendingApproval) return false;
  State.chatHistory = [];
  State.droppedFile = null;
  State.promptContext = { kind: "window", appName: w.appName, title: w.title };
  void Bridge.chatReset();
  State.notify();
  return true;
}

/** A picture or file made on this computer (a screenshot) joins a fresh chat, as attachWindow does. */
export function attachFile(f: { name: string; path: string }): boolean {
  if (State.stateOverride === "thinking" || State.pendingApproval) return false;
  State.chatHistory = [];
  State.droppedFile = null;
  State.promptContext = { kind: "file", name: f.name, path: f.path };
  void Bridge.chatReset();
  State.notify();
  return true;
}

function saveSettings() {
  void Bridge.saveSettings(State.settings);
}

const folderName = (path: string) => path.replace(/[\\/]+$/, "").split(/[\\/]/).pop() || path;

/** What an error is shown as: its first line, and not all of a long one. */
const oneLine = (text: string) => (text.trim().split(/\r?\n/)[0] ?? "").slice(0, 240);

// ── Model picker ──────────────────────────────────────────────────────────────

interface Picker {
  el: HTMLElement;
  /** The search field: it sits where the composer was while the picker is open. */
  search: HTMLElement;
  open(): void;
  close(): void;
  readonly isOpen: boolean;
}

/** Provider chips, then the chosen provider's models — ModelPickerView. */
function buildPicker(onChange: () => void): Picker {
  const chips = h("div", { class: "picker-chips" });
  const list = h("div", { class: "picker-list" });
  const el = h("div", { class: "picker" }, chips, h("div", { class: "picker-rule" }), list);
  const searchInput = h("input", {
    type: "text",
    placeholder: tl(STRINGS.searchModels),
    spellcheck: "false",
    autocomplete: "off",
  }) as HTMLInputElement;
  const search = h("label", { class: "field picker-search-field" }, icon("search", 14), searchInput);

  /** Models already asked for, by provider; a model server is asked again each time. */
  const cache = new Map<string, ModelInfo[]>();
  /** What the list shows, so that typing in the search field can filter it. */
  let shown: { p: ProviderDef; models: ModelInfo[] } | null = null;
  /** What was found the last time the picker opened. */
  let extras: PickerExtras = {};
  let found: FoundServer[] = [];
  let isOpen = false;
  let request = 0;
  let opened = 0;

  // The chips are one line that scrolls sideways: the wheel moves it.
  chips.addEventListener("wheel", (e) => {
    chips.scrollLeft += (e as WheelEvent).deltaY;
  }, { passive: true });

  /** The servers found on this computer that are not connected yet. */
  function offered(): FoundServer[] {
    return found.filter((server) => {
      const field = providerDef(server.id).urlField;
      return server.models.length > 0 && field != null && State.settings[field] === "";
    });
  }

  function drawChips() {
    clear(chips);
    for (const p of visibleProviders(State.settings, extras)) {
      const on = p.id === State.settings.chatProvider;
      const chip = h(
        "button",
        { class: on ? "picker-chip on" : "picker-chip", style: `--accent:${p.accent}` },
        h("i", { class: "picker-dot" }),
        h("span", { text: t(p.name) }),
      );
      chip.addEventListener("click", () => {
        if (p.id === State.settings.chatProvider) return;
        State.settings = { ...State.settings, chatProvider: p.id };
        saveSettings();
        Sound.play("pop");
        drawChips();
        void loadModels();
        onChange();
      });
      chips.append(chip);
    }
    for (const server of offered()) {
      const chip = h(
        "button",
        { class: "picker-chip found", style: `--accent:${providerDef(server.id).accent}`, title: tl(STRINGS.found) },
        h("i", { class: "picker-dot" }),
        h("span", { text: server.name }),
      );
      chip.addEventListener("click", () => connect(server));
      chips.append(chip);
    }
    chips.querySelector(".picker-chip.on")?.scrollIntoView({ block: "nearest", inline: "nearest" });
  }

  /** A found server is taken: connected with nothing to type, and no key. */
  function connect(server: FoundServer) {
    const p = providerDef(server.id);
    if (!p.urlField) return;
    let next: Settings = { ...State.settings, chatProvider: p.id };
    next[p.urlField] = server.url;
    if (!server.models.includes(next.chatModels[p.id] ?? "")) next = withModel(next, p.id, server.models[0]);
    State.settings = next;
    saveSettings();
    Sound.play("pop");
    drawChips();
    void loadModels();
    onChange();
  }

  function status(text: string, withSettings = false) {
    shown = null;
    search.classList.remove("on");
    clear(list);
    const line = h("div", { class: "picker-status", text });
    if (withSettings) {
      line.append(
        h("button", {
          class: "picker-link",
          text: tl(STRINGS.openSettings),
          onclick: () => void Bridge.openSettingsWindow(),
        }),
      );
    }
    list.append(line);
  }

  function drawModels(p: ProviderDef, models: ModelInfo[]) {
    shown = { p, models };
    const searchable = models.length > SEARCH_FROM;
    search.classList.toggle("on", searchable);
    const matching = searchable ? searchModels(models, searchInput.value) : models;
    clear(list);
    if (matching.length === 0) list.append(h("div", { class: "picker-status", text: t(STRINGS.noMatch) }));
    const current = activeModel(State.settings);
    for (const m of matching) {
      const on = m.id === current;
      const row = h(
        "button",
        { class: on ? "picker-model on" : "picker-model", style: `--accent:${p.accent}`, title: m.id },
        h("span", { class: "picker-model-name", text: m.label }),
        on ? svg(ICONS.check, 11, { stroke: 2.2 }) : null,
      );
      row.addEventListener("click", () => {
        State.settings = withModel(State.settings, p.id, m.id);
        saveSettings();
        Sound.play("blip");
        close();
      });
      list.append(row);
    }
    list.querySelector(".picker-model.on")?.scrollIntoView({ block: "nearest" });
  }

  async function loadModels() {
    const p = providerDef(State.settings.chatProvider);
    const ticket = ++request;
    searchInput.value = "";
    // A model server is asked again each time, and so is what is installed.
    const cached = p.urlField || p.id === CLI.id ? undefined : cache.get(p.id);
    if (cached) {
      drawModels(p, cached);
      if (cached.length > SEARCH_FROM) searchInput.focus();
      return;
    }
    // Nothing is asked of a provider that has no key yet.
    // (A Claude gateway has a token of its own, or none.)
    const gateway = p.id === "anthropic" && (await chatPrefs()).gatewayUrl !== "";
    if (p.key && !gateway && !(await Bridge.secretPresent(p.key))) {
      if (ticket === request) status(t(STRINGS.noKey), true);
      return;
    }
    if (ticket !== request) return;
    status(t(STRINGS.loading));
    try {
      const models = await Bridge.chatModels(p.id);
      if (ticket !== request) return;
      if (!p.urlField) cache.set(p.id, models);
      const keep = pickModel(p, models.map((m) => m.id), activeModel(State.settings));
      if (keep && keep !== activeModel(State.settings)) {
        State.settings = withModel(State.settings, p.id, keep);
        saveSettings();
        onChange();
      }
      drawModels(p, models);
      // A long list is searched by typing, straight away.
      if (models.length > SEARCH_FROM) searchInput.focus();
    } catch (err) {
      if (ticket === request) status(oneLine(String(err).replace(/^Error:\s*/, "")), Boolean(p.key));
    }
  }

  /**
   * What only shows once it is known: the free OpenRouter (a setting), the
   * CLI chip (a CLI is installed) and the model servers running on this
   * computer. Asked each time the user opens the picker, never otherwise.
   */
  function lookAround() {
    const mine = ++opened;
    const then = <T>(asked: Promise<T>, use: (answer: T) => void) =>
      void asked.then((answer) => {
        if (!isOpen || mine !== opened) return;
        use(answer);
        drawChips();
      }, () => {});
    then(chatPrefs(), (prefs) => {
      extras = { ...extras, openrouterFree: prefs.openrouterFree };
    });
    then(installedClis(), (clis) => {
      extras = { ...extras, cli: clis.length > 0 };
    });
    then(scanLocal(), (servers) => {
      found = servers;
    });
  }

  searchInput.addEventListener("input", () => {
    if (shown) drawModels(shown.p, shown.models);
  });
  searchInput.addEventListener("keydown", (e) => {
    const key = (e as KeyboardEvent).key;
    if (key === "Enter") {
      // The first match is the answer to what was typed.
      e.preventDefault();
      e.stopPropagation();
      (list.querySelector(".picker-model") as HTMLElement | null)?.click();
    } else if (key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      close();
    }
  });

  function open() {
    isOpen = true;
    el.classList.add("on");
    drawChips();
    onChange();
    void loadModels();
    lookAround();
  }

  function close() {
    isOpen = false;
    request++;
    opened++;
    shown = null;
    search.classList.remove("on");
    el.classList.remove("on");
    onChange();
  }

  return {
    el,
    search,
    open,
    close,
    get isOpen() {
      return isOpen;
    },
  };
}

// ── View ──────────────────────────────────────────────────────────────────────

export function buildPrompt(onHeightChange: () => void): ViewHost {
  const chipRow = h("div", { class: "chip-row" });
  const log = h("div", { class: "chat-log" });
  const input = h("input", {
    type: "text",
    class: "chat-input",
    placeholder: t(STRINGS.placeholderFirst),
    spellcheck: "false",
  }) as HTMLInputElement;
  const send = h("button", { class: "btn primary icon send-btn", title: tl(STRINGS.send) }, icon("send", 14));
  // The mascot sits left of the field (core/layout.ts), hence the room kept for it.
  // The dictation feature puts its microphone button in this slot (empty when it is off).
  const micSlot = h("span", { class: "chat-mic-slot" });
  const bar = h("div", { class: "isl-compose" }, h("label", { class: "field" }, input), micSlot, send);

  const modelDot = h("i", { class: "model-dot" });
  const modelName = h("span", { class: "model-name" });
  const modelBtn = h(
    "button",
    { class: "model-btn", title: tl(STRINGS.switchModel) },
    modelDot,
    modelName,
    svg(ICONS.chevronUpDown, 9, { stroke: 2 }),
  );

  // The task chip: on, the next message goes to Claude Code in the folder next to it.
  // It is only there with Claude Code installed (see `lookForClaude`).
  const taskBtn = h(
    "button",
    { class: "task-chip", title: tl(STRINGS.taskHint), "aria-pressed": "false" },
    icon("terminal", 12),
    h("span", { text: tl(STRINGS.task) }),
  );
  // The folder, and a menu of the others. (The design language's menu, which
  // stays inside the island: a native list would open outside its shape.)
  const folderLabel = h("span", { class: "task-folder-name" });
  const folderBtn = h(
    "button",
    { class: "task-folder", title: tl(STRINGS.taskFolder) },
    folderLabel,
    svg(ICONS.chevronUpDown, 9, { stroke: 2 }),
  );
  // "Other…": the button steps aside for a field that takes any full path.
  const folderField = h("input", {
    type: "text",
    class: "task-folder",
    placeholder: tl(STRINGS.folderPath),
    spellcheck: "false",
  }) as HTMLInputElement;
  const clearBtn = h(
    "button",
    { class: "chat-clear", title: tl(STRINGS.clear), "aria-label": tl(STRINGS.clear) },
    icon("trash", 12),
  );

  // What went wrong with the last message: it stays in the chat, where what it
  // asks for (another model, a key) is done, and leaves by itself.
  const noteText = h("span", { class: "chat-note-text" });
  const note = h(
    "div",
    { class: "chat-note", role: "alert" },
    icon("alert", 14),
    noteText,
    h("button", {
      class: "btn sm ghost icon",
      title: tl(STRINGS.close),
      "aria-label": tl(STRINGS.close),
      onclick: () => hideNote(),
    }, icon("close", 14)),
  );

  // A dropped text or code file, as a code card with a copy button.
  const fileCard = h("div", { class: "chat-file-card", style: "max-height:130px;overflow:auto;display:none" });
  let cardPath = "";

  const body = h("div", { class: "chat-body" });
  let pickerWasOpen = false;
  const picker = buildPicker(() => {
    body.classList.toggle("picking", picker.isOpen);
    drawModelButton();
    if (picker.isOpen === pickerWasOpen) return;
    pickerWasOpen = picker.isOpen;
    State.pickingModel = picker.isOpen;
    onHeightChange(); // the island takes its full chat height for the list
    // The composer steps aside while the picker is open (chat.css): the
    // keyboard stays in the chat, and goes back to the field afterwards.
    if (picker.isOpen) modelBtn.focus();
    else if (State.view === "prompt") input.focus();
  });
  // With the field away, Escape lands here: the picker folds first, and the
  // next Escape is the island's.
  body.addEventListener("keydown", (e) => {
    if ((e as KeyboardEvent).key !== "Escape" || !picker.isOpen) return;
    e.preventDefault();
    e.stopPropagation();
    picker.close();
  });
  const modelRow = h(
    "div",
    { class: "model-row" },
    taskBtn, folderBtn, folderField, picker.search, h("span", { class: "model-gap" }), clearBtn, modelBtn,
  );
  body.append(chipRow, fileCard, log, picker.el, note, modelRow, bar);

  const el = h("div", { class: "view" }, body);

  let sending = false;
  /** The user pressed Stop: the turn's failure is theirs, not an error to show. */
  let stopped = false;
  let renderedCount = -1;
  // A local model answers token by token: where its text so far is shown.
  let live: HTMLElement | null = null;
  let taskMode = false;
  /** The folders on offer, as the task chip last loaded them, and the chosen one. */
  let folders: string[] = [];
  let folder = "";
  let noteTimer: number | null = null;
  /** The chat is on screen, as of the last `sync`. */
  let onScreen = false;

  /**
   * No Claude Code, no task chip. Asked each time the chat comes on screen
   * (file names are looked at, nothing is started), so installing it is
   * enough to get the chip.
   */
  taskBtn.style.display = "none";
  function lookForClaude() {
    void installedClis().then((clis) => {
      const there = clis.some((cli) => cli.id === "claude");
      taskBtn.style.display = there ? "" : "none";
      if (!there && taskMode) setTaskMode(false);
    });
  }

  /** An answer a CLI is writing can be stopped; a request cannot. */
  const stoppable = () => sending && State.settings.chatProvider === CLI.id;

  function drawModelButton() {
    const p = providerDef(State.settings.chatProvider);
    modelDot.style.background = p.accent;
    const model = activeModel(State.settings);
    // A CLI goes by its name, a model by its id.
    const cli = p.id === CLI.id ? KNOWN_CLIS.find((known) => known.id === model)?.name : undefined;
    modelName.textContent = cli ?? (model || t(STRINGS.noModel));
    modelBtn.classList.toggle("open", picker.isOpen);
    modelBtn.disabled = sending;
  }

  function drawSend() {
    const mode = stoppable() ? "stop" : "send";
    if (send.dataset.mode === mode) return;
    send.dataset.mode = mode;
    clear(send);
    send.append(icon(mode === "stop" ? STOP_ICON : "send", 14));
    send.title = t(mode === "stop" ? STRINGS.stop : STRINGS.send);
  }

  modelBtn.addEventListener("click", () => {
    if (sending) return;
    if (picker.isOpen) picker.close();
    else picker.open();
  });

  // ── The error note ──────────────────────────────────────────────────────────

  function showNote(text: string) {
    hideNote();
    noteText.textContent = oneLine(text);
    note.title = text;
    note.classList.add("on");
  }

  function hideNote() {
    if (noteTimer != null) window.clearTimeout(noteTimer);
    noteTimer = null;
    note.classList.remove("on");
  }

  /** The note's time runs while it can be read, not while the island is away. */
  function armNote() {
    if (noteTimer != null || !note.classList.contains("on")) return;
    if (State.view !== "prompt" || State.mode !== "expanded") return;
    noteTimer = window.setTimeout(hideNote, NOTE_SECONDS * 1000);
  }

  // ── Clear chat ──────────────────────────────────────────────────────────────

  clearBtn.addEventListener("click", () => {
    // Not while an answer is on its way: it would land in the cleared chat.
    if (sending) return;
    // A task that is still running keeps its line, and with it its Cancel.
    State.chatHistory = State.chatHistory.filter((m) => taskOf(m) != null);
    State.droppedFile = null;
    State.promptContext = null;
    void Bridge.chatReset();
    hideNote();
    Sound.play("blip");
    renderedCount = -1;
    State.notify();
    onHeightChange();
    input.focus();
  });

  // ── The task chip ───────────────────────────────────────────────────────────

  function pickFolder(path: string) {
    folder = path;
    folderLabel.textContent = folderName(path);
    folderBtn.title = path;
    showFolderField(false);
  }

  /** Remembered folders first, then where the agents' sessions run right now. */
  async function loadFolders() {
    const prefs = await chatPrefs();
    const sessions = State.tasks.map((task) => task.sessionCwd ?? "").filter(Boolean);
    folders = [...new Set([...prefs.taskFolders, ...sessions])];
    if (folders.length > 0) pickFolder(folders[0]);
    else showFolderField(true);
  }

  function showFolderField(on: boolean) {
    folderField.classList.toggle("on", taskMode && on);
    folderBtn.classList.toggle("on", taskMode && !on);
    if (taskMode && on) folderField.focus();
  }

  folderBtn.addEventListener("click", () => {
    const at = folderBtn.getBoundingClientRect();
    const items: (MenuItem | null)[] = folders.map((path) => ({
      label: folderName(path),
      icon: path === folder ? "check" : "file",
      onClick: () => pickFolder(path),
    }));
    items.push(null, {
      label: t(STRINGS.other),
      icon: "plus",
      onClick: () => {
        folderField.value = "";
        showFolderField(true);
      },
    });
    menu(items, { x: at.left, y: at.bottom + 4 });
  });

  function setTaskMode(on: boolean) {
    taskMode = on;
    taskBtn.classList.toggle("on", on);
    taskBtn.setAttribute("aria-pressed", String(on));
    showFolderField(false);
    if (on) void loadFolders();
    input.placeholder = t(placeholder());
  }

  const placeholder = () =>
    taskMode ? STRINGS.placeholderTask
    : State.chatHistory.length === 0 ? STRINGS.placeholderFirst : STRINGS.placeholderNext;

  taskBtn.addEventListener("click", () => {
    setTaskMode(!taskMode);
    Sound.play("pop");
    if (!folderField.classList.contains("on")) input.focus();
  });

  folderField.addEventListener("keydown", (e) => {
    const key = (e as KeyboardEvent).key;
    if (key === "Enter") {
      e.preventDefault();
      e.stopPropagation();
      input.focus();
    } else if (key === "Escape" && folder) {
      // Back to the chosen folder; with none to go back to, Escape is the island's.
      e.preventDefault();
      e.stopPropagation();
      showFolderField(false);
    }
  });

  /** The folder the next task runs in: the typed one while its field is up. */
  function taskFolder(): string {
    const path = folderField.classList.contains("on") ? folderField.value.trim() : folder;
    return path.replace(/[\\/]+$/, "");
  }

  /** The task chip's send: the message goes to a task, and the chat stays free. */
  async function submitTask(query: string) {
    const dir = taskFolder();
    if (!dir) {
      showNote(t(STRINGS.noFolder));
      State.notify();
      return;
    }
    hideNote();
    let taskId: number;
    try {
      taskId = await startTask(query, dir);
    } catch (err) {
      // Nothing started (no such folder, no Claude Code, three already running).
      showNote(String(err).replace(/^Error:\s*/, ""));
      Sound.play("error");
      State.notify();
      return;
    }
    input.value = "";
    Sound.play("send");
    State.chatHistory.push({ id: nextId++, role: "user", content: query });
    const line: ChatMessage = { id: nextId++, role: "assistant", content: t(STRINGS.taskRunning, { folder: folderName(dir) }) };
    State.chatHistory.push(line);

    // The task shows as a session pill of its own, like any agent's session.
    const pill = `task_${taskId}`;
    State.upsertExternalAgent(pill, folderName(dir), CLI.accent);
    const session = State.tasks.find((task) => task.id === pill);
    if (session) session.sessionCwd = dir;
    State.updateTask(pill, "working");
    State.appendStep(pill, oneLine(query));
    running.set(taskId, { messageId: line.id, pill });

    // One message, one task: the chip goes off, the folder is remembered.
    const prefs = await chatPrefs();
    void saveChatPrefs({ taskFolders: [dir, ...prefs.taskFolders.filter((f) => f !== dir)].slice(0, MAX_FOLDERS) });
    setTaskMode(false);
    State.notify();
    onHeightChange();
    input.focus();
  }

  void onEvent<TaskEnded>("chat-task", (ended) => {
    const task = running.get(ended.id);
    if (!task) return;
    running.delete(ended.id);
    const line = State.chatHistory.find((m) => m.id === task.messageId);
    if (line) line.content = ended.cancelled ? t(STRINGS.canceled) : ended.text;

    const session = State.tasks.find((x) => x.id === task.pill);
    if (ended.cancelled) {
      State.removeTask(task.pill);
    } else if (session) {
      session.finalLine = oneLine(ended.text.replace(/^[#>*\s-]+/, ""));
      State.updateTask(task.pill, ended.ok ? "finished" : "error");
      if (State.focusId !== task.pill) State.setPillBadge(task.pill, ended.ok ? "finished" : "error");
      window.setTimeout(() => State.removeTask(task.pill), PILL_LINGER_MS);
      Sound.play(ended.ok ? "finish" : "error");
    }
    renderedCount = -1; // the bubble changed, the count did not
    State.notify();
    onHeightChange();
  });

  // ── A chat turn ─────────────────────────────────────────────────────────────

  void onEvent<string>("chat-delta", (text) => {
    if (!sending || !text) return; // nothing visible yet: the dots stay
    if (!live) {
      live = h("div", { class: "reply" });
      log.querySelector(".typing")?.parentElement?.remove();
      log.append(h("div", { class: "chat-row" }, live));
    }
    renderMarkdown(live, text);
    log.scrollTop = log.scrollHeight;
  });

  /** A tool of an MCP server: one click per call. Resolved by the card; Stop or New chat answers no for it (Rust). */
  void onEvent<{ id: number; server: string; tool: string; args: unknown }>("chat-mcp-approve", (ask) => {
    const answer = (yes: boolean) => {
      card.remove();
      void answerMcp(ask.id, yes);
      onHeightChange();
    };
    const card: HTMLElement = h(
      "div",
      { class: "chat-note on", role: "alert" },
      icon("alert", 14),
      h("span", { class: "chat-note-text", text: t(STRINGS.mcpAsk, { tool: ask.tool, server: ask.server }) }),
      h("code", { text: JSON.stringify(ask.args).slice(0, 400) }),
      h("button", { class: "btn sm", text: t(STRINGS.allow), onclick: () => answer(true) }),
      h("button", { class: "btn sm ghost", text: t(STRINGS.deny), onclick: () => answer(false) }),
    );
    body.insertBefore(card, modelRow);
    onHeightChange();
  });

  /** A model refused the picture: send without it, switch model, or cancel. Remembered when asked to. */
  async function imageChoice(): Promise<"drop" | "switch" | "cancel"> {
    const kept = (await chatPrefs()).imageRefusal;
    if (kept) return kept;
    State.stateOverride = null;
    State.notify();
    return new Promise((resolve) => {
      const remember = h("input", { type: "checkbox", checked: "" }) as HTMLInputElement;
      remember.checked = true;
      const choose = (how: "drop" | "switch" | "cancel") => {
        card.remove();
        if (how !== "cancel" && remember.checked) void saveChatPrefs({ imageRefusal: how });
        onHeightChange();
        resolve(how);
      };
      const card: HTMLElement = h(
        "div",
        { class: "chat-note on", role: "alert" },
        icon("alert", 14),
        h("span", { class: "chat-note-text", text: t(STRINGS.noImage) }),
        h("button", { class: "btn sm", text: t(STRINGS.withoutImage), onclick: () => choose("drop") }),
        h("button", { class: "btn sm", text: t(STRINGS.switchInstead), onclick: () => choose("switch") }),
        h("button", { class: "btn sm ghost", text: t(STRINGS.cancel), onclick: () => choose("cancel") }),
        h("label", { class: "hint" }, remember, h("span", { text: t(STRINGS.remember) })),
      );
      body.insertBefore(card, modelRow);
      onHeightChange();
    });
  }

  async function submit() {
    if (stoppable()) {
      stopped = true;
      void stopAnswer();
      return;
    }
    const query = input.value.trim();
    if (!query || sending) return;
    if (picker.isOpen) picker.close();
    if (taskMode) {
      await submitTask(query);
      return;
    }
    input.value = "";
    sending = true;
    stopped = false;
    hideNote();
    drawModelButton();
    Sound.play("send");

    State.chatHistory.push({ id: nextId++, role: "user", content: query });
    State.stateOverride = "thinking";
    State.notify();
    onHeightChange();

    const pc = State.promptContext;
    const context: ChatContext | null =
      State.chatHistory.length !== 1 || !pc ? null
      : pc.kind === "window" ? pc
      : pc.path ? { kind: "file", name: pc.name, path: pc.path } : null;

    try {
      let reply;
      try {
        reply = await Bridge.chatSend(query, context);
      } catch (err) {
        const refused = !stopped && context?.kind === "file" && IMAGE_FILE.test(context.name) && NO_IMAGES.test(String(err));
        const how = refused ? await imageChoice() : "cancel";
        if (how === "cancel") throw err;
        if (how === "switch") {
          // Back to the question, with the picker open; nothing is shown as an error.
          State.chatHistory.pop();
          input.value = query;
          stopped = true;
          picker.open();
          throw err;
        }
        State.stateOverride = "thinking";
        State.notify();
        reply = await Bridge.chatSend(query, null);
      }
      State.chatHistory.push({ id: nextId++, role: "assistant", content: reply.text });
      State.stateOverride = null;
      Sound.play("finish");
    } catch (err) {
      State.stateOverride = null;
      if (!stopped) {
        showNote(String(err).replace(/^Error:\s*/, ""));
        Sound.play("error");
      }
    } finally {
      sending = false;
      live = null;
      renderedCount = -1; // the finished answer replaces the streamed one
      drawModelButton();
      State.notify();
      onHeightChange();
      input.focus();
    }
  }

  send.addEventListener("click", () => void submit());
  input.addEventListener("keydown", (e) => {
    const key = (e as KeyboardEvent).key;
    // Only what the field itself uses stops here. Every other key goes on to
    // the island: Escape folds it, Alt+letter jumps, and typing keeps it open.
    // (All of them used to stop here: Escape did nothing while the chat had the text cursor.)
    if (key === "Enter") {
      e.preventDefault();
      e.stopPropagation();
      if (!sending) void submit();
    } else if (key === "Escape" && picker.isOpen) {
      // The picker folds first; the next Escape is the island's.
      e.preventDefault();
      e.stopPropagation();
      picker.close();
    }
  });

  return {
    el,
    sync() {
      const pc = State.promptContext;
      const wantChip = pc ? (pc.kind === "window" ? `${pc.appName}, ${pc.title.slice(0, 40)}` : pc.name) : "";
      if (chipRow.dataset.label !== wantChip) {
        chipRow.dataset.label = wantChip;
        clear(chipRow);
        if (wantChip) chipRow.append(contextChip(wantChip));
      }

      // A dropped text file shows as a card; its copy in the inbox is read once.
      const file = pc?.kind === "file" && TEXT_FILE.test(pc.name) ? (pc.path ?? "") : "";
      if (file !== cardPath) {
        cardPath = file;
        fileCard.style.display = "none";
        clear(fileCard);
        if (file) {
          void Bridge.featureCall<string | null>("chat-plus", "read_text", { path: file }).then(
            (text) => {
              if (cardPath !== file || !text) return;
              fileCard.append(codeCard(text));
              fileCard.style.display = "";
              onHeightChange();
            },
            () => {},
          );
        }
      }

      const thinking = State.stateOverride === "thinking";
      const count = State.chatHistory.length + (thinking ? 0.5 : 0);
      if (count !== renderedCount && !live) {
        renderedCount = count;
        clear(log);
        for (const m of State.chatHistory) log.append(bubble(m));
        if (thinking) log.append(typingDots());
        log.scrollTop = log.scrollHeight;
      }

      // Leaving the chat folds the picker away.
      if (State.view !== "prompt" && picker.isOpen) picker.close();
      drawModelButton();
      drawSend();
      armNote();
      const shown = State.view === "prompt" && State.mode === "expanded";
      if (shown && !onScreen) lookForClaude();
      onScreen = shown;

      clearBtn.style.display = State.chatHistory.length > 0 ? "" : "none";
      clearBtn.disabled = sending;
      input.placeholder = t(placeholder());
      // While a CLI writes, the field waits but Stop stays within reach.
      input.disabled = sending;
      send.disabled = sending && !stoppable();
    },
    focus() {
      input.focus();
      input.select();
    },
  };
}
