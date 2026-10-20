// Island views: the header and the base views of the open island (overview,
// alert cards, in-island settings). Built from the design language
// (docs/ARCHITECTURE.md §7): cards, buttons and tabs come from src/ui.

import { h, clear, dot } from "./dom";
import { icon } from "../ui/icons";
import { btn, capsule, card, dotMark, field, meter, setDotMark, setMeter, tab, type Tone } from "../ui/components";
import { State, familyOf, needsYou, type AgentTask } from "../core/state";
import { QuestionFlow, questionKey, questionKeyNames, type Answers } from "../core/question";
import { firstParagraph, isLongAnswer, renderInline, renderMarkdown } from "./markdown";
import type { Notice } from "../core/feature";
import type { IslandViewName } from "../core/layout";
import { createMiniBot, pruneMiniBots } from "../mascot/minibots";
import { buildPrompt } from "./chat";
import { buildChoose, buildUpload, buildUploading } from "./upload";
import { idleActions, idleStatus, metaLine, renderIntegrationCard, serviceLine, type IntegrationCardHooks } from "./integrations";
import { highlightRow, listRows, openRow } from "./github";
import { pillDefinition, sessionOwner, sessionSubtitle } from "../core/pills";
import {
  PlanCard, buildPlanPill, claudePillVisible, codexPillVisible, planCardOpen, refreshCodexPlanUsage,
} from "./usage";
import { buildDiffCard } from "./diff";
import { buildCodeView } from "./codeview";
import { lastTextStep, parseDiffStep } from "../core/diff";
import { BRAND } from "../core/brand";
import { Bridge, type Decision } from "../core/bridge";
import { byPointer } from "../core/approvals";
import { featureOn } from "../core/registry";
import { buildRecap } from "./recap";
import { N_, language, t, tl, type Msg } from "../i18n/i18n";
import type { ViewCommand } from "../island/shortcuts";
import { buildGallery, sections } from "../island/sections";

// The session list, the chips and the answer preview (core-3) have their own
// sheet, loaded with the views rather than at import (tests have no bundler).
if (typeof document !== "undefined" && document.head) void import("./agents.css");

export interface ViewActions {
  setView(v: IslandViewName): void;
  /** "Cancel" on a dropped file: forgets it and goes back home. */
  cancelDrop(): void;
  collapse(): void;
  /** Folds a waiting card to the compact island without answering it. */
  foldApproval(): void;
  setFocus(id: string): void;
  /**
   * The focused session's home: the app it lives in (Codex, Claude, Cursor,
   * Antigravity) or its terminal. The island folds once it is there.
   */
  openTerminal(): void;
  /** A click on a row of the session list: that session to the front, and to its home. */
  openSessionOf(id: string): void;
  /** The ↗ button: opens whatever the focused pill points at. */
  openTarget(): void;
  openUrl(url: string): void;
  /**
   * The click on Allow or Deny, for the request the card was drawn for: one
   * that has left the card since is not answered by it. Only ever a click.
   */
  decide(d: Decision, requestId: string): void;
  /**
   * Answers the question Claude Code asked: question text → chosen label, or
   * the words typed under "Other…" for the questions listed in `other`.
   */
  answer(answers: Answers, other: string[], requestId: string): void;
  /** Hands that request back to the terminal. */
  answerInTerminal(requestId: string): void;
  /**
   * The question card's keys count: the pointer is on the card, or the island
   * has the keyboard by a click (docs/ARCHITECTURE.md §4.1).
   */
  keysAllowed(): boolean;
  toggleSound(): void;
  setVolume(v: number): void;
  setAutoClose(seconds: number): void;
  openSettingsWindow(): void;
  blip(): void;
  /** A feature's section, by its key (`<feature>.<id>`). */
  openSection(key: string): void;
  /** The section the island is on, or null. */
  activeSection(): string | null;
  /** The level to show in the header (a volume or brightness notice while the island is open). */
  level(): Notice | null;
  /** True while the feature runs: the gate for what a built-in feature shows. */
  featureOn(id: string): boolean;
}

export interface ViewHost {
  el: HTMLElement;
  sync(): void;
  /** Called when the view becomes active, for views with a text field. */
  focus?(): void;
  /** Called every frame while the view is on screen. True = needs another frame. */
  tick?(nowMs: number): boolean | void;
  /** Ctrl+O / Ctrl+E while the view is on screen (island/shortcuts.ts). */
  command?(command: ViewCommand): void;
  /**
   * The keys the view acts on that the island reads for it while the pointer
   * is over it (docs/ARCHITECTURE.md §4.1). Only the question card has any.
   */
  keys?(): string[];
}

// ── Shared pieces ─────────────────────────────────────────────────────────────

/** Clears `el` and puts these in, skipping what is absent. */
function fill(el: Element, ...kids: (Node | null | false | undefined)[]) {
  clear(el);
  for (const k of kids) if (k) el.append(k);
}

/** An alert card: tinted in its tone, a column of rows. */
function alertCard(tone: Tone | true | undefined, ...children: (Node | null)[]): HTMLElement {
  const el = card(children, tone);
  el.classList.add("isl-alert");
  return el;
}

/**
 * Where the mascot sits in a card. The island's one canvas glides onto it
 * (core/layout.ts says where); with the mascot hidden its dot does.
 */
function markSlot(size = 22): HTMLElement {
  return h("span", { class: "mark-slot", style: `width:${size}px;height:${size}px` });
}

/** First row of an alert card: the mascot's place, who it is, what is going on; `extra` sits at its right end. */
function whoRow(el: HTMLElement, task: AgentTask | null, label: string, ...extra: (Node | null)[]) {
  fill(
    el, markSlot(), task ? h("b", { text: task.name }) : null, h("span", { class: "isl-meta", text: label }),
    h("span", { class: "isl-gap" }), ...extra,
  );
}

/** Who the request on the card belongs to: its own session's pill, whatever pill is in front. */
function cardTask(): AgentTask | null {
  const id = State.pendingApproval?.pillId;
  return State.tasks.find((x) => x.id === id) ?? State.focusTask;
}

/** "1 of 3" while several requests wait: the card shows the oldest, the others keep their place. */
function queueCount(): HTMLElement | null {
  const total = State.approvals.size;
  return total > 1 ? capsule({ text: t("{index} of {total}", { index: 1, total }), tone: "warn" }) : null;
}

// ── Session chips ─────────────────────────────────────────────────────────────

/** Permission modes in which the agent no longer asks: their chip is red. */
const RISKY_MODES = new Set(["bypassPermissions", "dontAsk"]);

/** A task says what it runs with: a permission mode other than the default, an effort, a model. */
export function hasChips(task: AgentTask | null): boolean {
  return !!task && !!((task.mode && task.mode !== "default") || task.effort || task.model);
}

/** The session chips (permission mode, effort, model), as the agent names them. Drawn again only when one changes. */
function syncChips(el: HTMLElement, task: AgentTask | null) {
  const key = hasChips(task) ? `${task!.mode}|${task!.effort}|${task!.model}` : "";
  if (el.dataset.key === key) return;
  el.dataset.key = key;
  clear(el);
  if (!key || !task) return;
  if (task.mode && task.mode !== "default") {
    // "acceptEdits" reads "accept edits": the agent's own word, spaced.
    const words = task.mode.replace(/([a-z])([A-Z])/g, "$1 $2").toLowerCase();
    el.append(capsule({ text: words, tone: RISKY_MODES.has(task.mode) ? "bad" : undefined }));
  }
  if (task.effort) el.append(capsule({ text: task.effort }));
  if (task.model) el.append(capsule({ text: task.model }));
}

/** A task's mark in a pill: its mini mascot, or its dot while the mascot is hidden (§9). */
function taskMark(task: AgentTask, size = 22): HTMLElement {
  if (State.settings.showMascot) return createMiniBot(task, size);
  return h("span", { class: "mark-slot", style: `width:${size}px;height:${size}px` }, dotMark(task.color, task.state));
}

/** What a task is doing, in one line, and the tone to say it in. */
export function taskLine(task: AgentTask): { text: string; tone?: Tone } {
  const badge = task.pillBadge;
  if (task.state === "approval" || badge === "approval") return { text: t("Needs approval"), tone: "warn" };
  if (task.state === "question") return { text: t("Asking a question"), tone: "info" };
  if (task.state === "error" || badge === "error") {
    return { text: lastTextStep(task.steps) ?? t("Stopped on an error"), tone: "bad" };
  }
  if (task.state === "finished" || badge === "finished") {
    return { text: task.finalLine || lastTextStep(task.steps) || t("Finished"), tone: "ok" };
  }
  const step = task.steps.at(-1);
  const diff = step ? parseDiffStep(step) : null;
  if (diff) return { text: `${diff.filename}  +${diff.added} −${diff.removed}` };
  if (step) return { text: step };
  if (task.state === "working") return { text: t("Working") };
  if (task.state === "thinking") return { text: t("Thinking") };
  if (task.state === "searching") return { text: t("Searching") };
  // A service says the latest of its card; a tool or an agent at rest, what it is.
  return serviceLine(task.id) ?? { text: t(pillDefinition(familyOf(task))?.subtitle ?? sessionSubtitle(familyOf(task))) };
}

/** A workspace tool or an agent: a pill whose content is a session, not a service's data. */
export function isSessionPill(task: AgentTask): boolean {
  if (task.family || (task.source === "agent" && !task.isIntegration)) return true;
  const category = pillDefinition(task.id)?.category;
  return category === "workspace" || category === "agent" || (category == null && task.id.startsWith("agent_"));
}

// ── Sessions ──────────────────────────────────────────────────────────────────

/** The live sessions of a pill's agent as their pills, the ones that need the user first. */
export function sessionTasks(task: AgentTask): AgentTask[] {
  const out: AgentTask[] = [];
  for (const s of State.sessions.list(familyOf(task))) {
    const found = State.tasks.find((x) => x.id === s.pillId);
    if (found && !out.includes(found)) out.push(found);
  }
  return [...out.filter(needsYou), ...out.filter((x) => !needsYou(x))];
}

/** A session's state in one word (working, waiting, needs approval, done, error), and its tone. */
export function sessionState(task: AgentTask): { text: string; tone?: Tone } {
  if (task.state === "approval" || task.pillBadge === "approval") return { text: t("Needs approval"), tone: "warn" };
  if (task.state === "question") return { text: t("Waiting for you"), tone: "warn" };
  if (task.state === "error" || task.pillBadge === "error") return { text: t("Error"), tone: "bad" };
  if (task.state === "finished" || task.pillBadge === "finished") return { text: t("Done"), tone: "ok" };
  if (task.state === "working" || task.state === "thinking" || task.state === "searching") return { text: t("Working") };
  return { text: "" };
}

/**
 * The sessions of the focused pill's agent: state, project, last tool and
 * model on a row each, and how many need the user. A click on a row goes to
 * that session.
 */
function buildSessionList(actions: ViewActions) {
  const count = h("span");
  const rows = h("div", { class: "list" });
  const el = h("div", { class: "isl-sessions" }, h("div", { class: "section-title" }, h("span", { text: tl("SESSIONS") }), count), rows);
  let key = "";
  return {
    el,
    sync(tasks: AgentTask[], focusId: string) {
      const next = [language(), focusId, ...tasks.map((x) =>
        [x.id, x.name, x.color, x.state, x.pillBadge, x.sessionCwd, x.lastTool, x.model].join(":"))].join("|");
      // Rebuilt only when a row changes: between a mouse-down and a mouse-up it would swallow the click.
      if (next === key) return;
      key = next;
      const needs = tasks.filter(needsYou).length;
      fill(count, needs > 0 ? capsule({ icon: "bell", text: String(needs), tone: "warn" }) : null);
      fill(rows, ...tasks.map((x) => {
        const project = x.sessionCwd?.split(/[\\/]/).filter(Boolean).pop() ?? x.name;
        const { text, tone } = sessionState(x);
        const busy = x.state === "working" || x.state === "thinking" || x.state === "searching";
        const detail = [text, busy ? x.lastTool : null].filter(Boolean).join(" · ");
        const row = h(
          "button",
          { class: `list-item isl-session${x.id === focusId ? " on" : ""}`, title: detail, onclick: () => actions.openSessionOf(x.id) },
          dotMark(x.color, x.pillBadge && x.state === "idle" ? x.pillBadge : x.state),
          h("span", {}, h("b", { text: project }), detail ? h("i", { class: tone ?? "", text: detail }) : null),
          x.model ? h("small", { text: x.model }) : null,
        );
        return row;
      }));
    },
  };
}

const pillLabel = (task: AgentTask) => (task.id === "integration_claude" ? "VS Code" : task.name);

// ── Header ────────────────────────────────────────────────────────────────────

/** What the header's speaker button says: it switches the app's own sounds. */
const SOUNDS_ON = N_("{name} sounds on");
const SOUNDS_OFF = N_("{name} sounds off");

/** How many section tabs the header holds before the gallery is the way to the rest. */
const HEADER_SECTIONS = 7; // ponytail: a fixed count; measure the row if titles get long

export function buildHeader(actions: ViewActions): ViewHost {
  const go = (v: IslandViewName) => {
    actions.blip();
    actions.setView(v);
  };
  const iconTab = (name: string, title: Msg | string, onClick: () => void) => {
    const el = tab(name, title, onClick);
    el.classList.add("icon");
    return el;
  };

  const tabHome = tab("home", tl("Home"), () => go(State.tasks.length === 0 ? "empty" : "overview"));
  const tabChat = tab("chat", tl("Chat"), () => go("prompt"));
  const tabDrop = tab("plus", tl("Drop"), () => go("upload"));
  const sectionTabs = h("span", { class: "isl-tabs" });
  const galleryBtn = iconTab("grid", tl("All sections (Ctrl+K)"), () => go("gallery"));
  const gearBtn = iconTab("gear", tl("Settings"), () => go("settings"));
  // The switch for the app's own sounds (not the system volume): its icon, its
  // colour and its tooltip say which way it is.
  const soundBtn = iconTab("volume", "", () => actions.toggleSound());
  // Plan usage pills (off by default): before the gear, Claude first.
  const claudePill = buildPlanPill(false);
  const codexPill = buildPlanPill(true);
  // A volume or brightness change while the island is open shows here (§5).
  const levelBar = meter(0);
  const levelNum = h("span", { class: "isl-num" });
  const level = h("span", { class: "capsule isl-level" });
  const right = h("span", { class: "isl-head-right" }, galleryBtn, claudePill.el, codexPill.el, gearBtn, soundBtn);
  let codexShown = false;
  let tabsKey = "";
  let soundKey = "";
  let levelIcon = "";

  const el = h("div", { id: "header" }, tabHome, tabChat, tabDrop, sectionTabs, h("span", { class: "isl-gap" }), level, right);

  function syncSectionTabs() {
    const all = sections();
    const active = actions.activeSection();
    // The first few, and the one that is open if it is further down the list.
    const shown = all.filter((s, i) => i < HEADER_SECTIONS || s.key === active);
    const key = `${language()}~${shown.map((s) => s.key).join("|")}`;
    if (key !== tabsKey) {
      tabsKey = key;
      clear(sectionTabs);
      for (const s of shown) {
        const el = tab(s.def.icon, t(s.def.title), () => {
          actions.blip();
          actions.openSection(s.key);
        });
        el.dataset.key = s.key;
        sectionTabs.append(el);
      }
    }
    const on = State.view === "section" ? active : null;
    for (const el of sectionTabs.children) el.classList.toggle("on", (el as HTMLElement).dataset.key === on);
    galleryBtn.style.display = all.length > 0 ? "" : "none";
  }

  function syncPlanPills() {
    const agents = actions.featureOn("agents");
    const claudeOn = agents && claudePillVisible();
    const codexOn = agents && codexPillVisible();
    claudePill.el.style.display = claudeOn ? "" : "none";
    codexPill.el.style.display = codexOn ? "" : "none";
    if (claudeOn) claudePill.sync();
    if (codexOn) codexPill.sync();
    // Codex is asked when its pill comes into view (stale answers only).
    const shown = codexOn && State.mode === "expanded";
    if (shown && !codexShown) refreshCodexPlanUsage();
    codexShown = shown;
  }

  function syncLevel() {
    const n = actions.level();
    level.classList.toggle("on", n != null);
    right.classList.toggle("off", n != null);
    if (!n) return;
    const name = n.icon ?? "volume";
    if (name !== levelIcon || !level.firstChild) {
      levelIcon = name;
      fill(level, icon(name, 14), levelBar, levelNum);
    }
    setMeter(levelBar, n.level ?? 0);
    levelNum.textContent = String(Math.round((n.level ?? 0) * 100));
  }

  return {
    el,
    sync() {
      const v = State.view;
      tabHome.classList.toggle("on", v === "overview" || v === "empty");
      tabChat.classList.toggle("on", v === "prompt");
      tabDrop.classList.toggle("on", v === "upload");
      tabChat.style.display = actions.featureOn("chat") ? "" : "none";
      tabDrop.style.display = actions.featureOn("drop") ? "" : "none";
      galleryBtn.classList.toggle("on", v === "gallery");
      gearBtn.classList.toggle("on", v === "settings");
      const soundOn = State.settings.soundEnabled;
      if (soundKey !== `${soundOn}~${language()}`) {
        soundKey = `${soundOn}~${language()}`;
        const label = t(soundOn ? SOUNDS_ON : SOUNDS_OFF, { name: BRAND.name });
        fill(soundBtn, icon(soundOn ? "volume" : "mute", 13));
        soundBtn.title = label;
        soundBtn.setAttribute("aria-label", label);
        soundBtn.setAttribute("aria-pressed", String(soundOn));
        soundBtn.classList.toggle("muted", !soundOn);
      }
      syncSectionTabs();
      syncPlanPills();
      syncLevel();
      el.classList.toggle("gone", v === "confused");
    },
  };
}

// ── Overview ──────────────────────────────────────────────────────────────────

/** One of the four pills beside the focused one; updated in place so its mini mascot lives on. */
function buildPill(task: AgentTask, actions: ViewActions) {
  const name = h("b");
  const line = h("span");
  const mark = taskMark(task, 22);
  const el = h("button", { class: "card isl-pill", onclick: () => actions.setFocus(task.id) }, mark, name, line);
  const dotEl = mark.querySelector<HTMLElement>(".dot-mark");
  return {
    el,
    sync(now: AgentTask) {
      // An amber ring while its agent waits for the user.
      el.classList.toggle("waiting", needsYou(now));
      const label = pillLabel(now);
      if (name.textContent !== label) name.textContent = label;
      const { text, tone } = taskLine(now);
      if (line.textContent !== text) line.textContent = text;
      line.className = tone ?? "";
      el.title = text;
      if (dotEl) setDotMark(dotEl, now.color, now.state);
    },
  };
}

function buildOverview(actions: ViewActions): ViewHost {
  /** The diff open in the detail card (a FileDiff id). */
  let activeDiffId: number | null = null;
  /** The session code view (views/codeview.ts): open, pinned to a diff, or following the newest. */
  let codeOpen = false;
  let codePinned: number | null = null;
  let codeFollow = false;
  const closeDiff = () => {
    if (activeDiffId == null) return;
    activeDiffId = null;
    State.notify();
  };

  // The focused pill: the mascot, its name, what it is doing.
  // The name is what the strip shows too: it travels from there (§4).
  const name = h("b", { "data-shared": "title" });
  const line = h("span", { class: "focus-line" });
  const meta = h("small");
  const chips = h("div", { class: "isl-chips" });
  const jump = h(
    "button",
    { class: "btn sm ghost icon jump", title: tl("Open"), onclick: () => actions.openTarget() },
    icon("external", 12),
  );
  const focus = h("div", { class: "isl-focus" }, markSlot(58), name, line, meta, chips, jump);
  // A click on a step that is an edit opens its diff.
  line.addEventListener("click", () => {
    const step = State.focusTask?.steps.at(-1);
    const diff = step ? parseDiffStep(step) : null;
    if (!diff) {
      // Not on an edit: the code view, when the session has an edit or a command to show.
      const task = State.focusTask;
      if (!task || (!task.lastCommand && !State.sessionDiffs.get(task.id)?.length)) return;
      actions.blip();
      codeOpen = true;
      codePinned = State.sessionDiffs.get(task.id)?.at(-1)?.id ?? null;
      State.notify();
      return;
    }
    actions.blip();
    activeDiffId = diff.diffId;
    State.notify();
  });

  // Beside it: what the pill has to show (a service's card, a diff, plan usage)
  // when it has something, and the other pills.
  const detailBody = h("div", { class: "left-body" });
  const detail = h("div", { class: "card ov-detail" }, detailBody);
  const pills = h("div", { class: "isl-pills" });
  const el = h("div", { class: "view overview" }, focus, h("div", { class: "ov-right" }, detail, pills));

  const plan = new PlanCard();
  let planTimer: number | null = null;
  // The sessions of the focused pill's agent, when it runs more than one.
  const sessionList = buildSessionList(actions);

  let pillIds = "";
  let pillHosts: ReturnType<typeof buildPill>[] = [];
  let detailOpen = false;
  let lastFocus: string | null = null;
  let mode: "card" | "plan" | "diff" | "siblings" | null = null;
  let cardKey = "";
  let idleKey = "";
  /** The list row highlighted at the last sync, to scroll only when it moves. */
  let shownSelection: number | null = null;

  // Leaving the overview or folding the island closes the diff.
  State.subscribe(() => {
    if (activeDiffId != null && (State.view !== "overview" || State.mode !== "expanded")) {
      activeDiffId = null;
    }
    if (codeOpen && (State.view !== "overview" || State.mode !== "expanded")) codeOpen = false;
  });
  // Escape steps back out of the diff before it closes the island.
  window.addEventListener(
    "keydown",
    (e) => {
      if (e.key !== "Escape" || activeDiffId == null || State.view !== "overview") return;
      e.stopImmediatePropagation();
      closeDiff();
    },
    true,
  );

  const hooks: IntegrationCardHooks = {
    get detailOpen() {
      return detailOpen;
    },
    openDetail() {
      detailOpen = true;
      cardKey = "";
      State.notify();
    },
    closeDetail() {
      detailOpen = false;
      cardKey = "";
      State.notify();
    },
    openSettings: () => actions.openSettingsWindow(),
  };

  /** The countdowns move every 30 s while a card is open, and only then. */
  function syncPlanTimer(open: boolean) {
    const stop = () => {
      if (planTimer != null) window.clearInterval(planTimer);
      planTimer = null;
    };
    if (!open) return stop();
    if (planTimer != null) return;
    planTimer = window.setInterval(() => {
      if (planCardOpen() && State.mode === "expanded") State.notify();
      else stop();
    }, 30_000);
  }

  function syncFocus(task: AgentTask | null) {
    const label = task ? pillLabel(task) : "";
    if (name.textContent !== label) name.textContent = label;
    jump.style.display = task ? "" : "none";
    syncChips(chips, task);
    if (!task) {
      line.textContent = "";
      clear(meta);
      idleKey = "";
      return;
    }
    const session = isSessionPill(task);
    const idle = task.state === "idle" && task.steps.length === 0;
    // A pill made for one more session has nothing to set up: that is its agent's own pill's.
    if (session && idle && !task.family) {
      // Nothing running: whether it is set up, and the way to set it up.
      const info = State.integrations[task.id];
      const status = idleStatus(task.id, info, State.settings.model);
      line.textContent = status.label;
      line.className = `focus-line ${status.ok ? "" : "bad"}`;
      const key = `${language()}~${task.id}~${info?.configured}~${task.color}`;
      if (key !== idleKey) {
        idleKey = key;
        fill(meta, idleActions(task, hooks.openSettings, true));
      }
      return;
    }
    idleKey = "";
    const { text, tone } = taskLine(task);
    if (line.textContent !== text) line.textContent = text;
    const step = task.steps.at(-1);
    const busy = task.state === "working" || task.state === "thinking" || task.state === "searching";
    line.className = ["focus-line", tone, busy && "shimmer", (step && parseDiffStep(step) || task.lastCommand || State.sessionDiffs.get(task.id)?.length) && "link"].filter(Boolean).join(" ");
    line.title = text;
    const project = task.sessionCwd?.split(/[\\/]/).filter(Boolean).pop();
    const kind = t(session ? sessionSubtitle(familyOf(task)) : (pillDefinition(task.id)?.subtitle ?? N_("Integration")));
    const count = task.steps.length > 1 ? `${Math.min(task.stepIndex + 1, task.steps.length)}/${task.steps.length}` : "";
    meta.textContent = metaLine(session, text, project, kind, count);
  }

  return {
    el,
    command(command) {
      if (command === "openSelection") {
        // Ctrl+O: what a click on the highlighted row does.
        if (mode !== "card" || State.cardSelection == null) return;
        openRow(listRows(detailBody)[State.cardSelection]);
        return;
      }
      // Ctrl+E: closes the diff that is open, else opens the focused pill's latest edit.
      if (activeDiffId != null) {
        closeDiff();
        return;
      }
      const task = State.focusTask;
      const diffs = task ? State.sessionDiffs.get(task.id) : undefined;
      const last = diffs?.[diffs.length - 1];
      if (last?.id == null) return;
      activeDiffId = last.id;
      State.notify();
    },
    sync() {
      const task = State.focusTask;
      if (task?.id !== lastFocus) {
        lastFocus = task?.id ?? null;
        detailOpen = false;
        activeDiffId = null;
        codeOpen = false;
        cardKey = "";
        mode = null;
        clear(detailBody);
      }
      syncFocus(task);

      const planOpen = planCardOpen();
      if (mode === "plan" && !planOpen) {
        mode = null;
        cardKey = "";
      }
      syncPlanTimer(planOpen);

      // A diff that has since been dropped (cap, expiry, session end) just closes.
      const diff = task && activeDiffId != null ? State.findDiff(task.id, activeDiffId) : null;
      if (!diff) activeDiffId = null;

      if (planOpen) {
        if (mode !== "plan") {
          fill(detailBody, plan.el);
          mode = "plan";
        }
        plan.sync();
      } else if (task && diff) {
        const key = `diff~${task.id}~${diff.id}`;
        if (key !== cardKey) {
          cardKey = key;
          mode = "diff";
          fill(detailBody, buildDiffCard(diff, {
            dismiss: () => {
              actions.blip();
              closeDiff();
            },
            open: (path) => void Bridge.openFileInVSCode(path),
          }));
        }
      } else if (task && codeOpen) {
        const all = State.sessionDiffs.get(task.id) ?? [];
        const shown = (codeFollow ? null : all.find((d) => d.id === codePinned)) ?? all.at(-1) ?? null;
        const key = `code~${task.id}~${shown?.id}~${task.lastCommand}~${codeFollow}`;
        if (key !== cardKey) {
          cardKey = key;
          mode = "diff";
          fill(detailBody, buildCodeView(shown, task.lastCommand, {
            dismiss: () => {
              actions.blip();
              codeOpen = false;
              State.notify();
            },
            open: (path) => void Bridge.openFileInVSCode(path),
            follow: codeFollow,
            setFollow: (on) => {
              codeFollow = on;
              State.notify();
            },
            lines: (path, from, count) => Bridge.sessionFileLines(task.sessionCwd ?? "", path, from, count),
          }));
        }
      } else if (task && !isSessionPill(task)) {
        // A service shows its own data.
        const info = State.integrations[task.id];
        const key = [
          language(), task.id, task.color, detailOpen, task.state, task.steps.join("|"),
          info?.loaded, info?.error, info?.configured,
          JSON.stringify(info?.data ?? {}),
        ].join("~");
        if (key !== cardKey) {
          cardKey = key;
          mode = "card";
          fill(detailBody, renderIntegrationCard(task, hooks));
        }
      } else if (task && sessionTasks(task).length > 1) {
        // An agent with several sessions lists them, the ones that need you first.
        if (mode !== "siblings") {
          fill(detailBody, sessionList.el);
          mode = "siblings";
          cardKey = "";
        }
        sessionList.sync(sessionTasks(task), task.id);
      } else if (mode != null) {
        // A session's line is the focus column's: nothing beside it but the pills.
        mode = null;
        cardKey = "";
        clear(detailBody);
      }
      detail.style.display = mode ? "" : "none";
      pills.classList.toggle("compact", mode != null);

      // Ctrl+↓ Ctrl+↑ walk the open GitHub list (cardItemCount / cardSelection).
      const rows = mode === "card" ? listRows(detailBody) : [];
      State.cardItemCount = rows.length;
      if (State.cardSelection != null && State.cardSelection >= rows.length) State.cardSelection = null;
      highlightRow(rows, State.cardSelection, State.cardSelection !== shownSelection);
      shownSelection = State.cardSelection;

      // Every other pill: four fit, the rest by scrolling.
      const others = State.otherTasks;
      const pillKey = `${State.settings.showMascot}~${others.map((x) => `${x.id}:${x.color}`).join("|")}`;
      if (pillKey !== pillIds) {
        pillIds = pillKey;
        pillHosts = others.map((x) => buildPill(x, actions));
        fill(pills, ...pillHosts.map((p) => p.el));
        pruneMiniBots();
      }
      pillHosts.forEach((p, i) => p.sync(others[i]));
    },
  };
}

// ── Empty ─────────────────────────────────────────────────────────────────────

function buildEmpty(actions: ViewActions): ViewHost {
  const ask = btn(tl("Ask Claude"), "primary", () => actions.setView("prompt"));
  const body = h(
    "div",
    { class: "isl-row with-mark" },
    h(
      "div",
      { class: "texts" },
      h("div", { class: "title", text: tl("Nothing running right now.") }),
      h("div", { class: "sub", text: tl("Drop a file or window, or ask me anything.") }),
    ),
    h("span", { class: "isl-gap" }),
    ask,
  );
  return {
    el: h("div", { class: "view" }, alertCard(undefined, body)),
    sync() {
      ask.style.display = actions.featureOn("chat") ? "" : "none";
    },
  };
}

// ── Approval ──────────────────────────────────────────────────────────────────

/** How long a fresh permission card ignores clicks on its buttons. */
const CLICK_GUARD_MS = 600;

/**
 * The ⌃ in the corner of a waiting card: folds the island to its compact size
 * and leaves the request waiting — nothing is answered. Opening the island
 * again brings the card back.
 */
function foldButton(actions: ViewActions): HTMLElement {
  return h(
    "button",
    { class: "btn sm ghost icon fold", title: tl("Later — keep it waiting"), onclick: () => actions.foldApproval() },
    icon("chevu", 12),
  );
}

function buildApproval(actions: ViewActions): ViewHost {
  const who = h("div", { class: "isl-row" });
  const command = h("code");
  const where = h("span");
  // The card pops up under a cursor that was busy with something else: a click
  // meant for the window underneath must not land on Allow. Clicks in the first
  // moments after a new request appears are ignored. The same holds for the
  // next request of the queue coming up under the click that answered the one
  // before: a click only ever answers the request the card was drawn for.
  // There is no key here: a permission card is answered by a click, never a key.
  let shownFor: string | null = null;
  let shownAt = 0;
  const guarded = (d: Decision) => (click?: MouseEvent) => {
    const pill = State.pendingApproval?.pillId;
    if (!byPointer(click) || !shownFor || performance.now() - shownAt < CLICK_GUARD_MS) return;
    actions.decide(d, shownFor);
    // Change plan stops the turn: the change is typed in the session's terminal, brought forward.
    if (d === "plan_change" && pill) actions.openSessionOf(pill);
  };
  // Two buttons, built once: rebuilding them between a mouse-down and a
  // mouse-up would swallow the click.
  // Always (chat-plus): the agent's own suggestion goes back with the allow, so the
  // next identical request is not asked. Claude Code only, and only for a request
  // that came with a suggestion; the relay does the rest.
  const always = btn(tl("Always"), "secondary", (click?: MouseEvent) => {
    const id = shownFor;
    if (!byPointer(click) || !id || performance.now() - shownAt < CLICK_GUARD_MS) return;
    void Bridge.featureCall("chat-plus", "always", { requestId: id }).then(() => actions.decide("allow", id));
  });
  const row = h(
    "div",
    { class: "isl-row" },
    btn(tl("Allow"), "primary", guarded("allow")),
    always,
    h("span", { class: "isl-gap" }),
    btn(tl("Deny"), "danger", guarded("deny")),
  );
  // A plan (Claude Code's ExitPlanMode) is no tool to allow: the terminal's own
  // choices, approved in auto mode or approving each edit, or changed.
  const planRow = h(
    "div",
    { class: "isl-row" },
    btn(tl("Auto mode"), "primary", guarded("plan_auto")),
    btn(tl("Manual"), "secondary", guarded("plan_manual")),
    h("span", { class: "isl-gap" }),
    btn(tl("Change plan"), "secondary", guarded("plan_change")),
  );
  // Tab does not stop on them either: a button no key can press shows no focus ring.
  for (const b of [...row.querySelectorAll("button"), ...planRow.querySelectorAll("button")]) b.tabIndex = -1;
  const el = h("div", { class: "view" },
    alertCard("warn", who, h("div", { class: "isl-code" }, command, where), row, planRow, foldButton(actions)));
  return {
    el,
    sync() {
      const req = State.pendingApproval?.requestId ?? null;
      if (req !== shownFor) {
        shownFor = req;
        shownAt = performance.now();
      }
      const pending = State.pendingApproval;
      // Always is Claude Code's own rule: not for Codex, Copilot CLI or Muse Code.
      const claude = pending != null && !pending.family.startsWith("agent_");
      always.style.display = featureOn("chat-plus") && pending?.suggested && claude ? "" : "none";
      const plan = pending?.tool === "ExitPlanMode";
      row.style.display = plan ? "none" : "";
      planRow.style.display = plan ? "" : "none";
      whoRow(who, cardTask(), plan ? t("has a plan ready") : t("needs permission"), queueCount());
      // The whole point of approving here rather than in the terminal: this line
      // is the command, the file path or the URL being authorised, not just the
      // name of the tool asking.
      command.textContent = State.pendingApproval?.command || State.pendingApproval?.tool || "…";
      where.textContent = cardTask()?.sessionCwd ?? "";
    },
  };
}

// ── Question ──────────────────────────────────────────────────────────────────

// The card as the terminal has it (ported from upstream `windows/src/views/
// views.ts`, pull request 335): pick an option, "Other…" to answer in words,
// Back to the question before, 1–9 and Enter. Where the user is in the request
// is a QuestionFlow (core/question.ts); this draws it.
function buildQuestion(actions: ViewActions): ViewHost {
  const who = h("div", { class: "isl-row" });
  const title = h("div", { class: "title question-text" });
  const row = h("div", { class: "isl-row options" });
  const fold = foldButton(actions);
  const el = h("div", { class: "view" }, alertCard("info", who, title, row, fold));
  const words = field({ placeholder: tl("Type your answer…") });

  let requestId = "";
  let flow: QuestionFlow | null = null;
  // A card that has just come up ignores clicks and keys for a moment, like the
  // permission card: what was meant for the window underneath answers nothing.
  let shownAt = 0;
  const fresh = () => performance.now() - shownAt < CLICK_GUARD_MS;
  // The buttons are only rebuilt when what they show changes: rebuilding them
  // between a mouse-down and a mouse-up would swallow the click.
  let rowKey = "";

  /** After a step: the request is answered once its last question is, for the request it was drawn for. */
  const settle = () => {
    if (flow?.done) actions.answer(flow.answers, [...flow.other], requestId);
    else State.notify();
  };

  // 1–9 pick an option and Enter confirms a pick-several question, while the
  // pointer is on the card or the island has the keyboard by a click. Ctrl+digit
  // stays the pill shortcut, and what is typed under "Other…" is the field's.
  window.addEventListener("keydown", (e) => {
    if (State.mode !== "expanded" || fresh()) return;
    const inField = (e.target as Element | null)?.matches?.("input, textarea, [contenteditable]") === true;
    const act = questionKey(e, {
      view: State.view, allowed: actions.keysAllowed(), inField,
      flow: State.pendingApproval?.requestId === requestId ? flow : null,
    });
    if (!act || !flow) return;
    e.preventDefault();
    if (act === "confirm") flow.confirm();
    else flow.choose(act.choose);
    settle();
  });

  return {
    el,
    keys: () => (State.pendingApproval?.requestId === requestId ? questionKeyNames(flow) : []),
    sync() {
      const pending = State.pendingApproval;
      const questions = pending?.questions;
      // Only a request that is waiting can be folded away and come back.
      fold.style.display = pending ? "" : "none";

      // A question that arrived as a notification has nothing to pick from.
      if (!pending || !questions) {
        flow = null;
        requestId = "";
        whoRow(who, State.focusTask, t("is asking a question"));
        const task = State.focusTask;
        title.textContent = (task && lastTextStep(task.steps)) ?? t("Claude needs an answer.");
        if (rowKey !== "terminal") {
          rowKey = "terminal";
          fill(row, h("div", { class: "sub", text: tl("Answer it in your terminal.") }));
        }
        return;
      }

      if (pending.requestId !== requestId || !flow) {
        requestId = pending.requestId;
        flow = new QuestionFlow(questions);
        shownAt = performance.now();
        words.input.value = "";
      }
      const at = Math.min(flow.index, questions.length - 1);
      const q = questions[at];
      const asking = questions.length > 1
        ? t("is asking ({index} of {total})", { index: at + 1, total: questions.length })
        : t("is asking");
      whoRow(who, cardTask(), asking, queueCount());
      title.textContent = q.question;
      title.title = q.question;

      const key = `${requestId}:${flow.index}:${flow.typing}:${[...flow.picked].join("|")}`;
      if (rowKey === key) return;
      rowKey = key;
      clear(row);
      // Until the next question is drawn these buttons are still on screen: a
      // second click in that frame must not answer the next question with them.
      const drawn = flow;
      const live = (f: () => void) => () => {
        if (flow !== drawn || drawn.index !== at || fresh()) return;
        f();
        settle();
      };
      const toTerminal = btn(tl("Answer in terminal"), "ghost", () => actions.answerInTerminal(requestId));

      if (drawn.typing) {
        const send = live(() => drawn.submitOther(words.input.value));
        // Enter sends; Escape goes back to the options rather than folding the
        // card. Every other key is the field's own and goes on to the island.
        words.input.onkeydown = (e) => {
          if (e.key !== "Enter" && e.key !== "Escape") return;
          e.preventDefault();
          e.stopPropagation();
          if (e.key === "Enter") send();
          else live(() => drawn.closeOther())();
        };
        row.append(words, btn(tl("OK"), "primary", send), btn(tl("Back"), "ghost", live(() => drawn.closeOther())), toTerminal);
        // The click on "Other…" gave the island the keyboard (§4.1): the field takes the text cursor.
        words.input.focus();
        return;
      }

      q.options.forEach((option, i) => {
        const on = drawn.picked.has(option.label);
        const button = btn(option.label, on ? "primary" : "secondary", live(() => drawn.choose(option.label)), i < 9 ? String(i + 1) : undefined);
        if (option.description) button.title = option.description;
        row.append(button);
      });
      row.append(btn(tl("Other…"), "secondary", live(() => drawn.openOther())));
      if (q.multiSelect) {
        const last = at === questions.length - 1;
        const done = btn(last ? tl("Done") : tl("Next"), "primary", live(() => drawn.confirm()));
        if (drawn.picked.size === 0) done.classList.add("disabled");
        row.append(done);
      }
      if (at > 0) row.append(btn(tl("Back"), "ghost", live(() => drawn.back())));
      row.append(toTerminal);
    },
  };
}

// ── Error ─────────────────────────────────────────────────────────────────────

function buildError(actions: ViewActions): ViewHost {
  const who = h("div", { class: "isl-row" });
  const title = h("div", { class: "title one-line" });
  const detail = h("div", { class: "detail" });
  const row = h("div", { class: "isl-row" },
    btn(tl("Retry"), "primary", () => actions.setView(State.defaultView())),
    btn(tl("OK"), "secondary", () => actions.collapse()),
  );
  const el = h("div", { class: "view" },
    alertCard("bad", who, h("div", { class: "texts" }, title, detail), row));
  return {
    el,
    sync() {
      const task = State.focusTask;
      // The row already shows an agent's name: its label is just the kind.
      const label = task?.source === "n8n" ? "n8n" : task?.source === "agent" ? t("Agent") : "Claude Code";
      whoRow(who, task, label);
      title.textContent = task?.source === "n8n" ? t("Workflow stopped.") : t("Session stopped on an error.");
      detail.textContent = (task && lastTextStep(task.steps)) ?? t("No detail available.");
    },
  };
}

// ── Finished ──────────────────────────────────────────────────────────────────

/** What the button that leads to a session says: the app it lives in, or its terminal. */
export function openLabel(task: AgentTask | null): string {
  const owner = task ? sessionOwner(familyOf(task)) : null;
  // Sessions from the Claude desktop app live there, not in a terminal.
  if (owner?.id === "claude-desktop") return t("Open Claude");
  return owner ? t("Open {name}", { name: owner.name }) : t("Open terminal");
}

/** The finished card of a task shows its whole answer, in a box that scrolls: the island grows for it. */
export function finishedIsTall(task: AgentTask | null): boolean {
  return isLongAnswer(task?.finalText);
}

function buildFinished(actions: ViewActions): ViewHost {
  const who = h("div", { class: "isl-row" });
  const chips = h("span", { class: "isl-chips" });
  // One line of the agent's last words, with its bold, code and web links; or,
  // when one line does not hold them, all of them in a box that scrolls. Both
  // are built from text nodes: nothing an agent writes is ever read as markup.
  const title = h("div", { class: "title one-line" });
  const answer = h("div", { class: "isl-answer" });
  const open = btn(tl("Open terminal"), "primary", () => actions.openTerminal());
  const row = h("div", { class: "isl-row" }, open, btn(tl("OK"), "secondary", () => actions.collapse()));
  const el = h("div", { class: "view" }, alertCard("ok", who, h("div", { class: "texts" }, title, answer), row));
  let shown: string | null = null;
  return {
    el,
    sync() {
      const task = State.focusTask;
      syncChips(chips, task);
      whoRow(who, task, task?.source === "agent" ? t("finished") : t("Claude Code finished"), chips);
      const whole = task?.finalText ?? "";
      const tall = isLongAnswer(whole);
      // The final message, else the last step that is not a diff.
      const line = firstParagraph(whole) || task?.finalLine || (task && lastTextStep(task.steps)) || t("Session finished");
      const key = `${task?.id}~${tall}~${tall ? whole : line}`;
      if (key !== shown) {
        shown = key;
        if (tall) {
          renderMarkdown(answer, whole);
          answer.scrollTop = 0;
        } else {
          answer.replaceChildren();
          renderInline(title, line);
        }
        title.style.display = tall ? "none" : "";
        answer.style.display = tall ? "" : "none";
      }
      const label = openLabel(task);
      const span = open.firstElementChild as HTMLElement;
      if (span.textContent !== label) span.textContent = label;
    },
  };
}

// ── Confused, note, placeholders: the mascot and a line or two ────────────────

function plainCard(tone: Tone | true | undefined, title: Node, sub?: Node): HTMLElement {
  return h("div", { class: "view" },
    alertCard(tone, h("div", { class: "isl-row with-mark" }, h("div", { class: "texts" }, title, sub ?? null))));
}

function buildConfused(): ViewHost {
  const el = plainCard(
    true,
    h("div", { class: "title", text: tl("Too many hits at once.") }),
    h("div", { class: "sub", text: tl("Give me a sec — back to work in three seconds.") }),
  );
  return { el, sync() {} };
}

function buildNote(): ViewHost {
  const title = h("div", { class: "title" });
  return {
    el: plainCard(undefined, title),
    sync() {
      title.textContent = State.noteMessage ?? "";
    },
  };
}

function buildPlaceholder(title: Msg, sub: string): ViewHost {
  const el = plainCard(undefined, h("div", { class: "title", text: title }), h("div", { class: "sub", text: sub }));
  return { el, sync() {} };
}

// ── In-island settings ────────────────────────────────────────────────────────

function buildSettings(actions: ViewActions): ViewHost {
  const soundSwitch = h("button", { class: "switch", onclick: () => actions.toggleSound() });
  const volume = h("input", {
    type: "range", min: "0", max: "0.5", step: "0.005",
    oninput: (e: Event) => actions.setVolume(Number((e.target as HTMLInputElement).value)),
  }) as HTMLInputElement;
  const autoLabel = h("span", {});
  const segButtons = [10, 15, 30].map((s) =>
    h("button", { class: "tab", onclick: () => actions.setAutoClose(s) }, `${s}s`),
  );
  const claudeBadge = h("span", { class: "status-badge" });
  const apiBadge = h("span", { class: "status-badge" });

  const rows = h(
    "div",
    { class: "settings-rows" },
    h("div", { class: "settings-row" }, icon("volume", 16), h("span", { text: tl("Sound") }), h("span", { class: "isl-gap" }), volume, soundSwitch),
    h(
      "div",
      { class: "settings-row" },
      icon("timer", 16),
      autoLabel,
      h("span", { class: "isl-gap" }),
      ...segButtons,
    ),
    h(
      "div",
      { class: "settings-row" },
      claudeBadge,
      apiBadge,
      h("span", { class: "isl-gap" }),
      btn(tl("Settings…"), "secondary", () => actions.openSettingsWindow()),
    ),
  );

  const el = h("div", { class: "view" }, card(rows));

  return {
    el,
    sync() {
      const s = State.settings;
      soundSwitch.classList.toggle("on", s.soundEnabled);
      volume.value = String(s.soundVolume);
      volume.style.opacity = s.soundEnabled ? "1" : "0.4";
      autoLabel.textContent = t("Auto-close · {seconds}s", { seconds: Math.round(s.autoCloseInterval) });
      segButtons.forEach((b, i) => b.classList.toggle("on", s.autoCloseInterval === [10, 15, 30][i]));
      fill(claudeBadge, dot(s.hooksInstalled ? "var(--ok)" : "var(--bad)", 6), h("span", { text: "Claude Code" }));
      fill(apiBadge, dot("var(--bad)", 6), h("span", { text: "API" }));
    },
  };
}

// ── Registry ──────────────────────────────────────────────────────────────────

export function buildViews(
  actions: ViewActions,
  onChatHeightChange: () => void,
): Map<IslandViewName, ViewHost> {
  const map = new Map<IslandViewName, ViewHost>();
  map.set("overview", buildOverview(actions));
  map.set("empty", buildEmpty(actions));
  map.set("approval", buildApproval(actions));
  map.set("question", buildQuestion(actions));
  map.set("error", buildError(actions));
  map.set("finished", buildFinished(actions));
  map.set("confused", buildConfused());
  map.set("note", buildNote());
  map.set("settings", buildSettings(actions));
  map.set("prompt", buildPrompt(onChatHeightChange));
  map.set("upload", buildUpload());
  map.set("uploading", buildUploading());
  map.set("choose", buildChoose(actions));
  map.set("recap", buildRecap(actions));
  map.set("gallery", buildGallery((key) => actions.openSection(key)));
  // Not in the Windows v1: sending a file by email, window attach + web result.
  map.set("mail", buildPlaceholder(tl("Sending by email isn't in this version."), ""));
  map.set("searching", buildPlaceholder(tl("Claude is searching…"), ""));
  map.set("result", buildPlaceholder(tl("Result"), ""));
  // "section" has no entry: src/island/sections.ts owns those elements.
  return map;
}
