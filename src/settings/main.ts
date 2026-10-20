// Settings window — the place where anything that writes to disk is confirmed.
// Stage 2 covers the Claude Code hooks and the general preferences; API keys and
// integrations land here too in a later stage.

import { BRAND } from "../core/brand";
import "./settings.css";
import { Bridge, onEvent, type FeatureInfo, type HookPreview, type HookStatus, type ShortcutsReport } from "../core/bridge";
import {
  KNOWN_CLIS, chatPrefs, ghLoginOn, installedClis, mcpServers, pulseThroughGh, saveChatPrefs, scanLocal, setGhLogin,
  type ChatPrefs, type FoundServer,
} from "../core/chat-tools";
import { CLI, CUSTOM_SERVER_KEY, keyAddress, providerDef, urlExposure } from "../core/providers";
import {
  ISLAND_SHORTCUTS, SHORTCUTS, SHORTCUT_TEXT, activeKeys, displayKeys, duplicates, effective,
  recordPress, type Binding,
} from "../core/shortcuts";
import { DEFAULT_SETTINGS, type Settings } from "../core/state";
import { SOUND_NAMES } from "../core/sound";
import {
  MAX_DECLARED, PILL_CATEGORIES, availablePills, chooseMainPill, isComingSoon, mainPillChoices,
  sanitizeDeclared, toggleDeclared, type PillDefinition,
} from "../core/pills";
import { h, clear } from "../views/dom";
import { card, row, field, iconBtn, list, toggle, type ListItem } from "../ui/components";
import { icon } from "../ui/icons";
import { installTooltip } from "../ui/tooltip";
import { featureById, featureCtx, featureInfos, loadFeatures, onFeaturesChanged } from "../core/registry";
import { showOnboarding } from "../features/misc-late/onboarding";
import { privacySection } from "../features/misc-late/privacy";
import { sections } from "../island/sections";
import { agentsSection } from "./agents";
import { sshSection, wslSection } from "./remote";
import { colorDot } from "./colors";
import { renderDiff, statusDot } from "./parts";
import {
  LANGUAGES, N_, isRtl, onLanguageChange, resolveLanguage, setLanguage, systemLanguages, t, tn,
} from "../i18n/i18n";

/** Where secrets.rs keeps the keys on this OS. */
const KEY_STORE = "Windows Credential Manager";

let settings: Settings = { ...DEFAULT_SETTINGS };
let version = "";

const root = document.getElementById("settings-root")!;

async function save() {
  await Bridge.saveSettings(settings);
}

/** A colour was picked for a pill's Mascot (see ./colors.ts): the island follows. */
function pickColor(next: Record<string, string>) {
  settings.pillColors = next;
  void save();
}

// ── Changes to Claude Code's settings.json ────────────────────────────────────

/** One kind of change to ~/.claude/settings.json, with the words that go with it. */
interface Change {
  preview: (install: boolean) => Promise<HookPreview | null>;
  apply: (install: boolean, fingerprint: string) => Promise<string | null>;
  installText: string;
  removeText: string;
  installButton: string;
  removeButton: string;
  /** The note once written; `backup` is "" when there was no file to back up. */
  done: (backup: string) => string;
}

const HOOKS_CHANGE: Change = {
  preview: Bridge.hooksPreview,
  apply: Bridge.hooksApply,
  get installText() { return t("This is exactly what will change in your settings.json. Your own hooks are left untouched."); },
  get removeText() { return t("This removes {app}'s entries only. Your own hooks are left untouched."); },
  get installButton() { return t("Back up and write"); },
  get removeButton() { return t("Back up and remove"); },
  done: (backup) => backup
    ? t("Done. Previous settings saved as {backup}. Open a new Claude Code session to pick the hooks up.", { backup })
    : t("Done. Open a new Claude Code session to pick the hooks up."),
};

/**
 * Starting the app with the agent is written into the hook commands themselves
 * (the relay is told to with `--launch`), so turning it on or off is a change
 * to settings.json like any other: the same diff, backup and click.
 */
const launchChange = (launch: boolean): Change => ({
  ...HOOKS_CHANGE,
  installText: HOOKS_CHANGE.installText,
  preview: (install) => Bridge.hooksPreview(install, launch),
  apply: (install, fingerprint) => Bridge.hooksApply(install, fingerprint, launch),
});

const STATUS_LINE_CHANGE: Change = {
  preview: Bridge.statusLinePreview,
  apply: Bridge.statusLineApply,
  get installText() { return t("This is exactly what will change: only the status line. If you already have one it keeps working, {app}'s relay runs it for you."); },
  get removeText() { return t("This puts your previous status line back, or removes the entry if there was none."); },
  get installButton() { return t("Back up and write"); },
  get removeButton() { return t("Back up and remove"); },
  done: (backup) => backup
    ? t("Done. Previous settings saved as {backup}. The numbers appear after the next reply of a Claude Code session.", { backup })
    : t("Done. The numbers appear after the next reply of a Claude Code session."),
};

/**
 * Shows the diff of a change in `body` and writes it only after an explicit
 * click, and only if settings.json still matches the diff that was shown.
 * `back` redraws the section; `applied` runs a moment after a successful write.
 */
async function reviewChange(
  body: HTMLElement,
  change: Change,
  install: boolean,
  back: () => void,
  applied: () => void,
) {
  let preview;
  try {
    preview = await change.preview(install);
  } catch (err) {
    // An unreadable or invalid settings.json stops here rather than being
    // treated as empty and written over.
    clear(body);
    body.append(
      h("div", { class: "notice err", text: String(err).replace(/^Error:\s*/, "") }),
      h("div", { class: "line" }, h("button", { class: "btn secondary", text: t("Back"), onclick: back })),
    );
    return;
  }
  if (!preview) return;
  clear(body);
  body.append(
    h("div", { class: "hint", text: install ? change.installText : change.removeText }),
    renderDiff(preview.diff),
    h("div", { class: "line" },
      h("span", {
        class: "path",
        text: preview.backup
          ? t("Backup → {path}", { path: preview.backup })
          : t("No settings.json yet — nothing to back up."),
      }),
    ),
  );
  const confirm = h("button", {
    class: install ? "btn primary" : "btn danger",
    text: install ? change.installButton : change.removeButton,
  });
  confirm.addEventListener("click", async () => {
    confirm.disabled = true;
    try {
      const backup = await change.apply(install, preview.fingerprint);
      clear(body);
      body.append(h("div", { class: "notice ok", text: change.done(backup ?? "") }));
      window.setTimeout(applied, 2600);
    } catch (err) {
      confirm.disabled = false;
      body.append(h("div", { class: "notice err", text: t("Could not write: {error}", { error: String(err) }) }));
    }
  });
  body.append(h("div", { class: "line" }, confirm, h("button", { class: "btn secondary", text: t("Cancel"), onclick: back })));
}

// ── Claude Code section ───────────────────────────────────────────────────────

function claudeSection(status: HookStatus): HTMLElement {
  const body = h("div", { style: "display:flex;flex-direction:column;gap:12px" });
  const section = h(
    "section",
    {},
    h("h2", { class: "section-title" }, statusDot(status.installed), h("span", { text: "Claude Code" })),
    body,
  );

  const redraw = () => {
    clear(body);
    draw();
  };
  const rebuild = async () => {
    const fresh = await Bridge.hooksStatus();
    if (fresh) Object.assign(status, fresh);
    redraw();
    const head = section.querySelector("h2")!;
    clear(head);
    head.append(statusDot(status.installed), h("span", { text: "Claude Code" }));
  };

  function draw() {
    body.append(
      h("div", {
        class: "hint",
        text: status.installed
          ? t("{app} is hooked into your Claude Code sessions. Tool calls, questions and permission requests show up in the island, and you can answer them there.")
          : t("Install the hooks to see your Claude Code sessions in the island and approve permissions without leaving what you are doing."),
      }),
      h("div", { class: "line" },
        h("label", { text: "settings.json" }),
        h("span", { class: "path", text: status.settingsPath }),
      ),
      h("div", { class: "line" },
        h("label", { text: t("Relay") }),
        h("span", { class: "path", text: status.hookPath }),
        statusDot(status.hookReady),
      ),
    );

    if (!status.hookReady) {
      body.append(h("div", {
        class: "notice warn",
        text: t("{slug}-hook.exe is not in place yet. Restart {app}; if it still fails, build it with `cargo build -p {slug}-hook`."),
      }));
    }

    const actions = h("div", { class: "line" });
    const install = h("button", {
      class: "btn primary",
      text: status.installed ? t("Reinstall hooks…") : t("Install hooks…"),
      onclick: () => void reviewChange(body, HOOKS_CHANGE, true, redraw, () => void rebuild()),
    });
    // Writing hook commands that point at a relay which isn't there would give
    // every Claude Code session a broken hook and nothing to show for it.
    if (!status.hookReady) {
      install.disabled = true;
      install.title = t("The relay isn't installed yet.");
    }
    actions.append(install);
    if (status.installed) {
      actions.append(h("button", {
        class: "btn danger",
        text: t("Uninstall hooks…"),
        onclick: () => void reviewChange(body, HOOKS_CHANGE, false, redraw, () => void rebuild()),
      }));
    }
    body.append(actions);
    body.append(
      h("div", { class: "line" },
        h("label", { text: t("Show another session's finish for a moment") }),
        toggle(settings.peekFinished, (v) => { settings.peekFinished = v; void save(); }),
      ),
      h("div", { class: "hint", text: t("When a session you are not looking at finishes or fails, its card opens for a few seconds and the pill you were on comes back. Never over an approval or a question.") }),
    );

    // Opt-in, and only with the hooks in place: it is those hooks that start the app.
    if (status.installed && status.hookReady) {
      body.append(
        h("div", { class: "line" },
          h("label", { text: t("Start {app} when Claude Code starts working") }),
          toggle(status.autoLaunch, (on) => void reviewChange(body, launchChange(on), true, redraw, () => void rebuild())),
        ),
        h("div", {
          class: "hint",
          text: t("While {app} is closed, the relay starts it when a Claude Code turn begins, and {app} quits by itself ten minutes after the agents have gone quiet. This rewrites {app}'s own hook commands in settings.json: you see the change before it is written."),
        }),
      );
    }
  }

  draw();
  return section;
}

// ── Plan usage section ────────────────────────────────────────────────────────

/**
 * The 5-hour and weekly limits in the island's header. They come from Claude
 * Code's status line, so the relay has to be the status line first: turning the
 * switch on without it starts the install, and the switch only stays on once
 * that has been confirmed. A status line the user had keeps working.
 */
const PLAN_SETTINGS_TEXT = {
  get claude() { return t("Shows your Claude plan usage (5-hour and weekly limits) in the island's header. {app} adds a status line relay in ~/.claude/settings.json. If you already have a status line, it keeps working as before. Pro and Max plans only."); },
  get desktop() { return t("The numbers come from Claude Code sessions in a terminal. The Claude desktop app does not report them."); },
  get showClaude() { return t("Show in notch"); },
  get codex() { return t("Shows your Codex plan usage (weekly limit and free resets left) in the island's header. {app} asks the Codex CLI (codex app-server) when the pill shows; nothing is installed. Codex must be signed in with ChatGPT."); },
  get showCodex() { return t("Show Codex plan in the notch"); },
};

function planSection(status: HookStatus): HTMLElement {
  const body = h("div", { style: "display:flex;flex-direction:column;gap:12px" });
  const section = h("section", {}, h("h2", { class: "section-title" }, h("span", { text: t("Plan usage") })), body);

  const redraw = () => {
    clear(body);
    draw();
  };
  const rebuild = async () => {
    const fresh = await Bridge.hooksStatus();
    if (fresh) Object.assign(status, fresh);
    settings.planRelayInstalled = status.planRelayInstalled;
    // Cancelled or failed: a switch that was waiting for the install falls back.
    if (!status.planRelayInstalled) settings.showPlanInNotch = false;
    redraw();
  };

  function draw() {
    // The switch shows "on" while the install it asked for is being reviewed.
    const sw = toggle(settings.showPlanInNotch, (on) => {
      if (!on) {
        settings.showPlanInNotch = false;
        void save();
      } else if (status.planRelayInstalled) {
        settings.showPlanInNotch = true;
        void save();
      } else {
        // Turned on before the relay is in: install it first; it stays on once confirmed.
        void reviewChange(body, STATUS_LINE_CHANGE, true, () => void rebuild(), () => {
          settings.planRelayInstalled = true;
          settings.showPlanInNotch = true;
          void save().then(rebuild);
        });
      }
    });
    body.append(
      h("div", {
        class: "hint",
        text: PLAN_SETTINGS_TEXT.claude,
      }),
      h("div", { class: "hint", text: PLAN_SETTINGS_TEXT.desktop }),
      h("div", { class: "line" }, h("label", { text: PLAN_SETTINGS_TEXT.showClaude }), sw),
      h("div", { class: "line" },
        h("label", { text: t("Relay") }),
        statusDot(status.planRelayInstalled),
        h("span", { class: "hint", text: status.planRelayInstalled ? t("installed") : t("not installed") }),
        status.planRelayInstalled
          ? h("button", {
              class: "btn danger",
              text: t("Uninstall relay…"),
              onclick: () => void reviewChange(body, STATUS_LINE_CHANGE, false, redraw, () => void rebuild()),
            })
          : h("button", {
              class: "btn primary",
              text: t("Install relay…"),
              onclick: () => void reviewChange(body, STATUS_LINE_CHANGE, true, redraw, () => void rebuild()),
            }),
      ),
      // Codex: nothing to install, Lintel asks the Codex CLI when the pill shows.
      h("div", { class: "hint", text: PLAN_SETTINGS_TEXT.codex }),
      h("div", { class: "line" },
        h("label", { text: PLAN_SETTINGS_TEXT.showCodex }),
        toggle(settings.showCodexPlanInNotch, (on) => {
          settings.showCodexPlanInNotch = on;
          void save();
        }),
      ),
    );
  }

  draw();
  return section;
}

// ── Claude API section ────────────────────────────────────────────────────────

const MODELS: [string, string][] = [
  ["claude-opus-5", "Claude Opus 5"],
  ["claude-sonnet-5", "Claude Sonnet 5"],
  ["claude-haiku-4-5", "Claude Haiku 4.5"],
];

function apiSection(hasKey: boolean): HTMLElement {
  const dot = statusDot(hasKey);
  const state = h("span", { class: "hint", text: hasKey ? t("Key saved in the {store}.", { store: KEY_STORE }) : t("No key yet — the chat needs one.") });

  const field = h("input", {
    type: "password",
    placeholder: hasKey ? `••••••••••••  ${t("(stored)")}` : "sk-ant-...",
    style: "flex:1 1 auto;min-width:0",
    autocomplete: "off",
    spellcheck: "false",
  }) as HTMLInputElement;

  const saveBtn = h("button", { class: "btn primary", text: t("Save key") });
  const clearBtn = h("button", { class: "btn danger", text: t("Remove") });
  const feedback = h("div", {});

  async function refresh() {
    const present = (await Bridge.secretPresent("anthropic-api-key")) ?? false;
    dot.style.background = present ? "var(--ok)" : "var(--bad)";
    state.textContent = present
      ? t("Key saved in the {store}.", { store: KEY_STORE })
      : t("No key yet — the chat needs one.");
    field.placeholder = present ? `••••••••••••  ${t("(stored)")}` : "sk-ant-...";
    clearBtn.style.display = present ? "" : "none";
  }

  saveBtn.addEventListener("click", async () => {
    const value = field.value.trim();
    if (!value) return;
    clear(feedback);
    try {
      await Bridge.secretSet("anthropic-api-key", value);
      field.value = "";
      feedback.append(h("div", { class: "notice ok", text: t("Saved. It never touches disk.") }));
      await refresh();
    } catch (err) {
      feedback.append(h("div", { class: "notice err", text: t("Could not save: {error}", { error: String(err) }) }));
    }
  });

  clearBtn.addEventListener("click", async () => {
    clear(feedback);
    try {
      await Bridge.secretClear("anthropic-api-key");
      feedback.append(h("div", { class: "notice ok", text: t("Key removed.") }));
      await refresh();
    } catch (err) {
      feedback.append(h("div", { class: "notice err", text: t("Could not remove: {error}", { error: String(err) }) }));
    }
  });

  const model = h("select", {}) as HTMLSelectElement;
  for (const [id, label] of MODELS) model.append(h("option", { value: id, text: label }));
  if (!MODELS.some(([id]) => id === settings.model)) {
    model.append(h("option", { value: settings.model, text: settings.model }));
  }
  model.value = settings.model;
  model.addEventListener("change", () => {
    settings.model = model.value;
    void save();
  });

  clearBtn.style.display = hasKey ? "" : "none";

  return h(
    "section",
    {},
    h("h2", { class: "section-title" }, dot, h("span", { text: "Claude" })),
    state,
    h("div", { class: "line" }, h("label", { text: t("API key") }), field, saveBtn, clearBtn),
    h("div", { class: "line" }, h("label", { text: t("Model") }), model),
    feedback,
  );
}

// ── Gateway and agenda (chat feature's own settings) ──────────────────────────

/** Claude through an Anthropic-compatible gateway, and the opt-in calendar agenda for the chat. */
async function chatExtrasSection(prefs: ChatPrefs): Promise<HTMLElement> {
  const url = h("input", {
    type: "text", placeholder: "https://gateway.example.com", value: prefs.gatewayUrl,
    style: "flex:1 1 auto;min-width:0", spellcheck: "false", autocomplete: "off",
  }) as HTMLInputElement;
  const hasToken = (await Bridge.secretPresent("anthropic-gateway-token")) ?? false;
  const token = h("input", {
    type: "password", placeholder: hasToken ? `••••••••••••  ${t("(stored)")}` : t("Bearer token (optional)"),
    style: "flex:1 1 auto;min-width:0", autocomplete: "off", spellcheck: "false",
  }) as HTMLInputElement;
  const feedback = h("div", {});
  const note = (ok: boolean, text: string) => {
    clear(feedback);
    feedback.append(h("div", { class: ok ? "notice ok" : "notice err", text }));
  };
  const save = h("button", { class: "btn primary", text: t("Save") });
  save.addEventListener("click", async () => {
    const value = url.value.trim();
    // Only https, or http to this computer: the key goes where this points.
    try {
      if (value) {
        const u = new URL(value);
        const local = ["localhost", "127.0.0.1", "[::1]"].includes(u.hostname);
        if (u.protocol !== "https:" && !(u.protocol === "http:" && local)) throw new Error();
      }
    } catch {
      note(false, t("The address must start with https:// (http:// only to this computer)."));
      return;
    }
    try {
      if (token.value.trim()) await Bridge.secretSet("anthropic-gateway-token", token.value.trim());
      else if (!value) await Bridge.secretClear("anthropic-gateway-token");
      token.value = "";
      await saveChatPrefs({ gatewayUrl: value });
      note(true, value ? t("Saved. Claude now goes through this gateway.") : t("Saved. Claude goes to Anthropic."));
    } catch (err) {
      note(false, t("Could not save: {error}", { error: String(err) }));
    }
  });
  return h(
    "section",
    {},
    h("h2", { class: "section-title" }, h("span", { text: t("Claude gateway") })),
    h("div", { class: "hint", text: t("Optional. Send Claude to an Anthropic-compatible gateway instead of Anthropic: its address and a bearer token. Web search is not asked of a gateway. The token stays in the system keychain.") }),
    h("div", { class: "line" }, h("label", { text: t("Address") }), url),
    h("div", { class: "line" }, h("label", { text: t("Token") }), token, save),
    feedback,
    h("h2", { class: "section-title" }, h("span", { text: t("Today's agenda") })),
    h("div", { class: "hint", text: t("Let the chat read today's events from your Calendar. They go to the provider you chat with, with each question, and only while this is on.") }),
    h("div", { class: "line" }, h("label", { text: t("Share today's agenda with the chat") }),
      toggle(prefs.calendarAgenda, (on) => { prefs.calendarAgenda = on; void saveChatPrefs({ calendarAgenda: on }); })),
  );
}

/** MCP servers other apps set up: which the Claude chat may use. Each tool call still asks. */
async function mcpSection(prefs: ChatPrefs): Promise<HTMLElement> {
  const found = await mcpServers();
  const rows = found.map((s) =>
    h("div", { class: "line" },
      h("label", { text: `${s.id} · ${s.source}`, title: s.command }),
      toggle(prefs.mcpServers.includes(s.id), (on) => {
        prefs.mcpServers = on ? [...prefs.mcpServers, s.id] : prefs.mcpServers.filter((x) => x !== s.id);
        void saveChatPrefs({ mcpServers: prefs.mcpServers });
      })));
  return h(
    "section",
    {},
    h("h2", { class: "section-title" }, h("span", { text: t("MCP tools") })),
    h("div", { class: "hint", text: t("MCP servers you set up in Claude Desktop, Cursor, VS Code or Claude Code are listed here (their files are only read). A server you turn on is started when Claude needs its tools and stopped when idle; every call asks for a click first. Claude only.") }),
    ...(rows.length ? rows : [h("div", { class: "hint", text: t("None found.") })]),
  );
}

// ── Active pills section ──────────────────────────────────────────────────────

/**
 * The tools you use (Mac 0.1.1–0.1.2): pick the main workspace tool, which is
 * always on and takes no slot, and declare the agents and chat providers you
 * want as pills. Services are declared in Integrations below, next to their keys.
 */
function activePillsSection(connected: Record<string, boolean>): HTMLElement {
  const slots = h("div", { class: "hint" });
  const main = h("select", {}) as HTMLSelectElement;
  for (const def of mainPillChoices()) main.append(h("option", { value: def.id, text: def.name }));
  main.addEventListener("change", () => {
    const next = chooseMainPill(settings, main.value);
    if (!next) return;
    settings.mainPill = next.mainPill;
    settings.activeIntegrations = next.activeIntegrations;
    declaredChanged();
  });
  const groups = h("div", { style: "display:flex;flex-direction:column;gap:12px" });

  /** Why a pill would show nothing yet, as on the Mac's row. */
  function hint(def: PillDefinition): string | null {
    if (isComingSoon(def.id)) return t("Coming soon");
    if (def.connect.kind === "hooks" && !connected[def.id]) return t("Hooks not installed");
    if (def.connect.kind === "key" && !connected[def.id]) return t("Key not configured");
    if (def.connect.kind === "server" && !settings[def.connect.field]) return t("Not connected");
    return null;
  }

  function row(def: PillDefinition): HTMLElement {
    const isMain = def.id === settings.mainPill;
    const on = settings.activeIntegrations.includes(def.id);
    const full = !isMain && !on && settings.activeIntegrations.length >= MAX_ACTIVE;
    const el = h("div", { class: full ? "pill-row full" : "pill-row" },
      colorDot(def, "width:10px;height:10px", () => settings.pillColors, pickColor),
      h("span", { class: "name", text: def.name }),
    );
    if (isMain) {
      el.append(h("span", { class: "state", text: t("Main") }));
      return el;
    }
    const why = hint(def);
    el.append(h("span", { class: "state", text: why ?? "" }));
    const sw = h("button", { class: on ? "switch on" : "switch" }) as HTMLButtonElement;
    sw.disabled = full;
    sw.addEventListener("click", () => {
      const next = toggleDeclared(settings, def.id);
      if (!next) return;
      settings.activeIntegrations = next;
      declaredChanged();
    });
    el.append(sw);
    return el;
  }

  function draw() {
    const used = settings.activeIntegrations.length;
    slots.textContent = t("{used}/{max} slots in use — the main tool doesn't take one.", { used, max: MAX_ACTIVE });
    slots.classList.toggle("full", used >= MAX_ACTIVE);
    main.value = settings.mainPill;
    clear(groups);
    for (const cat of PILL_CATEGORIES) {
      if (cat.id === "service") continue;
      const pills = availablePills().filter((p) => p.category === cat.id);
      if (pills.length === 0) continue;
      groups.append(h("div", { class: "pill-group" }, h("h3", { text: t(cat.title) }), ...pills.map(row)));
    }
  }
  declaredViews.push(draw);
  draw();

  return h(
    "section",
    {},
    h("h2", { class: "section-title" }, h("span", { text: t("Active pills") })),
    h("div", { class: "hint", text: t("Choose the tools you use. {app} only shows what you declare here.") }),
    slots,
    h("div", { class: "line" }, h("label", { text: t("Main tool") }), main),
    groups,
  );
}

// ── Chat providers section ────────────────────────────────────────────────────

const CHAT_STRINGS = {
  get providersTitle() { return t("Chat providers"); },
  get providersHint() { return t("Chat with Google AI, OpenAI or OpenRouter instead of Claude: add a key here, then click the model name above the chat box to switch provider and model. Keys stay in the system keychain. These providers get no web search and no tools: they can answer, never act on this computer."); },
  get stored() { return `••••••••  ${t("(stored)")}`; },
  get save() { return t("Save"); },
  get remove() { return t("Remove"); },
  get localTitle() { return t("Local models"); },
  get localHint() { return t("Chat with a model you run yourself: Ollama or LM Studio (leave the address empty for the usual one on this computer), or any server that speaks the OpenAI API, such as vLLM or llama.cpp. Once connected, pick it above the chat box."); },
  get connect() { return t("Connect"); },
  get connecting() { return t("Connecting…"); },
  get disconnect() { return t("Disconnect"); },
  get useInChat() { return t("Use in chat"); },
  get inUse() { return t("In use"); },
  get keyOptional() { return t("API key (optional)"); },
  get localOnly() { return t("Nothing leaves your PC: the server runs on this computer."); },
  get remote() { return t("This address is another machine: what you ask is sent to it."); },
  get remoteHttp() { return t("This address is another machine, over plain http: what you ask travels unencrypted."); },
  get keyOverHttp() { return t("Warning: the key would be sent unencrypted (http://) to another machine. Use https://, or a server on this computer."); },
  get invalid() { return t("Not a valid http:// or https:// address."); },
  noModels: (name: string) => t("No models yet. Download one in {name} first.", { name }),
  models: (n: number) => tn("{count} model", "{count} models", n),
  get freeOnly() { return t("Free models only: nothing that costs money is listed or asked for."); },
  get found() { return t("Found on this computer"); },
  get scanning() { return t("Looking for model servers on this computer…"); },
  get noneFound() { return t("No model server is running on this computer right now."); },
  get scanAgain() { return t("Scan again"); },
  get cliTitle() { return t("Agent CLIs"); },
  get cliHint() { return t("Chat through an agent CLI you installed and signed in to, on your own plan. {app} starts it once per message, with its tools off, and reads its answer. Pick it under CLI above the chat box. The ones turned on here take over, in this order, when the first one fails."); },
  get notInstalled() { return t("Not installed"); },
  get moveUp() { return t("Move up"); },
};

interface CloudDef {
  id: "google" | "openai" | "openrouter";
  name: string;
  placeholder: string;
  where: string;
}

const CLOUD: CloudDef[] = [
  { id: "google", name: "Google AI", placeholder: "AIza…", where: "aistudio.google.com" },
  { id: "openai", name: "OpenAI", placeholder: "sk-…", where: "platform.openai.com" },
  { id: "openrouter", name: "OpenRouter", placeholder: "sk-or-…", where: "openrouter.ai/keys" },
];

function chatProvidersSection(
  present: Record<string, boolean>,
  keyChanged: (key: string, on: boolean) => void,
  prefs: ChatPrefs,
): HTMLElement {
  const list = h("div", { style: "display:flex;flex-direction:column;gap:8px" });
  for (const def of CLOUD) {
    const p = providerDef(def.id);
    const key = p.key!;
    const input = h("input", {
      type: "password",
      placeholder: present[key] ? CHAT_STRINGS.stored : def.placeholder,
      autocomplete: "off",
      spellcheck: "false",
      style: "flex:1 1 auto;min-width:0",
    }) as HTMLInputElement;
    const dotEl = statusDot(present[key] ?? false);
    const saveBtn = h("button", { class: "btn secondary", text: CHAT_STRINGS.save });
    const removeBtn = h("button", { class: "btn danger", text: CHAT_STRINGS.remove });
    const refresh = () => {
      input.placeholder = present[key] ? CHAT_STRINGS.stored : def.placeholder;
      dotEl.style.background = present[key] ? "var(--ok)" : "var(--bad)";
      removeBtn.style.display = present[key] ? "" : "none";
    };
    saveBtn.addEventListener("click", async () => {
      const value = input.value.trim();
      if (!value) return;
      try {
        await Bridge.secretSet(key, value);
        present[key] = true;
        input.value = "";
        keyChanged(key, true);
      } catch {
        dotEl.style.background = "var(--warn)";
        return;
      }
      refresh();
    });
    removeBtn.addEventListener("click", async () => {
      try {
        await Bridge.secretClear(key);
        present[key] = false;
        keyChanged(key, false);
      } catch {
        dotEl.style.background = "var(--warn)";
        return;
      }
      refresh();
    });
    refresh();
    list.append(
      h("div", { class: "line" },
        h("label", {},
          h("i", { class: "dot", style: `background:${p.accent};margin-right:8px` }),
          h("span", { text: def.name }),
        ),
        input, saveBtn, removeBtn, dotEl,
      ),
      h("div", { class: "hint", style: "margin:-4px 0 0 144px", text: t("Key from {site}", { site: def.where }) }),
    );
    if (def.id === "openrouter") {
      // Free models only: the chat goes to the free provider, with the same key.
      const free = toggle(prefs.openrouterFree, (on) => {
        prefs.openrouterFree = on;
        void saveChatPrefs({ openrouterFree: on });
        const from = on ? "openrouter" : "openrouter-free";
        if (settings.chatProvider === from) {
          settings.chatProvider = on ? "openrouter-free" : "openrouter";
          void save();
        }
      });
      list.append(
        h("div", { class: "line", style: "margin-left:144px;flex-wrap:nowrap" }, free, h("span", { class: "hint", text: CHAT_STRINGS.freeOnly })),
      );
    }
  }
  return h(
    "section",
    {},
    h("h2", { class: "section-title" }, h("span", { text: CHAT_STRINGS.providersTitle })),
    h("div", { class: "hint", text: CHAT_STRINGS.providersHint }),
    list,
  );
}

// ── Local models section ──────────────────────────────────────────────────────

type LocalId = "ollama" | "lmstudio" | "custom";

/** Redraws the local models section after a change made elsewhere (the island). */
let localRedraw: (() => void) | null = null;

const LOCAL: Record<LocalId, { name: string; usual: string }> = {
  ollama: { name: "Ollama", usual: "http://127.0.0.1:11434" },
  lmstudio: { name: "LM Studio", usual: "http://127.0.0.1:1234" },
  // No usual address: any server that speaks the OpenAI API.
  custom: { name: N_("OpenAI-compatible"), usual: "" },
};

/** What an address means for the user's data, as a hint line. */
function exposureNotice(url: string, withKey: boolean): HTMLElement | null {
  switch (urlExposure(url)) {
    case "local":
      return h("div", { class: "hint", text: CHAT_STRINGS.localOnly });
    case "remote":
      return h("div", { class: "hint", text: CHAT_STRINGS.remote });
    case "remote-http":
      return withKey
        ? h("div", { class: "notice warn", text: CHAT_STRINGS.keyOverHttp })
        : h("div", { class: "hint", text: CHAT_STRINGS.remoteHttp });
    case "invalid":
      return url.trim() ? h("div", { class: "notice err", text: CHAT_STRINGS.invalid }) : null;
  }
}

function localSection(customKey: boolean): HTMLElement {
  const body = h("div", { style: "display:flex;flex-direction:column;gap:14px" });
  // The servers already running on this computer, to connect with one click
  // and no key. Looked for when this page opens and on "Scan again".
  const foundBox = h("div", { style: "display:flex;flex-direction:column;gap:6px" });
  const section = h(
    "section",
    {},
    h("h2", { class: "section-title" }, h("span", { text: CHAT_STRINGS.localTitle })),
    h("div", { class: "hint", text: CHAT_STRINGS.localHint }),
    foundBox,
    body,
  );
  let found: FoundServer[] | null = null;

  const drawFound = () => {
    clear(foundBox);
    const again = h("button", { class: "btn secondary", text: CHAT_STRINGS.scanAgain, onclick: () => void scan() });
    const head = (text: string) => h("div", { class: "line" }, h("span", { class: "hint", text }), h("span", { class: "spacer" }), again);
    if (found == null) {
      again.disabled = true;
      foundBox.append(head(CHAT_STRINGS.scanning));
      return;
    }
    // One that is already connected is in the list below.
    const fresh = found.filter((server) => settings[providerDef(server.id).urlField!] !== server.url);
    foundBox.append(head(fresh.length > 0 ? CHAT_STRINGS.found : found.length > 0 ? "" : CHAT_STRINGS.noneFound));
    for (const server of fresh) {
      const p = providerDef(server.id);
      const connect = h("button", { class: "btn primary", text: CHAT_STRINGS.connect });
      connect.disabled = server.models.length === 0;
      connect.addEventListener("click", async () => {
        settings[p.urlField!] = server.url;
        if (!server.models.includes(settings.chatModels[server.id] ?? "")) {
          settings.chatModels = { ...settings.chatModels, [server.id]: server.models[0] };
        }
        await save();
        redraw();
        drawFound();
      });
      foundBox.append(
        h("div", { class: "line" },
          h("label", {},
            h("i", { class: "dot", style: `background:${p.accent};margin-right:8px` }),
            h("span", { text: server.name }),
          ),
          h("span", { class: "path", text: server.url }),
          h("span", { class: "hint", text: server.models.length ? CHAT_STRINGS.models(server.models.length) : CHAT_STRINGS.noModels(server.name) }),
          h("span", { class: "spacer" }),
          connect,
        ),
      );
    }
  };

  async function scan() {
    found = null;
    drawFound();
    found = await scanLocal();
    drawFound();
  }

  const redraw = () => {
    clear(body);
    for (const id of Object.keys(LOCAL) as LocalId[]) body.append(serverBlock(id));
  };

  function serverBlock(id: LocalId): HTMLElement {
    const def = LOCAL[id];
    const p = providerDef(id);
    const field = p.urlField!;
    const connected = settings[field] !== "";
    const status = h("div", {});
    const exposure = h("div", {});
    const label = h("label", {},
      h("i", { class: "dot", style: `background:${p.accent};margin-right:8px` }),
      h("span", { text: t(def.name) }),
    );
    const block = h("div", { style: "display:flex;flex-direction:column;gap:6px" });

    if (connected) {
      const inUse = settings.chatProvider === id;
      const use = h("button", { class: inUse ? "btn secondary" : "btn primary", text: inUse ? CHAT_STRINGS.inUse : CHAT_STRINGS.useInChat });
      use.disabled = inUse;
      use.addEventListener("click", () => {
        settings.chatProvider = id;
        void save().then(redraw);
      });
      const disconnect = h("button", { class: "btn danger", text: CHAT_STRINGS.disconnect });
      disconnect.addEventListener("click", async () => {
        settings[field] = "";
        if (settings.chatProvider === id) settings.chatProvider = "anthropic";
        if (id === "custom") {
          await Bridge.secretClear(CUSTOM_SERVER_KEY).catch(() => {});
          customKey = false;
        }
        await save();
        redraw();
      });
      block.append(
        h("div", { class: "line" }, label, h("span", { class: "path", text: settings[field] }), statusDot(true), use, disconnect),
        status,
      );
      exposure.append(exposureNotice(settings[field], id === "custom" && customKey) ?? "");
      block.append(exposure);
      return block;
    }

    const input = h("input", {
      type: "text",
      placeholder: def.usual || "https://llm.example.com",
      style: "flex:1 1 auto;min-width:0",
      spellcheck: "false",
      autocomplete: "off",
    }) as HTMLInputElement;
    // A custom server may want a key; it goes to the keychain, never to settings.json.
    const key = h("input", {
      type: "password",
      placeholder: customKey ? CHAT_STRINGS.stored : CHAT_STRINGS.keyOptional,
      style: "flex:1 1 auto;min-width:0",
      autocomplete: "off",
      spellcheck: "false",
    }) as HTMLInputElement;
    const connect = h("button", { class: "btn primary", text: CHAT_STRINGS.connect });

    const showExposure = () => {
      clear(exposure);
      const withKey = id === "custom" && (customKey || key.value.trim() !== "");
      const notice = exposureNotice(input.value || def.usual, withKey);
      if (notice) exposure.append(notice);
    };
    input.addEventListener("input", showExposure);
    key.addEventListener("input", showExposure);

    connect.addEventListener("click", async () => {
      connect.disabled = true;
      clear(status);
      status.append(h("div", { class: "hint", text: CHAT_STRINGS.connecting }));
      try {
        if (id === "custom" && key.value.trim()) {
          // Stored with this address: the key is only ever sent there.
          await Bridge.localSetKey(input.value || def.usual, key.value.trim());
          key.value = "";
          customKey = true;
        }
        const server = await Bridge.localConnect(id, input.value);
        if (!server.models.length) {
          clear(status);
          status.append(h("div", { class: "notice err", text: CHAT_STRINGS.noModels(t(def.name)) }));
        } else {
          settings[field] = server.url;
          if (!server.models.includes(settings.chatModels[id] ?? "")) {
            settings.chatModels = { ...settings.chatModels, [id]: server.models[0] };
          }
          await save();
          redraw();
          return;
        }
      } catch (err) {
        clear(status);
        status.append(h("div", { class: "notice err", text: String(err).replace(/^Error:\s*/, "") }));
      }
      connect.disabled = false;
    });

    block.append(h("div", { class: "line" }, label, input, connect));
    if (id === "custom") block.append(h("div", { class: "line" }, h("label", { text: "" }), key));
    block.append(status, exposure);
    showExposure();
    return block;
  }

  redraw();
  localRedraw = redraw;
  void scan();
  return section;
}

// ── Agent CLIs section ────────────────────────────────────────────────────────

/**
 * Chat through an installed CLI (src-tauri/src/chat/cli.rs): which ones are
 * here, and which take over, in which order, when the one picked in the chat
 * fails. Looking for them reads file names only; nothing is started.
 */
function cliSection(prefs: ChatPrefs, installed: Set<string>): HTMLElement {
  const rows = h("div", { style: "display:flex;flex-direction:column;gap:2px" });

  const setOrder = (order: string[]) => {
    prefs.cliOrder = order;
    void saveChatPrefs({ cliOrder: order });
    draw();
  };

  function draw() {
    clear(rows);
    const on = prefs.cliOrder.filter((id) => installed.has(id));
    const rest = KNOWN_CLIS.filter((cli) => !on.includes(cli.id));
    const ordered = [...on.map((id) => KNOWN_CLIS.find((cli) => cli.id === id)!), ...rest];
    for (const cli of ordered) {
      const here = installed.has(cli.id);
      const at = on.indexOf(cli.id);
      const line = h("div", { class: here ? "pill-row" : "pill-row full" },
        h("i", { class: "dot", style: `background:${here ? CLI.accent : "var(--dim-2)"}` }),
        h("span", { class: "name", text: cli.name }),
        h("span", { class: "state", text: here ? (at >= 0 ? String(at + 1) : "") : CHAT_STRINGS.notInstalled }),
      );
      if (at > 0) {
        line.append(iconBtn("chevu", CHAT_STRINGS.moveUp, () => {
          const next = [...on];
          [next[at - 1], next[at]] = [next[at], next[at - 1]];
          setOrder(next);
        }));
      }
      if (here) {
        line.append(toggle(at >= 0, (take) => setOrder(take ? [...on, cli.id] : on.filter((id) => id !== cli.id))));
      }
      rows.append(line);
    }
  }
  draw();

  return h(
    "section",
    {},
    h("h2", { class: "section-title" }, h("span", { text: CHAT_STRINGS.cliTitle })),
    h("div", { class: "hint", text: CHAT_STRINGS.cliHint }),
    rows,
  );
}

// ── Integrations section ──────────────────────────────────────────────────────

interface IntegrationDef {
  id: string;
  name: string;
  color: string;
  /** Credential Manager keys, in the order they are shown. */
  fields: { key: string; label: string; placeholder: string; secret: boolean }[];
  /** What the key needs, shown under its field. */
  hint?: string;
}

const INTEGRATIONS: IntegrationDef[] = [
  { id: "integration_stripe", name: "Stripe", color: "#0570DE",
    fields: [{ key: "stripe-api-key", label: N_("Secret key"), placeholder: "sk_live_…", secret: true }] },
  { id: "integration_github", name: "GitHub", color: "#F4505E",
    fields: [
      { key: "github-token", label: N_("Token"), placeholder: "ghp_…", secret: true },
      { key: "forge-url", label: N_("GitLab or Gitea"), placeholder: "https://gitlab.example.com", secret: false },
      { key: "forge-token", label: N_("Its token"), placeholder: "…", secret: true },
    ],
    hint: N_("Classic token with the repo scope, or fine-grained with read access to Pull requests, Commit statuses and Actions. A GitLab or Gitea address and token (https only) add its merge requests, reviews and pipelines to the same card.") },
  { id: "integration_vercel", name: "Vercel", color: "#7C5CFF",
    fields: [{ key: "vercel-token", label: N_("Token"), placeholder: "…", secret: true }] },
  { id: "integration_n8n", name: "n8n", color: "#F29B38",
    fields: [
      { key: "n8n-url", label: N_("Instance URL"), placeholder: "https://n8n.example.com", secret: false },
      { key: "n8n-api-key", label: N_("API key"), placeholder: "…", secret: true },
    ] },
  { id: "integration_resend", name: "Resend", color: "#22C55E",
    fields: [{ key: "resend-api-key", label: N_("API key"), placeholder: "re_…", secret: true }] },
  { id: "integration_notion", name: "Notion", color: "#8C8C8C",
    fields: [{ key: "notion-api-key", label: N_("Integration token"), placeholder: "ntn_…", secret: true }] },
  { id: "integration_calcom", name: "Cal.com", color: "#C9956A",
    fields: [{ key: "calcom-api-key", label: N_("API key"), placeholder: "cal_…", secret: true }] },
  { id: "integration_linear", name: "Linear", color: "#5E6AD2",
    fields: [{ key: "linear-api-key", label: N_("API key"), placeholder: "lin_api_…", secret: true }] },
];

const MAX_ACTIVE = MAX_DECLARED;

/** Everything that shows the declared pills, redrawn when any of them changes. */
const declaredViews: (() => void)[] = [];

function declaredChanged() {
  for (const redraw of declaredViews) redraw();
  void save();
}

/**
 * Under the n8n address field: what the app will refuse (the key does not go
 * over plain http to another machine), said while typing rather than on the
 * card at the next poll. Stays shown after Save, which empties the field.
 */
function n8nUrlNotice(input: HTMLInputElement): HTMLElement {
  // Hidden while empty: even with no height it would count in the column's gap
  // and push the address and key rows apart.
  const notice = h("div", {});
  notice.hidden = true;
  input.addEventListener("input", () => {
    clear(notice);
    const kind = input.value.trim() ? keyAddress(input.value) : "ok";
    if (kind === "remote-http") {
      notice.append(h("div", { class: "notice warn", text: t("A key is only sent over https, or to a server on this computer.") }));
    } else if (kind === "invalid") {
      notice.append(h("div", { class: "notice err", text: CHAT_STRINGS.invalid }));
    }
    notice.hidden = !notice.firstChild;
  });
  return notice;
}

/**
 * GitHub without a token: the login of the local GitHub CLI, read when the
 * card opens and never stored (src-tauri/src/github.rs). Turning it on asks
 * once, to say whose login it is, or why there is none.
 */
function ghLoginRow(): HTMLElement {
  const state = h("div", { class: "hint" });
  const check = async (on: boolean) => {
    state.textContent = "";
    if (!on) return;
    try {
      const answer = await pulseThroughGh();
      state.textContent = answer
        ? t("Using the gh login as {login}.", { login: answer.pulse.login })
        : t("The saved token is used first.");
    } catch (err) {
      state.textContent = String(err).replace(/^Error:\s*/, "");
    }
  };
  const sw = toggle(false, (on) => void setGhLogin(on).then(() => check(on)));
  void ghLoginOn().then((on) => sw.setAttribute("aria-checked", String(on)));
  return h("div", { style: "display:flex;flex-direction:column;gap:6px;padding-top:4px" },
    h("div", { class: "line", style: "flex-wrap:nowrap" },
      sw,
      h("span", { style: "font-size:12.5px", text: t("Use the gh login") }),
    ),
    h("div", { class: "hint", text: t("With no token saved, the pull requests are fetched with the GitHub CLI's login each time the card opens. The login is read at that moment and never stored.") }),
    state,
  );
}

/**
 * Developer alerts: notices for a failing CI, a review asked of you, a pull
 * request waiting on you, and (opt-in) a Vercel deploy turning Ready or Failed.
 * A watch list of up to 20 repositories and Vercel projects limits them to those
 * (empty: your own activity). Kept with the integrations feature; the pollers
 * read it (src-tauri/src/github.rs).
 */
const WATCH_MAX = 20;

function devAlertsBlock(): HTMLElement {
  const patch = (value: object) => void Bridge.featurePatchSettings("integrations", value);
  const names = (raw: string) =>
    [...new Set(raw.split(/[\s,;]+/).filter(Boolean))].slice(0, WATCH_MAX);
  const watchRow = (label: string, hint: string, placeholder: string, key: "watchRepos" | "watchProjects") => {
    const input = h("input", {
      type: "text", placeholder, autocomplete: "off", spellcheck: "false", style: "flex:1 1 auto;min-width:0",
    }) as HTMLInputElement;
    const save = () => {
      const list = names(input.value);
      input.value = list.join(", ");
      patch({ [key]: list });
    };
    input.addEventListener("change", save);
    void Bridge.featureSettings("integrations").then((stored) => {
      const list = stored?.[key];
      if (Array.isArray(list)) input.value = list.filter((x) => typeof x === "string").join(", ");
    });
    return h("div", { style: "display:flex;flex-direction:column;gap:4px" },
      h("div", { class: "line" }, h("label", { style: "min-width:104px", text: label }), input),
      h("div", { class: "hint", text: hint }),
    );
  };
  const vercel = toggle(false, (on) => patch({ vercelAlerts: on }));
  void Bridge.featureSettings("integrations").then((stored) =>
    vercel.setAttribute("aria-checked", String(stored?.vercelAlerts === true)));
  return h("div", { style: "display:flex;flex-direction:column;gap:12px;margin-top:18px" },
    h("h3", { class: "section-title", text: t("Developer alerts") }),
    h("div", { class: "hint", text: t("A failing CI, a review asked of you or a pull request waiting on you appear as a notice with the GitHub pill on. Only what happens after the app starts is announced.") }),
    h("div", { class: "line", style: "flex-wrap:nowrap" },
      vercel,
      h("span", { style: "font-size:12.5px", text: t("Alert when a Vercel deploy is Ready or Failed") }),
    ),
    watchRow(t("Repositories"), t("Watch list, up to 20, owner/name separated by commas. Empty: your own activity."), "owner/name, owner/other", "watchRepos"),
    watchRow(t("Vercel projects"), t("Watch list, up to 20 project names. Empty: all."), "my-site, my-app", "watchProjects"),
  );
}

function integrationsSection(present: Record<string, boolean>): HTMLElement {
  const note = h("div", { class: "hint" });
  const list = h("div", { style: "display:flex;flex-direction:column;gap:14px" });

  function updateNote() {
    const used = settings.activeIntegrations.length;
    note.textContent = t("Pick up to {max} pills to show next to the mascot — {used}/{max} in use. Keys are stored in the {store}, never on disk.", { max: MAX_ACTIVE, used, store: KEY_STORE });
  }
  declaredViews.push(updateNote);

  const offered = new Set(availablePills().map((p) => p.id));
  for (const def of INTEGRATIONS.filter((d) => offered.has(d.id))) {
    const active = settings.activeIntegrations.includes(def.id);
    const sw = h("button", { class: active ? "switch on" : "switch" });
    sw.addEventListener("click", () => {
      const on = settings.activeIntegrations.includes(def.id);
      if (on) {
        settings.activeIntegrations = settings.activeIntegrations.filter((x) => x !== def.id);
      } else {
        if (settings.activeIntegrations.length >= MAX_ACTIVE) return;
        settings.activeIntegrations = [...settings.activeIntegrations, def.id];
      }
      sw.classList.toggle("on", !on);
      declaredChanged();
    });

    const rows = h("div", { style: "display:flex;flex-direction:column;gap:6px;flex:1 1 auto;min-width:0" });
    for (const field of def.fields) {
      const input = h("input", {
        type: field.secret ? "password" : "text",
        placeholder: present[field.key] ? CHAT_STRINGS.stored : field.placeholder,
        autocomplete: "off",
        spellcheck: "false",
        style: "flex:1 1 auto;min-width:0",
      }) as HTMLInputElement;
      const saveBtn = h("button", { class: "btn secondary", text: t("Save") });
      const dotEl = statusDot(present[field.key] ?? false);
      saveBtn.addEventListener("click", async () => {
        const value = input.value.trim();
        try {
          await Bridge.secretSet(field.key, value);
          present[field.key] = value.length > 0;
          input.value = "";
          input.placeholder = value ? CHAT_STRINGS.stored : field.placeholder;
          dotEl.style.background = value ? "var(--ok)" : "var(--bad)";
        } catch {
          dotEl.style.background = "var(--warn)";
        }
      });
      rows.append(
        h("div", { class: "line" },
          h("label", { style: "min-width:104px", text: t(field.label) }),
          input, saveBtn, dotEl,
        ),
      );
      if (field.key === "n8n-url") rows.append(n8nUrlNotice(input));
    }

    if (def.hint) rows.append(h("div", { class: "hint", text: t(def.hint) }));
    if (def.id === "integration_github") rows.append(ghLoginRow());

    list.append(
      h("div", { style: "display:flex;gap:12px;align-items:flex-start" },
        h("div", { style: "display:flex;align-items:center;gap:8px;min-width:132px;padding-top:4px" },
          sw,
          colorDot(def, "", () => settings.pillColors, pickColor),
          h("span", { style: "font-size:12.5px", text: def.name }),
        ),
        rows,
      ),
    );
  }

  updateNote();
  return h("section", {}, h("h2", { class: "section-title" }, h("span", { text: t("Integrations") })), note, list, devAlertsBlock());
}

// ── General, Island, Mascot, Sounds ───────────────────────────────────────────

/** A select in the design language's field. */
function selectField(select: HTMLSelectElement): HTMLElement {
  return h("label", { class: "field" }, select, icon("chevd", 14));
}

function generalPage(): HTMLElement[] {
  return [
    card([
      languageRow(),
      row(t("Launch at startup"), toggle(settings.autostart, (v) => { settings.autostart = v; void save(); })),
    ]),
    // About: the license has to be stated where the user can read it. English only for now.
    card([
      row(
        `${BRAND.name} ${version}`,
        h("span", {}),
        "Free software under the GNU General Public License, version 3 or later (GPL-3.0-or-later). It comes with no warranty. The licenses of the parts it includes are in NOTICE.md and the licenses folder, installed beside the app.",
      ),
    ]),
  ];
}

function islandPage(): HTMLElement[] {
  const autoClose = field({ type: "number", value: String(Math.round(settings.autoCloseInterval)) });
  Object.assign(autoClose.input, { min: "5", max: "120", step: "1" });
  autoClose.style.width = "88px";
  autoClose.input.addEventListener("change", () => {
    settings.autoCloseInterval = Math.max(5, Math.min(120, Number(autoClose.input.value) || 15));
    autoClose.input.value = String(settings.autoCloseInterval);
    void save();
  });

  const screen = h("select", {}) as HTMLSelectElement;
  screen.append(
    h("option", { value: "primary", text: t("Main display") }),
    h("option", { value: "cursor", text: t("Display under the cursor") }),
  );
  screen.value = settings.screen;
  void Bridge.listMonitors().then((list) => {
    for (const m of list ?? []) screen.append(h("option", { value: m.key, text: m.label }));
    // Set again now the option exists. A display saved under an older key (moved,
    // resized, or saved before names were kept) is shown by its place or its
    // name; one that is gone shows as the main one.
    const saved = settings.screen;
    const [place, name] = saved.split("|");
    const keys = (list ?? []).map((m) => m.key);
    screen.value =
      keys.find((k) => k === saved) ??
      (saved.startsWith("at:") ? keys.find((k) => k.split("|")[0] === place) : undefined) ??
      (name ? keys.find((k) => k.split("|")[1] === name) : undefined) ??
      saved;
    if (!screen.value) screen.value = "primary";
  });
  screen.addEventListener("change", () => {
    settings.screen = screen.value;
    // Choosing where it lives puts a dragged island back there.
    settings.islandSpot = null;
    void save();
  });

  // The tab the island opens on: the home view, or a section of a feature that is on.
  const openOn = h("select", {}, h("option", { value: "", text: t("Home") }));
  for (const s of sections()) openOn.append(h("option", { value: s.key, text: t(s.def.title) }));
  openOn.value = settings.openOn;
  if (openOn.value !== settings.openOn) openOn.value = "";
  openOn.addEventListener("change", () => {
    settings.openOn = openOn.value;
    void save();
  });

  // The Dashboard's cards: both, one of the two, or no Dashboard at all.
  const homeCards = h("select", {},
    h("option", { value: "both", text: t("Both cards") }),
    h("option", { value: "now", text: t("Player and agents") }),
    h("option", { value: "levels", text: t("Levels and next event") }),
    h("option", { value: "off", text: t("Off") }),
  );
  homeCards.value = settings.homeCards;
  homeCards.addEventListener("change", () => {
    settings.homeCards = homeCards.value as Settings["homeCards"];
    void save();
  });

  // How long the strip keeps out once it is out.
  const peekStay = h("select", {},
    h("option", { value: "moment", text: t("For a moment") }),
    h("option", { value: "agent", text: t("While an agent is working") }),
    h("option", { value: "always", text: t("Until I dismiss it") }),
  );
  peekStay.value = settings.peekStay;
  if (!peekStay.value) peekStay.value = "moment";
  peekStay.addEventListener("change", () => {
    settings.peekStay = peekStay.value as Settings["peekStay"];
    void save();
  });

  return [
    h("div", { class: "section-title", text: t("Behavior") }),
    card([
      row(t("Open on"), selectField(openOn), t("The tab the island opens on when the strip shows nothing of its own.")),
      row(t("Dashboard cards"), selectField(homeCards), t("The Dashboard is the island's page of smart cards: each shows what matters now, and its dots change the face.")),
      row(
        t("Open on hover"),
        toggle(settings.openOnHover, (v) => { settings.openOnHover = v; void save(); }),
        t("Resting the pointer on the strip opens the island; it folds again shortly after the pointer leaves. Click inside to keep it open."),
      ),
      row(t("Auto-close"), autoClose, t("seconds after you leave the island")),
      row(
        t("The small island stays"),
        selectField(peekStay),
        t("Once the strip is out. To put it away: Esc with the pointer on it, the arrow at its right end, or the island's shortcut."),
      ),
      row(
        t("Keep live activities on screen"),
        toggle(settings.keepActivities, (v) => { settings.keepActivities = v; void save(); }),
        t("A running timer or a playing track holds the strip instead of showing it for a few seconds."),
      ),
      row(
        t("Wheel on the strip"),
        toggle(settings.wheelGesture, (v) => { settings.wheelGesture = v; void save(); }),
        t("Wheel down opens the island, wheel up puts it away."),
      ),
      row(
        t("Keyboard jumps"),
        toggle(settings.keyJumps, (v) => { settings.keyJumps = v; void save(); }),
        t("Ctrl+K opens the gallery and Alt plus a letter opens a section."),
      ),
      row(
        t("Show percentage"),
        toggle(settings.levelPercent, (v) => { settings.levelPercent = v; void save(); }),
        t("A volume or brightness notice shows its level as a number beside the bar."),
      ),
      row(
        t("Enlarge cards on hover"),
        toggle(settings.hoverEnlarge, (v) => { settings.hoverEnlarge = v; void save(); }),
        t("A card under the pointer grows a little. A click on text that a card cut short shows all of it either way."),
      ),
      row(
        t("Clock on the strip"),
        toggle(settings.stripClock, (v) => { settings.stripClock = v; void save(); }),
        t("The time and the day on the resting strip."),
      ),
      row(
        t("Agents and plan limits on the strip"),
        toggle(settings.stripStats, (v) => { settings.stripStats = v; void save(); }),
        t("How many agents are at work, and the 5-hour use of each plan, on the resting strip."),
      ),
    ]),
    h("div", { class: "section-title", text: t("Appearance") }),
    card([
      row(t("Island lives on"), selectField(screen)),
      row(
        t(LET_DRAG),
        toggle(settings.islandDrag, (v) => { settings.islandDrag = v; void save(); }),
        t(LET_DRAG_HINT),
      ),
      row(
        t(PUT_BACK),
        h("button", { class: "btn secondary", text: t(PUT_BACK_DO), onclick: () => { settings.islandSpot = null; void save(); } }),
        t(PUT_BACK_HINT),
      ),
      row(
        t("Island outline"),
        toggle(settings.islandOutline, (v) => { settings.islandOutline = v; void save(); }),
        t("A thin edge that keeps the island visible on dark wallpapers."),
      ),
    ]),
  ];
}

// debt: English only until the next translation task (constants, so the completeness test does not ask for nine more languages).
const LET_DRAG = "Let me drag the island";
const LET_DRAG_HINT = "Drag the strip, or the empty part of the open island's top bar. Near the top edge of a display it docks; anywhere else it floats.";
const PUT_BACK = "Put the island back";
const PUT_BACK_DO = "Put back";
const PUT_BACK_HINT = "To the top center of the display it lives on. The tray menu has this too.";

// debt: English only until the next translation task (constants, as above).
const TRAY_ANIMATE = "Animate the tray icon";
const TRAY_ANIMATE_HINT = "The mascot beside the clock does what the island's mascot does. Off shows one still picture.";
const ICON_SHORTCUTS = `Use this colour for ${BRAND.name}'s shortcuts too`;
const ICON_SHORTCUTS_HINT = `The tray, the taskbar and ${BRAND.name}'s own Desktop and Start menu shortcuts wear the colour of the VS Code pill (Pills). Off leaves the shortcuts with the icon the program came with.`;

function mascotPage(): HTMLElement[] {
  return [
    card([
      row(
        t("Show the mascot"),
        toggle(settings.showMascot, (v) => { settings.showMascot = v; void save(); }),
        t("Off shows a small dot in the pill colour instead."),
      ),
      row(TRAY_ANIMATE, toggle(settings.trayAnimate !== false, (v) => { settings.trayAnimate = v; void save(); }), TRAY_ANIMATE_HINT),
      row(ICON_SHORTCUTS, toggle(settings.iconShortcuts !== false, (v) => { settings.iconShortcuts = v; void save(); }), ICON_SHORTCUTS_HINT),
    ]),
  ];
}

function soundsPage(): HTMLElement[] {
  const volume = h("input", {
    type: "range", min: "0", max: "0.5", step: "0.01",
    value: String(settings.soundVolume),
  }) as HTMLInputElement;
  volume.addEventListener("input", () => {
    settings.soundVolume = Number(volume.value);
    void save();
  });

  // Your own sounds: the folder, a reload, and how many are replaced.
  const customCount = h("span", { class: "hint" });
  const countCustom = () =>
    void Bridge.customSounds(SOUND_NAMES).then((own) => {
      const n = own?.length ?? 0;
      customCount.textContent = n > 0 ? t("{0} custom", { 0: n }) : "";
    });
  countCustom();
  const soundsFolder = h("button", { class: "btn secondary", text: t("Open sounds folder"), onclick: () => void Bridge.revealSoundsFolder() });
  const reloadSounds = h("button", {
    class: "btn secondary",
    text: t("Reload sounds"),
    onclick: () => void Bridge.reloadSounds().then(() => window.setTimeout(countCustom, 300)),
  });
  // Your own file for any one sound, or back to the built-in one.
  const which = h("select", {}) as HTMLSelectElement;
  for (const n of SOUND_NAMES) which.append(h("option", { value: n, text: n }));
  const picker = h("input", { type: "file", accept: "audio/*", style: "display:none" }) as HTMLInputElement;
  picker.addEventListener("change", async () => {
    const f = picker.files?.[0];
    picker.value = "";
    if (!f) return;
    const ext = f.name.split(".").pop() ?? "";
    const ok = await Bridge.setCustomSound(which.value, ext, new Uint8Array(await f.arrayBuffer()));
    customCount.textContent = ok ? "" : t("That file can't be used (audio, up to 8 MB).");
    if (ok) void Bridge.reloadSounds().then(() => window.setTimeout(countCustom, 300));
  });
  const chooseFile = h("button", { class: "btn secondary", text: t("Choose file…"), onclick: () => picker.click() });
  const resetSound = h("button", {
    class: "btn secondary",
    text: t("Reset"),
    onclick: () => void Bridge.resetCustomSound(which.value).then(() => Bridge.reloadSounds()).then(() => window.setTimeout(countCustom, 300)),
  });
  return [
    card([
      row(
        t("Sound"),
        h("span", { class: "line" }, volume, toggle(settings.soundEnabled, (v) => { settings.soundEnabled = v; void save(); })),
      ),
      h("div", { class: "line", style: "padding:var(--s-2) 0" }, soundsFolder, reloadSounds, customCount),
      h("div", { class: "line", style: "padding:var(--s-2) 0" }, which, chooseFile, resetSound, picker),
    ]),
    h("div", { class: "hint", text: t("Drop a file named like one of the built-in sounds (finish.wav, approval.mp3, greet.m4a…) in the sounds folder to replace it, then Reload.") }),
  ];
}

/**
 * Settings → General → Language, as on the Mac: "System" follows the
 * system's language when Lintel has it (else English), or one of the ten.
 * Both windows and the tray switch in place, without a restart.
 */
function languageRow(): HTMLElement {
  const select = h("select", {}) as HTMLSelectElement;
  select.append(h("option", { value: "", text: t("System") }));
  for (const { code, name } of LANGUAGES) select.append(h("option", { value: code, text: name, lang: code }));
  select.value = LANGUAGES.some((l) => l.code === settings.language) ? settings.language : "";
  select.addEventListener("change", () => {
    settings.language = select.value;
    void save();
    applyLanguage();
  });
  return row(t("Language"), selectField(select));
}

// ── Shortcuts section ─────────────────────────────────────────────────────────

const SHORTCUTS_UI = {
  get title() { return t("Shortcuts"); },
  get hint() { return t("Work from any app. Click a shortcut to change it, then press the new keys — Esc cancels, Backspace removes it."); },
  get global() { return t("From anywhere"); },
  get island() { return t("In the open island"); },
  get recording() { return t("Press keys…"); },
  get none() { return t("None"); },
  get reset() { return t("Reset to defaults"); },
  get inUse() { return t("In use by another app"); },
  get duplicate() { return t("Used twice"); },
  get invalid() { return t("Not a valid shortcut"); },
  types: (ch: string) => t("Types “{char}”", { char: ch }),
  typesNote: (keys: string, ch: string) =>
    t("{keys} types “{char}” on your keyboard, so it can't be a shortcut. Pick another key.", { keys, char: ch }),
  get needsModifier() { return t("Hold Ctrl, Alt or the Windows key with it."); },
  get unsupportedKey() { return t("That key can't be used in a shortcut."); },
};

function shortcutsSection(initial: ShortcutsReport | null): HTMLElement {
  let report = initial;
  const list = h("div", { class: "shortcut-list" });
  const feedback = h("div", {});

  let stopRecording: (() => void) | null = null;

  function store(id: string, binding: Binding) {
    settings.shortcuts = { ...settings.shortcuts, [id]: binding };
    void save();
  }

  function tagFor(id: string, dups: Set<string>): HTMLElement | null {
    if (dups.has(id)) return h("span", { class: "tag err", text: SHORTCUTS_UI.duplicate });
    const st = report?.actions.find((a) => a.id === id);
    switch (st?.status) {
      case "inUse": return h("span", { class: "tag warn", text: SHORTCUTS_UI.inUse });
      case "duplicate": return h("span", { class: "tag err", text: SHORTCUTS_UI.duplicate });
      case "invalid": return h("span", { class: "tag err", text: SHORTCUTS_UI.invalid });
      case "typesCharacter": return h("span", { class: "tag warn", text: SHORTCUTS_UI.types(st.typed ?? "?") });
      default: return null;
    }
  }

  function record(id: string, binding: Binding, button: HTMLButtonElement) {
    stopRecording?.();
    clear(feedback);
    button.classList.add("recording");
    button.textContent = SHORTCUTS_UI.recording;
    void Bridge.shortcutsSuspend(true);

    const onKey = (e: KeyboardEvent) => {
      e.preventDefault();
      e.stopPropagation();
      const result = recordPress(e);
      switch (result.kind) {
        case "pending":
          return;
        case "keys":
          finish();
          store(id, { keys: result.keys, enabled: true });
          return;
        case "clear":
          finish();
          store(id, { keys: "", enabled: binding.enabled });
          return;
        case "typesCharacter":
          finish();
          feedback.append(h("div", {
            class: "notice warn",
            text: SHORTCUTS_UI.typesNote(displayKeys(result.keys), result.typed),
          }));
          return;
        case "needsModifier":
          feedback.replaceChildren(h("div", { class: "notice warn", text: SHORTCUTS_UI.needsModifier }));
          return;
        case "unsupported":
          feedback.replaceChildren(h("div", { class: "notice warn", text: SHORTCUTS_UI.unsupportedKey }));
          return;
        case "cancel":
          finish();
          return;
      }
    };
    const onBlur = () => finish();

    function finish() {
      window.removeEventListener("keydown", onKey, true);
      window.removeEventListener("blur", onBlur);
      stopRecording = null;
      // Takes the global shortcuts back, from what is saved by now.
      void Bridge.shortcutsSuspend(false);
      draw();
    }
    stopRecording = finish;
    window.addEventListener("keydown", onKey, true);
    window.addEventListener("blur", onBlur);
  }

  /** The shortcuts of the features that are on: from features_list, their binding from settings. */
  const featureRows = () =>
    featureInfos().filter((f) => f.on).flatMap((f) =>
      f.shortcuts.map((sc) => ({
        id: `${f.id}:${sc.id}`,
        text: `${t(f.name)} — ${t(sc.label)}`,
        binding: settings.shortcuts?.[`${f.id}:${sc.id}`] ?? { keys: sc.default ?? "", enabled: true },
      })),
    );

  function draw() {
    clear(list);
    const dups = duplicates(activeKeys(settings.shortcuts));
    const rows = [
      ...SHORTCUTS.filter((d) => d.ported).map((d) => ({ id: d.id as string, text: t(SHORTCUT_TEXT[d.id]), binding: effective(d, settings.shortcuts) })),
      ...featureRows(),
    ];
    for (const { id, text, binding } of rows) {
      const keycap = h("button", {
        class: "keycap",
        text: binding.keys ? displayKeys(binding.keys) : SHORTCUTS_UI.none,
      }) as HTMLButtonElement;
      keycap.disabled = !binding.enabled;
      keycap.addEventListener("click", () => record(id, binding, keycap));
      const sw = toggle(binding.enabled, (on) => store(id, { keys: binding.keys, enabled: on }));
      const tag = binding.enabled ? tagFor(id, dups) : null;
      list.append(h("div", { class: binding.enabled ? "line shortcut" : "line shortcut off" },
        sw,
        h("span", { class: "shortcut-name", text }),
        ...(tag ? [tag] : []),
        keycap,
      ));
    }

  }

  const islandList = h("div", { class: "shortcut-list" });
  for (const row of ISLAND_SHORTCUTS) {
    islandList.append(h("div", { class: "line shortcut" },
      h("span", { class: "shortcut-name", text: t(row.description) }),
      h("span", { class: "keycap static", text: row.keys }),
    ));
  }

  const reset = h("button", {
    class: "btn secondary",
    text: SHORTCUTS_UI.reset,
    onclick: () => {
      stopRecording?.();
      settings.shortcuts = {};
      clear(feedback);
      void save();
      draw();
    },
  });

  // The events are listened to once (see main); only the section on screen redraws.
  shortcutsListener = {
    report(fresh) {
      report = fresh;
      if (!stopRecording) draw();
    },
    settingsChanged() {
      if (!stopRecording) draw();
    },
  };

  draw();
  return h(
    "section",
    {},
    h("div", { class: "hint", text: SHORTCUTS_UI.hint }),
    h("div", { class: "subhead", text: SHORTCUTS_UI.global }),
    list,
    feedback,
    h("div", { class: "line" }, reset),
    h("div", { class: "subhead", text: SHORTCUTS_UI.island }),
    islandList,
  );
}

/** Settings → General → Weekly recap. The prefs live with the history in Rust. */
function recapRows(): HTMLElement[] {
  const T = {
    label: t("Weekly recap"),
    keep: t("Keep a history of my coding sessions"),
    clear: t("Clear history"),
    cleared: t("History cleared."),
    about: t("Counts and project names only — never commands, files or prompts. Kept on this computer for 12 weeks."),
  };
  const feedback = h("span", { class: "hint" });
  const sw = toggle(true, (v) => { void Bridge.recapSetEnabled(v); });
  void Bridge.recapPrefs().then((prefs) => {
    if (prefs) sw.setAttribute("aria-checked", String(prefs.enabled));
  });
  const clearBtn = h("button", {
    class: "btn danger",
    text: T.clear,
    onclick: async () => {
      clearBtn.disabled = true;
      await Bridge.recapClear();
      feedback.textContent = T.cleared;
      window.setTimeout(() => {
        clearBtn.disabled = false;
        feedback.textContent = "";
      }, 2400);
    },
  }) as HTMLButtonElement;
  return [
    h("div", { class: "line" },
      h("label", { text: T.label }),
      sw,
      h("span", { class: "hint", text: T.keep }),
    ),
    h("div", { class: "line" },
      h("label", {}),
      clearBtn,
      feedback,
    ),
    h("div", { class: "line" },
      h("label", {}),
      h("span", { class: "hint", style: "flex:1 1 0;min-width:0", text: T.about }),
    ),
  ];
}

// ── Language ──────────────────────────────────────────────────────────────────

/** The shortcuts section on screen, told about the events listened to once in main. */
let shortcutsListener: { report(fresh: ShortcutsReport): void; settingsChanged(): void } | null = null;

/** Shows the language Settings asks for. A change redraws the window in place. */
function applyLanguage() {
  setLanguage(resolveLanguage(settings.language, systemLanguages()));
}

function applyDirection() {
  document.documentElement.dir = isRtl() ? "rtl" : "ltr";
  document.title = t("Settings — {app}");
}

// ── Pages ─────────────────────────────────────────────────────────────────────

/** A page of the sidebar. `title` and `keywords` are English; the title is shown through `t()`. */
interface Page {
  id: string;
  title: string;
  icon: string;
  keywords: string[];
  /** A feature group, or "app" for the pages under App. */
  group?: string;
  /** Set on a feature's page: the switch and the gate. */
  feature?: FeatureInfo;
  build(): Promise<Node[]> | Node[];
}

const GROUP_TITLE: Record<FeatureInfo["group"], string> = {
  island: N_("Island"), agents: N_("Agents"), chat: N_("Chat"), media: N_("Media"), system: N_("System"),
  input: N_("Input"), windows: N_("Windows"), files: N_("Files"), capture: N_("Capture"), apps: N_("Apps"),
  tools: N_("Tools"),
};

const GROUP_ICON: Record<FeatureInfo["group"], string> = {
  island: "island", agents: "terminal", chat: "chat", media: "music", system: "cpu", input: "keyboard",
  windows: "grid", files: "shelf", capture: "shot", apps: "grid", tools: "sliders",
};

/** A main pill this build can run, and no pill declared twice. */
function sanitize() {
  settings = { ...settings, ...sanitizeDeclared(settings) };
}

async function pillsPage(): Promise<Node[]> {
  sanitize();
  const connected: Record<string, boolean> = { ...((await Bridge.agentHooksStatus()) ?? {}) };
  for (const def of availablePills()) {
    if (def.connect.kind === "key") connected[def.id] = (await Bridge.secretPresent(def.connect.key)) ?? false;
  }
  return [activePillsSection(connected)];
}

async function agentsBody(): Promise<Node[]> {
  const status = (await Bridge.hooksStatus()) ?? {
    installed: false, planRelayInstalled: false, autoLaunch: false, settingsPath: "", hookPath: "", hookReady: false,
  };
  return [claudeSection(status), agentsSection(await Bridge.agentHooksList()), wslSection(), sshSection(), planSection(status)];
}

async function chatBody(): Promise<Node[]> {
  const hasKey = (await Bridge.secretPresent("anthropic-api-key")) ?? false;
  const chatKeys: Record<string, boolean> = {};
  for (const def of CLOUD) {
    const key = providerDef(def.id).key!;
    chatKeys[key] = (await Bridge.secretPresent(key)) ?? false;
  }
  const customKey = (await Bridge.secretPresent(CUSTOM_SERVER_KEY)) ?? false;
  const prefs = await chatPrefs();
  const installed = new Set((await installedClis()).map((cli) => cli.id));
  return [
    apiSection(hasKey), await chatExtrasSection(prefs), await mcpSection(prefs), chatProvidersSection(chatKeys, () => {}, prefs), localSection(customKey),
    cliSection(prefs, installed),
  ];
}

async function integrationsBody(): Promise<Node[]> {
  sanitize();
  const present: Record<string, boolean> = {};
  for (const k of [
    "stripe-api-key", "github-token", "forge-url", "forge-token", "vercel-token",
    "n8n-url", "n8n-api-key", "resend-api-key", "notion-api-key", "calcom-api-key", "linear-api-key",
  ]) present[k] = (await Bridge.secretPresent(k)) ?? false;
  return [integrationsSection(present)];
}

/** What the base's own features show under their switch; the others use `settings(cx)`. */
const BASE_BODY: Record<string, () => Promise<Node[]> | Node[]> = {
  agents: agentsBody,
  chat: chatBody,
  integrations: integrationsBody,
  recap: () => [h("section", {}, ...recapRows())],
};

const BASE_KEYWORDS: Record<string, string[]> = {
  agents: ["hooks", "claude code", "plan usage", "status line", "relay", "codex", "cursor", "gemini", "permissions"],
  chat: ["api key", "model", "openai", "google", "openrouter", "free", "ollama", "lm studio", "local models", "providers", "cli", "claude code", "codex"],
  integrations: ["stripe", "github", "vercel", "n8n", "resend", "notion", "cal.com", "linear", "token", "keys"],
  recap: ["weekly", "history"],
};

/** One line per feature for the Features page. English only for now. debt: belongs in each feature's manifest. */
const FEATURE_LINE: Record<string, string> = {
  agents: "Agent sessions, permission cards and questions on the island.",
  chat: "Ask a model from the island, with your own key or a local model.",
  integrations: "Status pills for GitHub, Vercel and the other services you connect.",
  drop: "Drop a file on the island to hand it to an agent or send it on.",
  recap: "A weekly summary of your agents' work, kept on this computer.",
  "agent-usage": "Tokens used by your coding agents, and your plan limits.",
  apps: "Updates for your apps through winget, and installing packages.",
  "audio-extras": "Music bars, the mascot moving to the beat and finer volume steps.",
  "audio-mixer": "Volume per app, the output picker, and mute for all microphones.",
  breaks: "Eye and stretch breaks, and how long you have used the screen.",
  calendar: "The next meeting with its Join button, and reminders.",
  capture: "Screenshots of an area, a window or the whole screen.",
  "chat-plus": "Chat extras: attach a window, send by email, local models and tasks.",
  "cleaner-uninstaller": "Find files that can go, and uninstall programs with their leftovers.",
  clipboard: "Clipboard history and paste as plain text.",
  "command-bar": "Find and run apps, windows, settings and actions from one bar.",
  companion: "The mascot on your desktop, with outfits and small reactions.",
  "controls-toggles": "Quick system switches and actions on the Controls tab.",
  dictation: "Speak instead of typing. Speech becomes text on this computer.",
  "displays-keepawake": "Brightness for each display, dimming, and keeping the PC awake.",
  editor: "Annotate captures, capture a scrolling page, draw on the screen.",
  "huds-power": "Volume, brightness, battery and microphone notices.",
  "hw-helper": "Temperatures, fans and drive health, read by a small helper.",
  "input-tools": "Snippets, key remaps, mouse buttons and scrolling rules.",
  "island-extras": "A mirror, quick access tiles and website shortcuts.",
  lyrics: "Lyrics beside the player, in time with the song.",
  media: "The song that is playing, with controls for any player.",
  "media-tools": "Compress videos, make GIFs, convert pictures and read text in them.",
  "misc-late": "The welcome page, backup and reset, demo mode and small tools.",
  "notifications-accessories": "A notice when a Bluetooth device connects, with its battery.",
  "radial-menu": "A wheel of actions at the pointer, opened with a shortcut.",
  recorder: "Record the screen, a window or an area as an MP4 file.",
  shelf: "A place to park files and drag them out again later.",
  sysmon: "Processor, memory, disk and network, with processes and ports.",
  "timers-downloads": "Timers and Pomodoro on the island.",
  weather: "The forecast for a city you choose.",
  "window-layout": "Snap windows to halves, thirds and quarters with shortcuts.",
};

async function featuresPage(): Promise<Node[]> {
  const rows = [...featureInfos()].map((f) => {
    const group = FEATURE_LINE[f.id] ? t(FEATURE_LINE[f.id]) : t(GROUP_TITLE[f.group]);
    const hint = h("span", { class: "row-hint", text: group });
    const sw = toggle(f.on, (on) => {
      hint.textContent = group;
      Bridge.featureSetEnabled(f.id, on).catch((err) => {
        hint.textContent = String(err).replace(/^Error:\s*/, "");
      });
    });
    const label = h(
      "span",
      { class: "row-label", style: "display:inline-flex;align-items:center;gap:8px" },
      icon(f.id === "integrations" ? "link" : GROUP_ICON[f.group], 14),
      h("span", { text: t(f.name) }),
    );
    return h("div", { class: "row" }, label, hint, sw);
  });
  return [card(rows)];
}

function privacyPage(): HTMLElement[] {
  return [
    h("p", { class: "hint", text: t("No telemetry. Network requests only go to the services you configure yourself.") }),
    privacySection((page) => void Bridge.featureCall("misc-late", "open_settings", { page })),
  ];
}

const CORE: Page[] = [
  { id: "general", title: N_("General"), icon: "gear", keywords: ["language", "startup", "autostart", "launch", "system"], build: generalPage },
  { id: "island", title: N_("Island"), icon: "island", keywords: ["hover", "outline", "auto-close", "display", "screen", "monitor", "activities", "wake"], build: islandPage },
  { id: "mascot", title: N_("Mascot"), icon: "sparkle", keywords: ["character", "dot", "face"], build: mascotPage },
  { id: "pills", title: N_("Pills"), icon: "grid", keywords: ["active pills", "main tool", "colors", "tools"], build: pillsPage },
  { id: "sounds", title: N_("Sounds"), icon: "volume", keywords: ["sound", "volume", "mute", "folder", "custom"], build: soundsPage },
  { id: "shortcuts", title: N_("Shortcuts"), icon: "keyboard", keywords: ["keys", "hotkeys", "keyboard", "keybinding"], build: async () => [shortcutsSection(await Bridge.shortcutsStatus())] },
  { id: "features", title: N_("Features"), icon: "sliders", keywords: ["turn on", "turn off", "switch", "enable", "disable"], build: featuresPage },
  { id: "privacy", title: N_("Privacy"), icon: "shield", keywords: ["telemetry", "network", "data"], group: "app", build: privacyPage },
];

function featurePages(): Page[] {
  return [...featureInfos()].map((f) => ({
    id: `f-${f.id}`,
    title: f.name,
    icon: f.id === "integrations" ? "link" : GROUP_ICON[f.group],
    keywords: [...(BASE_KEYWORDS[f.id] ?? []), f.group, f.id],
    group: f.group,
    feature: f,
    build: () => featureBody(f),
  }));
}

/** The switch the core draws above every feature's page, then the feature's own body. */
async function featureBody(f: FeatureInfo): Promise<Node[]> {
  const hint = h("span", { class: "row-hint", text: f.on && !f.running ? t("Not running") : "" });
  const sw = toggle(f.on, (on) => {
    hint.textContent = "";
    Bridge.featureSetEnabled(f.id, on).catch((err) => {
      hint.textContent = String(err).replace(/^Error:\s*/, "");
    });
  });
  const head = card(h("div", { class: "row" }, h("span", { class: "row-label", text: t("Turned on") }), hint, sw));
  // `feature_call` is refused while it is off: nothing of its own is shown then.
  if (!f.running) {
    return [head, ...(f.on ? [] : [h("p", { class: "hint", text: t("Turn this on to change its settings.") })])];
  }
  const out: Node[] = [head];
  try {
    const base = BASE_BODY[f.id];
    const mod = featureById(f.id);
    if (base) out.push(...(await base()));
    else if (mod?.settings) out.push(mod.settings(featureCtx(f.id)));
  } catch (err) {
    out.push(h("div", { class: "notice err", text: String(err) }));
  }
  return out;
}

const allPages = (): Page[] => [...CORE.filter((p) => !p.group), ...featurePages(), ...CORE.filter((p) => p.group)];

// ── The shell: sidebar, search, hash router ───────────────────────────────────

const fold = (s: string) => s.normalize("NFD").replace(/\p{M}/gu, "").toLowerCase();

/** Folded substring over the page's title (shown and English) and its keywords. */
function matches(p: Page): boolean {
  const q = fold(query.trim());
  return !q || [t(p.title), p.title, ...p.keywords].some((s) => fold(s).includes(q));
}

let query = "";
let current = "general";
let navToken = 0;

const nav = h("div", {});
const pageEl = h("div", { class: "page" });
const search = field({
  icon: "search",
  placeholder: t("Search settings"),
  onInput: (v) => { query = v; drawNav(); },
  onEnter: () => {
    const first = allPages().find(matches);
    if (first) go(first.id);
  },
});

function go(id: string) {
  if (location.hash === `#/${id}`) void showPage();
  else location.hash = `#/${id}`;
}

function drawNav() {
  clear(nav);
  const shown = allPages().filter(matches);
  const item = (p: Page): ListItem => ({
    label: t(p.title),
    icon: p.icon,
    meta: p.feature && !p.feature.on ? t("Off") : undefined,
    on: p.id === current,
    onClick: () => go(p.id),
  });
  const block = (title: string | null, pages: Page[]) => {
    if (!pages.length) return;
    if (title) nav.append(h("div", { class: "section-title", text: title }));
    nav.append(list(pages.map(item)));
  };
  block(null, shown.filter((p) => !p.group));
  for (const g of Object.keys(GROUP_TITLE) as FeatureInfo["group"][]) {
    block(t(GROUP_TITLE[g]), shown.filter((p) => p.group === g));
  }
  block(t("App"), shown.filter((p) => p.group === "app"));
  if (!shown.length) nav.append(h("p", { class: "hint", style: "padding:var(--s-3)", text: t("No settings match.") }));
}

async function showPage() {
  const id = location.hash.replace(/^#\//, "") || "general";
  const page = allPages().find((p) => p.id === id) ?? allPages()[0];
  const scroll = page.id === current ? pageEl.scrollTop : 0;
  current = page.id;
  const token = ++navToken;
  // What the previous page registered is gone with it.
  declaredViews.length = 0;
  localRedraw = null;
  shortcutsListener = null;
  drawNav();
  const body = await page.build();
  if (token !== navToken) return;
  clear(pageEl);
  pageEl.append(h("h1", { text: t(page.title) }), ...body);
  pageEl.scrollTop = scroll;
}

// ── Boot ──────────────────────────────────────────────────────────────────────

async function main() {
  installTooltip();
  const boot = await Bridge.boot();
  if (boot) {
    settings = { ...settings, ...boot.settings };
    version = boot.version;
  }
  setLanguage(resolveLanguage(settings.language, systemLanguages()));
  applyDirection();
  await loadFeatures();
  search.input.placeholder = t("Search settings");
  // First run only: the welcome page, until it is dismissed (misc-late).
  await showOnboarding(root);

  clear(root);
  root.append(
    h("div", { class: "win-body shell" },
      h("aside", { class: "sidebar" }, search, nav, h("div", { class: "version", text: `${BRAND.name} ${version}` })),
      pageEl,
    ),
  );
  window.addEventListener("hashchange", () => void showPage());
  // A switch was flipped (here, the tray or a shortcut): the sidebar and the page follow.
  onFeaturesChanged(() => void showPage());
  onLanguageChange(() => {
    applyDirection();
    search.input.placeholder = t("Search settings");
    void showPage();
  });
  await showPage();

  void onEvent<ShortcutsReport>("shortcuts-status", (fresh) => shortcutsListener?.report(fresh));
  void onEvent<Settings>("settings-changed", (s) => {
    const mine = `${settings.soundEnabled}|${settings.soundVolume}|${settings.autoCloseInterval}`;
    const before = `${settings.chatProvider}|${settings.ollamaUrl}|${settings.lmstudioUrl}|${settings.customUrl}`;
    settings = { ...settings, ...s };
    shortcutsListener?.settingsChanged();
    for (const redraw of declaredViews) redraw();
    const after = `${settings.chatProvider}|${settings.ollamaUrl}|${settings.lmstudioUrl}|${settings.customUrl}`;
    if (before !== after) localRedraw?.();
    applyLanguage();
    // The island changed one of these: the open page shows the old value, redraw it.
    if (mine !== `${settings.soundEnabled}|${settings.soundVolume}|${settings.autoCloseInterval}` && (current === "general" || current === "sounds")) void showPage();
  });
}

void main();
