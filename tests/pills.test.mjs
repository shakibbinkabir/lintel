// The pill catalog (src/core/pills.ts) and the declared pills in the app state.

import { beforeEach, test } from "node:test";
import assert from "node:assert/strict";
import {
  DEFAULT_MAIN_PILL, MAX_DECLARED, PILL_CATALOG, PILL_CATEGORIES, availablePills, chooseMainPill,
  isComingSoon, isHookPill, mainPillChoices, orderPills, pillDefinition, sanitizeDeclared, sessionSubtitle,
  toggleDeclared,
} from "../src/core/pills.ts";
import { DEFAULT_SETTINGS, State } from "../src/core/state.ts";

// ── The catalog itself ────────────────────────────────────────────────────────

test("the catalog holds the Mac's pills, in the Mac's order, with the Mac's values", () => {
  // Copied from NotchBuddy/Sources/notchKit/PillCatalog.swift: an ID is a
  // contract value, and a colour or subtitle that drifts is a visible bug.
  const mac = [
    ["integration_claude", "VS Code", "#F5F6F8", "workspace", "Integration"],
    ["agent_cursor", "Cursor", "#C0C4CC", "workspace", "Integration"],
    ["agent_jetbrains", "JetBrains", "#FF318C", "workspace", "Integration"],
    ["agent_antigravity", "Antigravity", "#E879F9", "workspace", "Integration"],
    ["agent_codex", "Codex", "#2DD4BF", "workspace", "Integration"],
    ["agent_gemini", "Gemini CLI", "#8AB4F8", "agent", "Agent"],
    ["agent_copilot", "Copilot CLI", "#818CF8", "agent", "Agent"],
    ["agent_muse", "Muse Code", "#38BDF8", "agent", "Agent"],
    ["agent_opencode", "OpenCode", "#4ADE80", "agent", "Agent"],
    ["agent_amp", "Amp", "#F59E0B", "agent", "Agent"],
    ["agent_hermes", "Hermes", "#C084FC", "agent", "Agent"],
    ["agent_kiro", "Kiro", "#9046FF", "agent", "Agent"],
    ["agent_claude-desktop", "Claude Desktop", "#D97757", "agent", "Agent"],
    ["ai_anthropic", "Anthropic", "#E07950", "ai", "Chat"],
    ["ai_google", "Google AI", "#4285F4", "ai", "Chat"],
    ["ai_openai", "OpenAI", "#10A37F", "ai", "Chat"],
    ["ai_ollama", "Ollama", "#FACC15", "ai", "Chat"],
    ["ai_lmstudio", "LM Studio", "#A3E635", "ai", "Chat"],
    ["integration_resend", "Resend", "#22C55E", "service", "Integration"],
    ["integration_n8n", "n8n", "#F29B38", "service", "Integration"],
    ["integration_vercel", "Vercel", "#7C5CFF", "service", "Integration"],
    ["integration_github", "GitHub", "#F4505E", "service", "Integration"],
    ["integration_notion", "Notion", "#8C8C8C", "service", "Integration"],
    ["integration_calcom", "Cal.com", "#C9956A", "service", "Integration"],
    ["integration_stripe", "Stripe", "#0570DE", "service", "Integration"],
    ["integration_linear", "Linear", "#5E6AD2", "service", "Integration"],
    ["integration_music", "Apple Music", "#FA2D48", "service", "Integration"],
  ];
  assert.deepEqual(
    PILL_CATALOG.map((p) => [p.id, p.name, p.color, p.category, p.subtitle]),
    mac,
  );
  assert.deepEqual(PILL_CATEGORIES.map((c) => c.title), [
    "Where you code", "Agents", "AI for the chat", "Services",
  ]);
});

test("the pills Windows always had keep their IDs, names, colours and sources", () => {
  const shipped = [
    ["integration_claude", "VS Code", "#F5F6F8", "claudeCode"],
    ["integration_resend", "Resend", "#22C55E", "n8n"],
    ["integration_n8n", "n8n", "#F29B38", "n8n"],
    ["integration_vercel", "Vercel", "#7C5CFF", "n8n"],
    ["integration_github", "GitHub", "#F4505E", "n8n"],
    ["integration_notion", "Notion", "#8C8C8C", "n8n"],
    ["integration_calcom", "Cal.com", "#C9956A", "n8n"],
    ["integration_stripe", "Stripe", "#0570DE", "n8n"],
  ];
  for (const [id, name, color, source] of shipped) {
    const def = pillDefinition(id);
    assert.deepEqual([def.id, def.name, def.color, def.source], [id, name, color, source]);
    assert.equal(def.support, "yes");
  }
});

test("IDs are unique", () => {
  const ids = PILL_CATALOG.map((p) => p.id);
  assert.equal(new Set(ids).size, ids.length);
});

test("no chat provider that works here says Coming soon", () => {
  for (const id of ["ai_anthropic", "ai_google", "ai_openai", "ai_ollama", "ai_lmstudio"]) {
    assert.ok(!isComingSoon(id), id);
  }
  // The local servers are connected through Settings → Local models, not a key.
  assert.deepEqual(pillDefinition("ai_ollama").connect, { kind: "server", field: "ollamaUrl" });
  assert.deepEqual(pillDefinition("ai_lmstudio").connect, { kind: "server", field: "lmstudioUrl" });
  assert.equal(pillDefinition("ai_google").connect.key, "google-api-key");
});

test("the main pill is a workspace tool that works here", () => {
  assert.deepEqual(mainPillChoices("linux").map((p) => p.id), [
    "integration_claude", "agent_cursor", "agent_jetbrains", "agent_antigravity", "agent_codex",
  ]);
  assert.equal(DEFAULT_MAIN_PILL, "integration_claude");
});

test("hook-driven pills are the workspace tools and the agents with hooks", () => {
  for (const id of ["integration_claude", "agent_cursor", "agent_codex", "agent_gemini", "agent_copilot", "agent_muse", "agent_antigravity", "agent_opencode", "agent_amp", "agent_hermes"]) {
    assert.ok(isHookPill(id), id);
  }
  for (const id of ["agent_claude-desktop", "ai_anthropic", "integration_stripe", "agent_unknown"]) {
    assert.ok(!isHookPill(id), id);
  }
});

test("a live session names its tool next to the project", () => {
  assert.equal(sessionSubtitle("integration_claude"), "Claude Code");
  assert.equal(sessionSubtitle("agent_cursor"), "Cursor");
  assert.equal(sessionSubtitle("agent_claude-desktop"), "Claude Desktop");
  assert.equal(sessionSubtitle("agent_gemini"), "Agent");
  assert.equal(sessionSubtitle("agent_whatever"), "Agent");
});

// ── Declaring pills ──────────────────────────────────────────────────────────

test("a declaration from an older or edited settings file is made usable", () => {
  assert.deepEqual(
    sanitizeDeclared({ mainPill: "integration_stripe", activeIntegrations: ["integration_claude", "x"] }, "linux"),
    { mainPill: "integration_claude", activeIntegrations: [] },
  );
  assert.deepEqual(
    sanitizeDeclared({ mainPill: "agent_cursor", activeIntegrations: ["agent_cursor", "integration_n8n", "integration_n8n", "integration_music"] }, "linux"),
    { mainPill: "agent_cursor", activeIntegrations: ["integration_n8n"] },
  );
  // No mainPill at all: a settings.json from before this version.
  assert.deepEqual(
    sanitizeDeclared({ activeIntegrations: ["integration_github"] }, "windows"),
    { mainPill: "integration_claude", activeIntegrations: ["integration_github"] },
  );
  assert.equal(sanitizeDeclared({ mainPill: "agent_claude-desktop", activeIntegrations: [] }, "windows").mainPill,
    "integration_claude");
  // A pill that is no longer in the catalog (the Spotify integration, removed) is
  // dropped from a file that still has it switched on, and frees its slot.
  assert.deepEqual(
    sanitizeDeclared({ mainPill: "integration_claude", activeIntegrations: ["integration_spotify", "integration_github"] }, "windows"),
    { mainPill: "integration_claude", activeIntegrations: ["integration_github"] },
  );
});

test("any number of pills next to the main one, never the main one itself", () => {
  const d = {
    mainPill: "integration_claude",
    activeIntegrations: ["integration_n8n", "integration_github", "integration_stripe"],
  };
  assert.deepEqual(toggleDeclared(d, "agent_gemini", "linux"), [...d.activeIntegrations, "agent_gemini"]);
  // The limit is the catalog itself: every pill on this OS can be declared.
  assert.ok(MAX_DECLARED >= availablePills("linux").length);
  let cur = { mainPill: "integration_claude", activeIntegrations: [] };
  for (const p of availablePills("linux")) {
    const next = toggleDeclared(cur, p.id, "linux");
    if (p.id === cur.mainPill) { assert.equal(next, null); continue; }
    assert.ok(next?.includes(p.id), p.id);
    cur = { ...cur, activeIntegrations: next };
  }
  assert.equal(toggleDeclared(cur, "integration_claude", "linux"), null);
  assert.equal(toggleDeclared(d, "integration_music", "linux"), null);
  assert.equal(toggleDeclared(d, "agent_claude-desktop", "linux"), null);
  assert.ok(toggleDeclared(d, "agent_claude-desktop", "windows").includes("agent_claude-desktop"));
  assert.deepEqual(toggleDeclared(d, "integration_github", "linux"), ["integration_n8n", "integration_stripe"]);
  // Every chat provider can be declared now that the chat talks to it.
  assert.ok(toggleDeclared(d, "ai_google", "linux").includes("ai_google"));
  assert.ok(toggleDeclared(d, "ai_ollama", "linux").includes("ai_ollama"));
});

test("picking a main pill takes it out of the declared ones, and only workspace tools qualify", () => {
  const d = { mainPill: "integration_claude", activeIntegrations: ["agent_cursor", "integration_n8n"] };
  assert.deepEqual(chooseMainPill(d, "agent_cursor", "linux"), {
    mainPill: "agent_cursor", activeIntegrations: ["integration_n8n"],
  });
  assert.equal(chooseMainPill(d, "integration_n8n", "linux"), null);
  assert.equal(chooseMainPill(d, "agent_gemini", "linux"), null);
});

test("pills are ordered main first, then other agents, then the catalog's order", () => {
  const ids = (list) => list.map((t) => t.id);
  const tasks = ["integration_stripe", "agent_mine", "integration_n8n", "agent_cursor", "integration_claude", "agent_gemini"]
    .map((id) => ({ id }));
  assert.deepEqual(ids(orderPills(tasks, "integration_claude")), [
    "integration_claude", "agent_mine", "agent_cursor", "agent_gemini", "integration_n8n", "integration_stripe",
  ]);
  assert.deepEqual(ids(orderPills(tasks, "agent_cursor")).slice(0, 3), [
    "agent_cursor", "agent_mine", "integration_claude",
  ]);
});

// ── In the app state ─────────────────────────────────────────────────────────

beforeEach(() => {
  State.tasks = [];
  State.focusId = null;
  State.os = "windows";
  State.settings = { ...DEFAULT_SETTINGS };
});

const ids = () => State.tasks.map((t) => t.id);

test("the main pill always loads, the declared ones join it, and focus starts on it", () => {
  State.settings.mainPill = "agent_cursor";
  State.settings.activeIntegrations = ["integration_github", "agent_gemini"];
  State.loadIntegrationTasks();
  assert.deepEqual(ids(), ["agent_cursor", "agent_gemini", "integration_github"]);
  assert.equal(State.focusId, "agent_cursor");
  assert.equal(State.tasks[1].name, "Gemini CLI");
  assert.equal(State.tasks[1].color, "#8AB4F8");
});

test("an old settings file gets VS Code as its main pill and keeps its integrations", () => {
  State.settings = { ...DEFAULT_SETTINGS, mainPill: undefined };
  State.loadIntegrationTasks();
  assert.deepEqual(ids(), [
    "integration_claude", "integration_resend", "integration_n8n", "integration_vercel", "integration_github",
  ]);
  assert.equal(State.settings.mainPill, "integration_claude");
});

test("switching the main pill frees its slot and the old main pill goes when idle", () => {
  State.loadIntegrationTasks();
  State.settings.mainPill = "agent_codex";
  State.loadIntegrationTasks();
  assert.equal(ids()[0], "agent_codex");
  assert.ok(!ids().includes("integration_claude"));
  assert.equal(State.focusId, "agent_codex");
});

test("a pill in the middle of a session stays until the session ends", () => {
  State.loadIntegrationTasks();
  State.tasks[0].state = "working";
  State.settings.mainPill = "agent_cursor";
  State.loadIntegrationTasks();
  assert.ok(ids().includes("integration_claude"));
  State.removeTask("integration_claude");
  assert.ok(!ids().includes("integration_claude"));
});

test("the end of a session puts a kept pill back as it was, and removes any other", () => {
  State.settings.activeIntegrations = ["agent_gemini"];
  State.loadIntegrationTasks();
  const gemini = State.tasks.find((t) => t.id === "agent_gemini");
  Object.assign(gemini, { name: "proj", state: "finished", steps: ["a"], pillBadge: "finished" });
  State.removeTask("agent_gemini");
  assert.deepEqual(
    [gemini.name, gemini.state, gemini.steps, gemini.pillBadge],
    ["Gemini CLI", "idle", [], null],
  );
  State.upsertExternalAgent("agent_mine", "mine", "#22C55E");
  State.setFocus("agent_mine");
  State.removeTask("agent_mine");
  assert.ok(!ids().includes("agent_mine"));
  assert.equal(State.focusId, "integration_claude");
});

test("a Claude Code session gets its pill even when it is not loaded", () => {
  State.settings.mainPill = "agent_codex";
  State.loadIntegrationTasks();
  const t = State.upsertWorkspacePill("integration_claude", "proj", "/p");
  assert.equal(ids()[1], "integration_claude");
  assert.equal(t.name, "proj");
  assert.equal(t.sessionCwd, "/p");
  assert.equal(State.upsertWorkspacePill("not_a_pill", "x", ""), null);
});

test("toggling declares any number of pills and never the main one", () => {
  State.settings.activeIntegrations = [];
  State.loadIntegrationTasks();
  for (const id of ["integration_n8n", "agent_gemini", "ai_anthropic", "integration_stripe", "integration_github"]) {
    State.toggleIntegration(id);
  }
  assert.deepEqual(State.settings.activeIntegrations, [
    "integration_n8n", "agent_gemini", "ai_anthropic", "integration_stripe", "integration_github",
  ]);
  State.toggleIntegration("integration_claude");
  assert.ok(ids().includes("integration_claude"));
  State.setFocus("agent_gemini");
  State.toggleIntegration("agent_gemini");
  assert.ok(!ids().includes("agent_gemini"));
  assert.equal(State.focusId, "integration_claude");
});

// ── The island's width on a scaled display (src/core/layout.ts) ───────────────

test("the island's width is always an even number of device pixels", async () => {
  const { pixelWidth, ACTIVITY_W, COMPACT_W, EXPANDED_W, NOTICE_W } = await import("../src/core/layout.ts");
  for (const dpr of [1, 1.1, 1.25, 1.5, 1.75, 2, 2.25]) {
    for (const w of [COMPACT_W, ACTIVITY_W, NOTICE_W, EXPANDED_W, 246.963, 301.4, 633.487]) {
      const half = (pixelWidth(w, dpr) / 2) * dpr;
      assert.ok(Math.abs(half - Math.round(half)) < 1e-6, `${w} at ${dpr}: half is ${half} device px`);
      // Never more than one device pixel away from what was asked.
      assert.ok(Math.abs(pixelWidth(w, dpr) - w) * dpr <= 1 + 1e-6, `${w} at ${dpr}`);
    }
  }
  // Unscaled, the usual widths are what they were; at 125 % the 340 and 380 px strips move by 0.8 px.
  assert.equal(pixelWidth(ACTIVITY_W, 1), ACTIVITY_W);
  assert.equal(pixelWidth(COMPACT_W, 1.25), COMPACT_W);
  assert.equal(pixelWidth(ACTIVITY_W, 1.25), 340.8);
  assert.equal(pixelWidth(NOTICE_W, 1.25), 380.8);
});
