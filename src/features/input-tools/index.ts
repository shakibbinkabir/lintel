// Input tools — the page side of the feature (docs/ARCHITECTURE.md §2.3).
// Nothing runs at import: declare surfaces here, begin work in `start`.
//
// Settings for every tool of the package (snippets, debounce, remaps, blocked
// shortcuts, scrolling, mouse buttons, pointer acceleration), what is hooked
// right now, and the snippet library window. Keys are heard and typed by Rust
// (crates/input-tools on the hub of crates/svc-input); the page only edits the
// settings and shows a recorded combination when Rust reports one.

import "./style.css";
import { Bridge } from "../../core/bridge";
import { defineFeature, type FeatureCtx } from "../../core/feature";
import { effective, SHORTCUT_TEXT, SHORTCUTS } from "../../core/shortcuts";
import { N_, tn } from "../../i18n/i18n";
import { btn, capsule, card, field, iconBtn, kbd, menu, row, toggle } from "../../ui/components";
import { icon } from "../../ui/icons";
import { h } from "../../views/dom";

const P = "f-input-tools-";
/** A mouse, for the hook's row: no icon of the shared set says it. */
const MOUSE = "M12 3.5a5 5 0 0 1 5 5v7a5 5 0 0 1-10 0v-7a5 5 0 0 1 5-5zM12 7v3.5";

type S = Record<string, unknown>;

interface Snip {
  id: string;
  name: string;
  trigger: string;
  text: string;
  /** After a space, Tab or Enter, or on the trigger's last character. */
  mode: "delimiter" | "immediate";
  on: boolean;
  ignoreCase: boolean;
  folder: string;
  library: boolean;
}

interface Remap {
  from: string;
  /** Empty: the combination does nothing. */
  to: string;
  on: boolean;
}

interface Status {
  keyboard: boolean;
  mouse: boolean;
  suspended: boolean;
  elevated: boolean;
  stages: string[];
  /** Windows' own pointer acceleration as it is now. */
  acceleration: boolean | null;
}

const snips = (s: S): Snip[] =>
  (Array.isArray(s.snippetList) ? (s.snippetList as Partial<Snip>[]) : []).map((v) => ({
    id: String(v.id ?? ""),
    name: String(v.name ?? ""),
    trigger: String(v.trigger ?? ""),
    text: String(v.text ?? ""),
    mode: v.mode === "immediate" ? "immediate" : "delimiter",
    on: v.on !== false,
    ignoreCase: v.ignoreCase === true,
    folder: String(v.folder ?? ""),
    library: v.library !== false,
  }));

const strings = (v: unknown): string[] => (Array.isArray(v) ? v.map(String) : []);

/** Case and accents folded, so "resume" finds "Résumé". */
const fold = (s: string) => s.normalize("NFKD").replace(/\p{M}/gu, "").toLowerCase();

const found = (list: Snip[], query: string) => {
  const tokens = fold(query).split(/\s+/).filter(Boolean);
  return list.filter((x) => tokens.every((tok) => fold(`${x.name} ${x.trigger} ${x.text} ${x.folder}`).includes(tok)));
};

// ── Keys ──────────────────────────────────────────────────────────────────────

/** How a key's stored name is shown. Key names are not translated. */
const GLYPH: Record<string, string> = {
  Left: "←", Right: "→", Up: "↑", Down: "↓",
  Semicolon: ";", Equal: "=", Comma: ",", Minus: "-", Period: ".", Slash: "/", Backquote: "`",
  BracketLeft: "[", Backslash: "\\", BracketRight: "]", Quote: "'",
  CapsLock: "Caps Lock", PageUp: "Page Up", PageDown: "Page Down", PrintScreen: "Print Screen",
  ScrollLock: "Scroll Lock", NumLock: "Num Lock", VolumeMute: "Mute", VolumeDown: "Volume −", VolumeUp: "Volume +",
  MediaNext: "Next track", MediaPrevious: "Previous track", MediaStop: "Stop", MediaPlay: "Play/Pause",
};

/** A combination as keycaps: "Ctrl+Shift+Left" → Ctrl Shift ←. */
function keysOf(chord: string): HTMLElement {
  return h("span", { class: `${P}keys` }, ...chord.split("+").map((k) => kbd(GLYPH[k] ?? k)));
}

/** The recorder that is listening, if any: one at a time. */
let stopRecording: (() => void) | null = null;

/**
 * A button that shows a combination and records a new one. Rust hears the keys
 * (the hub swallows them meanwhile, so the Windows key, Alt+F4 and keys that
 * are remapped can be recorded) and reports the combination.
 */
function recorder(cx: FeatureCtx, value: string, empty: string, onChange: (chord: string) => void): HTMLButtonElement {
  const b = h("button", { class: `btn secondary sm ${P}rec`, title: cx.t("Click, then press the keys") });
  const show = () => b.replaceChildren(value ? keysOf(value) : h("span", { text: empty }));
  show();
  b.addEventListener("click", () => {
    if (b.classList.contains("recording")) return stopRecording?.();
    stopRecording?.();
    const stop = () => {
      window.clearTimeout(timer);
      window.removeEventListener("blur", stop);
      off();
      stopRecording = null;
      b.classList.remove("recording");
      show();
      void cx.call("record-stop");
    };
    const off = cx.on<{ chord: string }>("recorded", ({ chord }) => {
      value = chord;
      stop();
      onChange(chord);
    });
    // Rust stops listening after ten seconds whatever the page does.
    const timer = window.setTimeout(stop, 10_000);
    stopRecording = stop;
    window.addEventListener("blur", stop);
    b.classList.add("recording");
    b.replaceChildren(h("span", { text: cx.t("Press keys…") }));
    void cx.call("record");
  });
  return b;
}

// ── Settings ──────────────────────────────────────────────────────────────────

/** The suggestions of the remap card: each adds its rules and replaces a rule on the same keys. */
const PRESETS: { name: string; hint: string; rules: [string, string][] }[] = [
  { name: N_("Caps Lock as Esc"), hint: N_("Shift with Caps Lock still turns capitals on and off."), rules: [["CapsLock", "Esc"]] },
  { name: N_("Caps Lock as Ctrl"), hint: N_("Caps Lock acts as Ctrl. It replaces Caps Lock as Esc, since both use the same key."), rules: [["CapsLock", "Ctrl"]] },
  {
    name: N_("Home and End as on a Mac"),
    hint: N_("Home and End go to the start and the end of the document, and select up to there with Shift."),
    rules: [["Home", "Ctrl+Home"], ["End", "Ctrl+End"], ["Shift+Home", "Ctrl+Shift+Home"], ["Shift+End", "Ctrl+Shift+End"]],
  },
  { name: N_("A safer close shortcut"), hint: N_("Alt+F4 does nothing. Alt+Shift+F4 closes the window."), rules: [["Alt+F4", ""], ["Alt+Shift+F4", "Alt+F4"]] },
];

const STAGE: Record<string, string> = {
  recorder: N_("Recording a shortcut"),
  keyDebounce: N_("Doubled key presses"),
  guard: N_("Blocked shortcuts"),
  remaps: N_("Remaps"),
  snippets: N_("Snippets"),
  clickDebounce: N_("Doubled clicks"),
  buttons: N_("Mouse buttons"),
  wheel: N_("Scrolling"),
};
const MOUSE_STAGES = ["snippets", "clickDebounce", "buttons", "wheel"];

function settingsBody(cx: FeatureCtx): HTMLElement {
  const t = cx.t;
  const el = h("div", { class: `${P}settings` });
  const status = card([]);
  const shortcuts = card([]);
  let s: S = {};
  /** The snippet in the editor, and whether it is a new one. */
  let editing: { snip: Snip; fresh: boolean } | null = null;
  let rule: Remap = { from: "", to: "", on: true };

  const refreshStatus = async () => {
    const st = await cx.call<Status>("status");
    if (!st) return;
    const used = (stages: string[]) => stages.map((name) => t(STAGE[name] ?? name)).join(", ") || t("No tool needs it.");
    const hook = (on: boolean, name: string) => capsule({ icon: name, text: on ? t("Installed") : t("Not installed"), tone: on ? "ok" : undefined });
    status.replaceChildren(
      row(t("Keyboard hook"), hook(st.keyboard, "keyboard"), used(st.stages.filter((x) => x === "snippets" || !MOUSE_STAGES.includes(x)))),
      row(t("Mouse hook"), hook(st.mouse, MOUSE), used(st.stages.filter((x) => MOUSE_STAGES.includes(x)))),
      h("div", {
        class: `${P}note`,
        text: st.elevated
          ? t("The window in front runs as administrator: remaps, snippets and the wheel wait until you leave it.")
          : st.suspended
            ? t("Paused while a shortcut is being recorded.")
            : t("Windows asks the app about every key and click only while a tool that needs it is on. With all of them off nothing is installed."),
      }),
    );
  };

  /** Saves without drawing again (a switch has already moved). */
  const set = async (patch: S) => {
    Object.assign(s, patch);
    await cx.settings.patch(patch);
    void refreshStatus();
    drawShortcuts();
  };
  /** Saves and draws the page again (a list changed). */
  const change = async (patch: S) => {
    await set(patch);
    draw();
  };

  // ── Small controls ──────────────────────────────────────────────────────────

  const sw = (key: string) => toggle(s[key] === true, (on) => void set({ [key]: on }));
  const num = (key: string, min: number, max: number) => {
    const f = field({ type: "number", value: String(s[key] ?? min) });
    f.classList.add(`${P}num`);
    f.input.setAttribute("min", String(min));
    f.input.setAttribute("max", String(max));
    f.input.addEventListener("change", () => {
      const v = Math.max(min, Math.min(max, Math.round(Number(f.input.value) || 0)));
      f.input.value = String(v);
      void set({ [key]: v });
    });
    return f;
  };
  const words = (key: string, placeholder: string) => {
    const f = field({ value: String(s[key] ?? ""), placeholder });
    f.input.addEventListener("change", () => void set({ [key]: f.input.value }));
    return f;
  };
  const pick = (key: string, options: string[]) => {
    const select = h("select", {}, ...options.map((name) => h("option", { value: name, text: name })));
    select.value = String(s[key] ?? options[0]);
    select.addEventListener("change", () => void set({ [key]: select.value }));
    return h("label", { class: `field ${P}num` }, select);
  };
  /** A combination that may be empty: the recorder and a button to clear it. */
  const keys = (value: string, empty: string, onChange: (chord: string) => void) =>
    h("span", { class: `${P}keys` }, recorder(cx, value, empty, onChange), value ? iconBtn("close", t("Clear"), () => onChange("")) : null);
  const block = (title: string, hint: string, body: Node) =>
    h("div", { class: `${P}block` }, h("b", { text: title }), h("span", { class: `${P}note`, text: hint }), body);

  /** Program names as chips, with the running apps to pick from and a field for any other. */
  const appList = (key: string) => {
    const list = strings(s[key]);
    const add = (name: string) => {
      const exe = name.trim().toLowerCase();
      if (exe && !list.includes(exe)) void change({ [key]: [...list, exe] });
    };
    const other = field({ placeholder: "app.exe", onEnter: add });
    other.classList.add(`${P}app`);
    const more = iconBtn("plus", t("Add an app that is open"), () => {
      const at = more.getBoundingClientRect();
      void cx.call<string[]>("apps").then((apps) => {
        const open = (apps ?? []).filter((a) => !list.includes(a));
        if (open.length > 0) menu(open.map((a) => ({ label: a, onClick: () => add(a) })), { x: at.left, y: at.bottom + 4 });
      });
    }, "secondary");
    return h(
      "div",
      { class: `${P}chips` },
      ...list.map((exe) => h("span", { class: `${P}chip` }, exe, iconBtn("close", t("Remove"), () => void change({ [key]: list.filter((x) => x !== exe) })))),
      other,
      more,
    );
  };

  // ── Snippets ────────────────────────────────────────────────────────────────

  const snipRow = (list: Snip[], x: Snip) =>
    h(
      "div",
      { class: `${P}item` },
      h("code", { text: x.trigger }),
      h("button", { class: `${P}grow`, title: t("Edit"), text: x.name || x.text.replace(/\s+/g, " "), onclick: () => { editing = { snip: { ...x }, fresh: false }; draw(); } }),
      x.folder ? h("small", { text: x.folder }) : null,
      toggle(x.on, (on) => void set({ snippetList: list.map((y) => (y.id === x.id ? { ...y, on } : y)) })),
      iconBtn("trash", t("Delete"), () => void change({ snippetList: list.filter((y) => y.id !== x.id) })),
    );

  const editor = (list: Snip[]) => {
    const { snip, fresh } = editing!;
    const trigger = field({ value: snip.trigger, placeholder: ";sig" });
    const name = field({ value: snip.name, placeholder: t("Name (optional)") });
    const text = field({ value: snip.text, multiline: true, placeholder: t("The text it expands to") });
    text.classList.add(`${P}wide`);
    const folder = field({ value: snip.folder, placeholder: t("Folder (optional)") });
    const mode = h("select", {}, h("option", { value: "delimiter", text: t("After a space, Tab or Enter") }), h("option", { value: "immediate", text: t("On its last character") }));
    mode.value = snip.mode;
    const ignoreCase = toggle(snip.ignoreCase, (on) => (snip.ignoreCase = on));
    const library = toggle(snip.library, (on) => (snip.library = on));
    const note = h("div", { class: `${P}note ${P}wide` });
    const say = (message: string, bad = false) => {
      note.textContent = message;
      note.classList.toggle(`${P}bad`, bad);
    };
    const variables = t("Variables: {{date}}, {{time}}, {{datetime}}, {{clipboard}}, or a pattern such as {{date:yyyy-MM-dd}} and {{time:HH:mm}}.");
    say(variables);
    // What the variables come to now. The clipboard's text is never shown here.
    text.input.addEventListener("input", () => {
      const value = text.input.value;
      if (!value.includes("{{")) return say(variables);
      void cx.call<string>("preview", { text: value.replaceAll("{{clipboard}}", t("(clipboard)")) }).then((now) => {
        if (now != null && text.input.value === value) say(t("Now: {text}", { text: now.replace(/\s+/g, " ").slice(0, 160) }));
      });
    });
    const save = () => {
      const next: Snip = {
        ...snip,
        trigger: trigger.input.value.replace(/\s+/g, "").slice(0, 40),
        name: name.input.value.trim(),
        text: text.input.value,
        folder: folder.input.value.trim(),
        mode: mode.value === "immediate" ? "immediate" : "delimiter",
      };
      const same = (a: Snip, b: Snip) => (a.ignoreCase || b.ignoreCase ? a.trigger.toLowerCase() === b.trigger.toLowerCase() : a.trigger === b.trigger);
      if ([...next.trigger].length < 2) return say(t("A trigger needs at least 2 characters."), true);
      if (list.some((y) => y.id !== next.id && same(y, next))) return say(t("Another snippet already has this trigger."), true);
      if (!next.text) return say(t("Write the text the trigger expands to."), true);
      editing = null;
      void change({ snippetList: fresh ? [...list, next] : list.map((y) => (y.id === next.id ? next : y)) });
    };
    return h(
      "div",
      { class: `${P}editor` },
      trigger,
      name,
      text,
      folder,
      h("label", { class: "field" }, mode),
      h("label", { class: `${P}check` }, ignoreCase, h("span", { text: t("Capitals do not matter") })),
      h("label", { class: `${P}check` }, library, h("span", { text: t("Show in the library") })),
      note,
      h("div", { class: `${P}actions ${P}wide` }, btn(t("Cancel"), "ghost", () => { editing = null; draw(); }), btn(fresh ? t("Add") : t("Save"), "primary", save)),
    );
  };

  // ── Remaps ──────────────────────────────────────────────────────────────────

  const remaps = (): Remap[] =>
    (Array.isArray(s.remapList) ? (s.remapList as Partial<Remap>[]) : []).map((r) => ({ from: String(r.from ?? ""), to: String(r.to ?? ""), on: r.on !== false }));
  /** New rules after the old ones; a rule on the same keys is replaced. */
  const withRules = (list: Remap[], rules: [string, string][]) => [
    ...list.filter((r) => !rules.some(([from]) => from === r.from)),
    ...rules.map(([from, to]) => ({ from, to, on: true })),
  ];
  const arrow = () => h("span", { class: `${P}arrow` }, icon("chevr", 14));
  const target = (to: string) => (to ? keysOf(to) : h("span", { class: `${P}note`, text: t("Does nothing") }));

  const remapRow = (list: Remap[], r: Remap) =>
    h(
      "div",
      { class: `${P}item` },
      keysOf(r.from),
      arrow(),
      target(r.to),
      h("span", { class: `${P}grow` }),
      toggle(r.on, (on) => void set({ remapList: list.map((y) => (y.from === r.from ? { ...y, on } : y)) })),
      iconBtn("trash", t("Delete"), () => void change({ remapList: list.filter((y) => y.from !== r.from) })),
    );

  const newRule = (list: Remap[]) => {
    const add = btn(t("Add"), "secondary", () => {
      const rules = withRules(list, [[rule.from, rule.to]]);
      rule = { from: "", to: "", on: true };
      void change({ remapList: rules });
    });
    add.disabled = !rule.from;
    return h(
      "div",
      { class: `${P}item` },
      recorder(cx, rule.from, t("Key or shortcut"), (chord) => { rule.from = chord; draw(); }),
      arrow(),
      keys(rule.to, t("Does nothing"), (chord) => { rule.to = chord; draw(); }),
      h("span", { class: `${P}grow` }),
      add,
    );
  };

  const presetRow = (list: Remap[], p: (typeof PRESETS)[number]) => {
    const there = p.rules.every(([from, to]) => list.some((r) => r.from === from && r.to === to));
    const add = btn(there ? t("Added") : t("Add"), "secondary", () => void change({ remapList: withRules(list, p.rules), remaps: true }));
    add.disabled = there;
    return row(t(p.name), add, t(p.hint));
  };

  // ── Shortcuts: every global shortcut of the app, and what stands in its way ──

  /** Asked once when the page is built; a change made on the Shortcuts page is read when this page is next opened. */
  const known = Promise.all([Bridge.featuresList(), Bridge.shortcutsStatus(), Bridge.boot()]);
  const drawShortcuts = () => void known.then(([features, report, boot]) => {
    const stored = boot?.settings.shortcuts ?? {};
    const all = [
      ...SHORTCUTS.filter((d) => d.ported).map((d) => ({ id: d.id as string, name: t(SHORTCUT_TEXT[d.id]), binding: effective(d, stored) })),
      ...(features ?? []).filter((f) => f.running).flatMap((f) =>
        f.shortcuts.map((sc) => ({
          id: `${f.id}:${sc.id}`,
          name: `${t(f.name)} — ${t(sc.label)}`,
          binding: stored[`${f.id}:${sc.id}`] ?? { keys: sc.default ?? "", enabled: true },
        })),
      ),
    ].filter((x) => x.binding.enabled && x.binding.keys);
    const remapped = new Set(s.remaps === true ? remaps().filter((r) => r.on).map((r) => r.from) : []);
    const blocked = new Set(s.guard === true ? strings(s.guardKeys) : []);
    const rows = all.map((x) => {
      // The app writes the Windows key "Super"; here it is "Win".
      const chord = x.binding.keys.replace("Super", "Win");
      const state = report?.actions.find((a) => a.id === x.id)?.status;
      const why = remapped.has(chord)
        ? t("Replaced by a key remap, so it never runs")
        : blocked.has(chord)
          ? t("Blocked here in the chosen apps")
          : state === "inUse"
            ? t("In use by another app")
            : state === "duplicate"
              ? t("Used twice")
              : state === "typesCharacter"
                ? t("Types a character on this keyboard")
                : state === "invalid"
                  ? t("Not a valid shortcut")
                  : "";
      return { why, node: h("div", { class: `${P}item` }, h("span", { class: `${P}grow`, text: x.name }), why ? h("span", { class: `${P}tag`, text: why }) : null, keysOf(chord)) };
    });
    const conflicts = rows.filter((r) => r.why).length;
    shortcuts.replaceChildren(
      row(
        conflicts === 0 ? t("No conflicts") : tn("{count} shortcut needs a look", "{count} shortcuts need a look", conflicts),
        btn(t("Change shortcuts"), "secondary", () => (location.hash = "#/shortcuts")),
        t("Every shortcut of the app that works from anywhere, with what stands in its way."),
      ),
      // The ones that need a look first.
      ...rows.sort((a, b) => Number(!!b.why) - Number(!!a.why)).map((r) => r.node),
    );
  });

  // ── The page ────────────────────────────────────────────────────────────────

  const draw = () => {
    const list = snips(s);
    const rules = remaps();
    const guardKeys = strings(s.guardKeys);
    el.replaceChildren(
      h("div", { class: "section-title", text: t("What is installed") }),
      status,

      h("div", { class: "section-title", text: t("Snippets") }),
      card([
        row(t("Expand snippets as I type"), sw("snippets"), t("Type a trigger, then a space, Tab or Enter. What you type is never stored, and nothing is expanded in a password field.")),
        row(t("Snippet library"), btn(t("Open"), "secondary", () => void cx.call("library")), t("A small window to search your snippets and type one into the app you were in. Its shortcut is on the Shortcuts page.")),
      ]),
      card([
        ...list.map((x) => snipRow(list, x)),
        list.length === 0 && !editing ? h("div", { class: `${P}note`, text: t("No snippets yet. A trigger such as ;sig can type your signature.") }) : null,
        editing ? editor(list) : h("div", { class: `${P}actions` }, btn(t("Add a snippet"), "secondary", () => {
          editing = { snip: { id: crypto.randomUUID(), name: "", trigger: "", text: "", mode: "delimiter", on: true, ignoreCase: false, folder: "", library: true }, fresh: true };
          draw();
        })),
      ]),

      h("div", { class: "section-title", text: t("Keyboard") }),
      card([
        row(t("Ignore doubled key presses"), sw("keyDebounce"), t("For a worn keyboard: a key that comes again within the window of its own release is dropped. Holding a key still repeats.")),
        row(t("Window, in milliseconds"), num("keyDebounceMs", 0, 500), t("0 to 500. Start at 5 and raise it until the doubles stop.")),
        row(t("Other windows for single keys"), words("keyDebounceKeys", "E:40, Space:25"), t("A key and its own window, separated by commas.")),
      ]),

      h("div", { class: "section-title", text: t("Remaps") }),
      card([
        row(t("Remap keys and shortcuts"), sw("remaps"), t("A key or a shortcut stands for another while this is on. Nothing is remapped until you add a rule.")),
        ...rules.map((r) => remapRow(rules, r)),
        newRule(rules),
      ]),
      card(PRESETS.map((p) => presetRow(rules, p))),

      h("div", { class: "section-title", text: t("Blocked shortcuts") }),
      card([
        row(t("Block shortcuts in chosen apps"), sw("guard"), t("The shortcuts below do nothing while one of these apps is in front. Everywhere else they work as usual.")),
        block(t("Apps"), t("Program names, such as game.exe."), appList("guardApps")),
        block(
          t("Shortcuts"),
          t("A key on its own can be blocked too, such as the Windows key."),
          h(
            "div",
            { class: `${P}chips` },
            ...guardKeys.map((k) => h("span", { class: `${P}chip` }, keysOf(k), iconBtn("close", t("Remove"), () => void change({ guardKeys: guardKeys.filter((x) => x !== k) })))),
            recorder(cx, "", t("Add a shortcut"), (chord) => void change({ guardKeys: guardKeys.includes(chord) ? guardKeys : [...guardKeys, chord] })),
          ),
        ),
      ]),

      h("div", { class: "section-title", text: t("Mouse") }),
      card([
        row(t("Ignore doubled clicks"), sw("clickDebounce"), t("For a worn mouse button: a click that comes again within the window of its own release is dropped.")),
        row(t("Window, in milliseconds"), num("clickDebounceMs", 5, 100), t("5 to 100. A real double click is slower than that.")),
        row(t("Turn pointer acceleration off"), sw("pointerAccelOff"), t("Windows calls it Enhance pointer precision. Your own setting is put back when this is turned off and when the app quits.")),
      ]),

      h("div", { class: "section-title", text: t("Scrolling") }),
      card([
        row(t("Reverse the wheel"), sw("invertVertical"), t("The mouse wheel only; the touchpad has its own switch below.")),
        row(t("Reverse sideways scrolling"), sw("invertHorizontal")),
        row(t("Reverse the touchpad"), sw("padVertical"), t("Up and down on the touchpad, apart from the mouse wheel.")),
        row(t("Reverse the touchpad sideways"), sw("padHorizontal")),
        row(t("Scroll sideways while a key is held"), sw("sideways"), t("For apps that do not do it themselves.")),
        row(t("Key to hold"), pick("sidewaysKey", ["Shift", "Alt", "Ctrl"])),
        block(t("Apps to leave alone"), t("The wheel is not changed while the pointer is over one of them."), appList("scrollExceptions")),
      ]),

      h("div", { class: "section-title", text: t("Mouse buttons") }),
      card([
        row(t("Shortcuts on the side buttons"), sw("buttons"), t("A button presses the keys you choose and no longer reaches the app. One left empty keeps what it does.")),
        ...([["buttonBack", N_("Back button")], ["buttonForward", N_("Forward button")], ["wheelLeft", N_("Side wheel to the left")], ["wheelRight", N_("Side wheel to the right")]] as const).map(([key, name]) =>
          row(t(name), keys(String(s[key] ?? ""), t("Not set"), (chord) => void change({ [key]: chord }))),
        ),
        block(t("Apps to leave alone"), t("The buttons keep what they do while one of them is under the pointer or in front."), appList("buttonExceptions")),
      ]),

      h("div", { class: "section-title", text: t("Shortcuts") }),
      shortcuts,
    );
  };

  void cx.settings.get().then((value) => {
    s = { ...value };
    draw();
    void refreshStatus();
    drawShortcuts();
  });
  return el;
}

// ── The library window ────────────────────────────────────────────────────────

function library(root: HTMLElement, cx: FeatureCtx) {
  const t = cx.t;
  let all: Snip[] = [];
  let shown: Snip[] = [];
  let cursor = 0;
  const rows = h("div", { class: `${P}rows list`, role: "listbox" });
  const close = () => void cx.call("library-close");
  /** Rust closes the window, goes back to the app the user was in and types there. */
  const insert = (x: Snip | undefined) => x && void cx.call("insert", { id: x.id });

  const draw = () => {
    // By folder, in the order of a file list; the snippets without one come last.
    const order = new Intl.Collator(undefined, { numeric: true, sensitivity: "base" });
    shown = found(all, search.input.value).sort((a, b) => Number(!a.folder) - Number(!b.folder) || order.compare(a.folder, b.folder));
    cursor = Math.max(0, Math.min(cursor, shown.length - 1));
    rows.replaceChildren();
    shown.forEach((x, i) => {
      if (x.folder && x.folder !== shown[i - 1]?.folder) rows.append(h("div", { class: "section-title", text: x.folder }));
      rows.append(
        h(
          "button",
          { class: `list-item${i === cursor ? " on" : ""}`, role: "option", "aria-selected": String(i === cursor), onclick: () => insert(x) },
          h("span", { text: x.name || x.text.replace(/\s+/g, " ") }),
          h("small", { text: x.trigger }),
          i < 9 ? kbd(String(i + 1)) : null,
        ),
      );
    });
    if (shown.length === 0) {
      rows.append(h("div", { class: `${P}note`, text: all.length === 0 ? t("No snippets yet. Add some in Settings, under Input tools.") : t("Nothing matches.") }));
    }
    rows.querySelector(".on")?.scrollIntoView({ block: "nearest" });
  };
  const search = field({ icon: "search", placeholder: t("Search snippets"), onInput: () => { cursor = 0; draw(); } });

  window.addEventListener("keydown", (e) => {
    if (e.key === "ArrowDown" || e.key === "ArrowUp") cursor += e.key === "ArrowDown" ? 1 : -1;
    else if (e.key === "Enter") return insert(shown[cursor]);
    else if (e.key === "Escape") return close();
    else if (e.ctrlKey && /^[1-9]$/.test(e.key)) return insert(shown[Number(e.key) - 1]);
    else return;
    e.preventDefault();
    draw();
  });
  // A click anywhere else: the library goes away.
  window.addEventListener("blur", close);

  root.append(
    h(
      "div",
      { class: `win ${P}library` },
      h("div", { class: "win-bar" }, icon("text", 14), h("span", { text: t("Snippets") }), h("button", { class: "close", title: t("Close"), onclick: close, text: "×" })),
      search,
      rows,
      h("div", { class: `${P}foot` }, h("span", {}, kbd("↑"), kbd("↓"), t("Move")), h("span", {}, kbd("Enter"), t("Insert")), h("span", {}, kbd("Ctrl"), kbd("1"), t("Pick by number")), h("span", {}, kbd("Esc"), t("Close"))),
    ),
  );
  void cx.settings.get().then((s) => {
    all = snips(s).filter((x) => x.library);
    draw();
    search.input.focus();
  });
}

// ── The feature ───────────────────────────────────────────────────────────────

/**
 * A mouse wheel over a strip that only scrolls sideways moves it sideways. A
 * list around it keeps the wheel, and so does a touchpad, which scrolls both
 * ways by itself (its steps are smaller than a wheel's notch).
 */
function wheelOverStrips(e: WheelEvent) {
  if (e.defaultPrevented || e.ctrlKey || e.shiftKey || e.altKey || e.deltaX !== 0 || Math.abs(e.deltaY) < 100) return;
  for (let el: Element | null = e.target instanceof Element ? e.target : null; el && el !== document.body; el = el.parentElement) {
    const style = getComputedStyle(el);
    if (/auto|scroll/.test(style.overflowY) && el.scrollHeight > el.clientHeight + 1) return;
    if (/auto|scroll/.test(style.overflowX) && el.scrollWidth > el.clientWidth + 1) {
      el.scrollLeft += e.deltaY;
      e.preventDefault();
      return;
    }
  }
}

export default defineFeature({
  id: "input-tools",
  settings: settingsBody,
  windows: { library },
  // Rows for the command bar: the snippets that match, and the library.
  commands: async (query, cx) => [
    ...(query.trim() ? found(snips(await cx.settings.get()), query) : []).slice(0, 8).map((x) => ({
      id: `input-tools:${x.id}`,
      title: x.name || x.trigger,
      subtitle: cx.t("Snippet"),
      icon: "text",
      keywords: [x.trigger],
      run: async () => void (await cx.call("insert", { id: x.id })),
    })),
    { id: "input-tools:library", title: cx.t("Open the snippet library"), icon: "text", keywords: ["snippets", "text"], run: async () => void (await cx.call("library")) },
  ],
  start: (cx) => {
    window.addEventListener("wheel", wheelOverStrips, { passive: false });
    const off = cx.on<{ elevated: boolean }>("insert-failed", (r) =>
      cx.island.notice({
        kind: "notification",
        // One row of the strip: the reason in two words.
        title: cx.t("Snippet not inserted"),
        detail: r.elevated ? cx.t("Administrator window") : cx.t("Try again"),
        icon: "alert",
        tone: "warn",
      }),
    );
    return () => {
      off();
      window.removeEventListener("wheel", wheelOverStrips);
    };
  },
});
