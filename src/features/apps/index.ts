// Apps — the page side of the feature (docs/ARCHITECTURE.md §2.3).
// Rust asks winget and runs it; this file draws the Apps section of the island
// (the updates), the Apps window (updates, packages, release notes, feedback)
// and the settings. Nothing is installed without a click on a card that shows
// the command first, and feedback leaves only through the browser.

import "./style.css";
import { defineFeature, type FeatureCtx, type SectionHost, type Value } from "../../core/feature";
import { Bridge } from "../../core/bridge";
import { BRAND } from "../../core/brand";
import { h } from "../../views/dom";
import { language, tn } from "../../i18n/i18n";
import { icon } from "../../ui/icons";
import { btn, card, field, iconBtn, menu, meter, row, setMeter, tab, toggle } from "../../ui/components";
import changelog from "./changelog.json";

const P = "f-apps-";
// A box, and three dots: the shared set has neither.
const BOX = "M12 3l8 4.5v9L12 21l-8-4.5v-9zM4 7.5l8 4.5 8-4.5M12 12v9";
const MORE = "M6 12h.01M12 12h.01M18 12h.01";

interface Update { id: string; name: string; version: string; available: string; source: string; cmd: string }
interface Found { id: string; name: string; version: string; cmd: string }
interface Progress { phase: keyof typeof PHASE; fraction: number | null; line: string }
interface Op extends Progress { action: "install" | "upgrade"; id: string; name: string }
interface Ended { action: "install" | "upgrade"; name: string; ok: boolean; message: string }
interface Live { winget: boolean; updates: Update[]; checkedAt: number | null; scanning: boolean; note: string; op: Op | null; waiting: number; ended: Ended | null; tab: string; version: string }
interface Rule { id: string; name: string; version?: string }
interface About { version: string; windows: string; language: string; issues: string }
interface Release { version: string; added?: string[]; changed?: string[]; fixed?: string[] }

const PHASE = { preparing: "Preparing", downloading: "Downloading", verifying: "Checking the download", installing: "Installing", done: "Finishing" } as const;
const LOG = { added: "Added", changed: "Changed", fixed: "Fixed" } as const;

// What Rust last said, and who draws it in this window.
let live: Live | null = null;
let watching = false;
const views = new Map<string, (progressOnly: boolean) => void>();
const redraw = (progressOnly = false) => views.forEach((draw) => draw(progressOnly));

// A newer Lintel the island announced: the Apps section shows it with Update now, What's new and Later.
interface SelfCheck { latest?: string; newer?: boolean; page?: string; notes?: string; error?: string; canInstall?: boolean; announce?: boolean; off?: boolean }
let offer: SelfCheck | null = null;
let offerError = "";
let drawOffer = () => {};

/** Update now: Rust downloads, checks and runs the installer, then quits; only an error comes back. */
function updateNow(cx: FeatureCtx, r: SelfCheck) {
  offerError = "";
  if (r.canInstall) void cx.call("selfUpdate");
  else void Bridge.openUrl(r.page ?? "");
  drawOffer();
}

/** Follows the feature's state from now on; the returned function stops it. */
function watch(cx: FeatureCtx): () => void {
  if (watching) return () => {};
  watching = true;
  const offs = [
    cx.on<Live>("state", (s) => {
      live = s;
      redraw();
    }),
    cx.on<Progress>("progress", (p) => {
      if (!live?.op) return;
      Object.assign(live.op, p);
      redraw(true);
    }),
  ];
  void cx.call<Live>("state").then((s) => {
    if (!s || live) return;
    live = s;
    redraw();
  });
  return () => {
    offs.forEach((off) => off());
    watching = false;
    live = null;
  };
}

const small = (b: HTMLButtonElement) => (b.classList.add("sm"), b);

/** "5 minutes ago", in the app's language. */
function ago(secs: number): string {
  const d = Math.max(0, Math.round(Date.now() / 1000 - secs));
  const f = new Intl.RelativeTimeFormat(language(), { numeric: "auto" });
  if (d < 60) return f.format(0, "second");
  if (d < 3600) return f.format(-Math.round(d / 60), "minute");
  return d < 86400 ? f.format(-Math.round(d / 3600), "hour") : f.format(-Math.round(d / 86400), "day");
}

/** A `.field` around a native select. */
function select(opts: [string, string][], value: string, on: (v: string) => void): HTMLElement {
  const sel = h("select");
  for (const [v, label] of opts) sel.append(h("option", { value: v, text: label }));
  sel.value = value;
  sel.addEventListener("change", () => on(sel.value));
  return h("label", { class: "field" }, sel);
}

// ── The lane: what is about to run, what runs, how it ended ──────────────────

interface Lane {
  el: HTMLElement;
  /** Shows the commands; they run only on the card's own button. */
  ask(cmds: string[], run: () => void): void;
  draw(progressOnly: boolean): void;
}

function lane(cx: FeatureCtx): Lane {
  const el = h("div", { class: `${P}lane` });
  const bar = meter(0);
  const pct = h("span", { class: `${P}pct` });
  const line = h("small");
  let asking: HTMLElement | null = null;
  let dismissed = "";
  let running = false;

  const paint = (op: Op) => {
    // No number from winget: the bar is full and breathes instead of standing empty.
    setMeter(bar, op.fraction ?? 1);
    bar.classList.toggle(`${P}busy`, op.fraction == null);
    pct.textContent = op.fraction == null ? "" : `${Math.round(op.fraction * 100)}%`;
    line.textContent = op.line ? `${cx.t(PHASE[op.phase])} · ${op.line}` : cx.t(PHASE[op.phase]);
  };

  const runningCard = (op: Op, waiting: number) =>
    card(
      h("div", { class: `${P}op` },
        h("span", { class: "isl-ico info" }, icon("download", 14)),
        h("div", { class: `${P}op-text` },
          h("b", { text: cx.t(op.action === "install" ? "Installing {name}" : "Updating {name}", { name: op.name }) + (waiting ? ` · ${tn("{n} more waiting", "{n} more waiting", waiting, { n: waiting })}` : "") }),
          line),
        bar, pct,
        small(btn(cx.t("Cancel"), "ghost", () => void cx.call("cancel")))),
    );

  const endedCard = (e: Ended) =>
    card(
      h("div", { class: `${P}op` },
        h("span", { class: `isl-ico ${e.ok ? "ok" : "bad"}` }, icon(e.ok ? "check" : "alert", 14)),
        h("div", { class: `${P}op-text` },
          h("b", { text: cx.t(!e.ok ? "{name} did not finish" : e.action === "install" ? "{name} is installed" : "{name} is up to date", { name: e.name }) }),
          e.message ? h("small", { class: `${P}wrap`, text: e.message }) : null),
        iconBtn("close", cx.t("Dismiss"), () => {
          dismissed = JSON.stringify(e);
          draw(false);
        })),
      e.ok ? "ok" : "bad",
    );

  function draw(progressOnly: boolean) {
    const op = live?.op ?? null;
    if (op) paint(op);
    if (progressOnly && running) return;
    running = !!op;
    const ended = !op && live?.ended && JSON.stringify(live.ended) !== dismissed ? live.ended : null;
    el.replaceChildren(...[asking, op ? runningCard(op, live!.waiting) : null, ended ? endedCard(ended) : null].filter((n): n is HTMLElement => !!n));
  }

  return {
    el,
    draw,
    ask(cmds, run) {
      const close = () => {
        asking = null;
        draw(false);
      };
      asking = card(
        [
          h("b", { class: `${P}ask`, text: tn("This will run one command", "This will run {n} commands", cmds.length, { n: cmds.length }) }),
          h("pre", { class: `${P}cmd`, text: cmds.join("\n") }),
          h("p", { class: `${P}hint`, text: cx.t("winget downloads and installs without asking again, and accepts the licence terms of each package. If an installer needs administrator rights, Windows asks you. Cancelling an install half-way can leave the app broken.") }),
          h("div", { class: `${P}actions` },
            btn(cx.t("Cancel"), "ghost", close),
            btn(cx.t("Run"), "primary", () => {
              close();
              run();
            })),
        ],
        true,
      );
      asking.classList.add(`${P}asking`);
      draw(false);
    },
  };
}

// ── Updates ──────────────────────────────────────────────────────────────────

function updatesView(cx: FeatureCtx, the: Lane, compact: boolean) {
  const el = h("div", { class: `${P}view${compact ? ` ${P}compact` : ""}` });

  /** One rule per app: a new one replaces the app's old one. */
  const hide = async (rule: Rule) => {
    const rules = (((await cx.settings.get()).rules as Rule[] | undefined) ?? []).filter((r) => r.id.toLowerCase() !== rule.id.toLowerCase());
    await cx.settings.patch({ rules: [...rules, rule] });
  };

  const rowOf = (u: Update) => {
    const more: HTMLButtonElement = small(
      iconBtn(MORE, cx.t("More"), () => {
        const r = more.getBoundingClientRect();
        menu(
          [
            { label: cx.t("Skip version {version}", { version: u.available }), icon: "next", onClick: () => void hide({ id: u.id, name: u.name, version: u.available }) },
            { label: cx.t("Never check this app"), icon: "close", onClick: () => void hide({ id: u.id, name: u.name }) },
          ],
          { x: r.right - 190, y: r.bottom + 4 },
        );
      }),
    );
    const update = small(btn(cx.t("Update"), "secondary", () => the.ask([u.cmd], () => void cx.call("update", { ids: [u.id] }))));
    update.disabled = live?.op?.id === u.id;
    return h("div", { class: `${P}row` },
      h("div", { class: `${P}name` }, h("b", { text: u.name }), h("small", { text: u.source === "msstore" ? `${cx.t("Microsoft Store")} · ${u.id}` : u.id })),
      h("span", { class: `${P}ver` }, `${u.version} → `, h("b", { text: u.available })),
      update,
      more);
  };

  function draw() {
    const s = live;
    if (!s) return void el.replaceChildren();
    const n = s.updates.length;
    const title = s.scanning ? cx.t("Asking winget…")
      : s.checkedAt == null ? cx.t("Not checked yet")
      : n ? tn("{n} update", "{n} updates", n, { n })
      : cx.t("Everything is up to date");
    const detail = s.checkedAt != null ? cx.t("Checked {when}", { when: ago(s.checkedAt) }) : cx.t("A check asks winget what it can update.");
    const check = btn(cx.t("Check now"), "secondary", () => void cx.call("check"));
    check.disabled = s.scanning || !s.winget;
    const all = n > 1
      ? btn(cx.t("Update all"), "primary", () => the.ask(s.updates.map((u) => u.cmd), () => void cx.call("update", { ids: s.updates.map((u) => u.id) })))
      : null;
    const note = !s.winget ? cx.t("winget is not on this PC. It comes with App Installer, from the Microsoft Store.") : s.note;
    const top = el.lastElementChild?.scrollTop ?? 0;
    el.replaceChildren(
      h("div", { class: `${P}head` },
        h("div", { class: `${P}status` }, h("b", { text: title }), h("small", { text: detail })),
        check, all,
        compact ? iconBtn("external", cx.t("Open in a window"), () => void cx.call("open", { tab: "updates" })) : null),
      note ? h("p", { class: `${P}note`, text: note }) : "",
      h("div", { class: `${P}list` }, ...s.updates.map(rowOf)),
    );
    el.lastElementChild!.scrollTop = top;
  }

  return { el, draw };
}

// ── Packages ─────────────────────────────────────────────────────────────────

function packagesView(cx: FeatureCtx, the: Lane) {
  const list = h("div", { class: `${P}list` });
  const say = (text: string, bad = false) => list.replaceChildren(h("p", { class: `${P}empty${bad ? ` ${P}bad` : ""}`, text }));
  let timer = 0;
  let asked: string | null = null;

  async function search(raw: string) {
    const q = raw.trim();
    if (q === asked) return;
    asked = q;
    if (q.length < 2) return say(cx.t("Type a name to search the winget community repository."));
    say(cx.t("Searching…"));
    const r = await cx.call<{ results?: Found[]; error?: string; stale?: boolean }>("search", { q });
    if (q !== asked || r?.stale) return;
    if (!r || r.error) return say(r?.error || cx.t("winget could not search."), true);
    if (!r.results?.length) return say(cx.t("Nothing found for “{query}”.", { query: q }));
    list.replaceChildren(
      ...r.results.map((f) =>
        h("div", { class: `${P}row` },
          h("div", { class: `${P}name` }, h("b", { text: f.name }), h("small", { text: f.id })),
          h("span", { class: `${P}ver`, text: f.version }),
          small(btn(cx.t("Install"), "secondary", () => the.ask([f.cmd], () => void cx.call("install", { id: f.id })))))),
    );
  }

  const box = field({
    icon: "search",
    placeholder: cx.t("Search packages"),
    onInput: (v) => {
      clearTimeout(timer);
      timer = window.setTimeout(() => void search(v), 400);
    },
    onEnter: (v) => {
      clearTimeout(timer);
      void search(v);
    },
  });
  void search("");
  return { el: h("div", { class: `${P}view` }, box, list), focus: () => box.input.focus() };
}

// ── Release notes and the app's own update ───────────────────────────────────

function notesView(cx: FeatureCtx) {
  const result = h("div", { class: `${P}result` });
  const head = h("div");
  const check: HTMLButtonElement = btn(cx.t("Check for updates"), "secondary", async () => {
    check.disabled = true;
    result.replaceChildren(cx.t("Checking…"));
    const r = await cx.call<SelfCheck>("selfCheck");
    check.disabled = false;
    if (!r || r.error) return result.replaceChildren(cx.t("Could not check: {error}", { error: r?.error ?? "" }));
    if (!r.newer) return result.replaceChildren(cx.t("You have the latest version."));
    result.replaceChildren(
      h("span", { text: cx.t("Version {version} is out.", { version: r.latest ?? "" }) }),
      small(btn(cx.t(r.canInstall ? "Update now" : "Open the release page"), "primary", () => {
        updateNow(cx, r);
        if (r.canInstall) result.append(h("p", { class: `${P}hint`, text: cx.t("Downloading. Lintel closes when the installer starts.") }));
      })),
      r.notes ? h("pre", { class: `${P}cmd`, text: r.notes }) : "",
    );
  });
  const draw = () =>
    head.replaceChildren(
      card([
        row(`${BRAND.name} ${live?.version ?? ""}`, check, cx.t("Looks for a newer release. Update now downloads the installer from GitHub, checks it against GitHub's checksum and runs it after your click.")),
        result,
      ]),
    );
  const log = (changelog as Release[]).flatMap((rel) => [
    h("div", { class: "section-title", text: cx.t("Version {version}", { version: rel.version }) }),
    card(
      (["added", "changed", "fixed"] as const)
        .filter((k) => rel[k]?.length)
        .map((k) => h("div", { class: `${P}log` }, h("h4", { text: cx.t(LOG[k]) }), h("ul", {}, ...rel[k]!.map((text) => h("li", { text: cx.t(text) }))))),
    ),
  ]);
  return { el: h("div", { class: `${P}scroll` }, head, ...log), draw };
}

// ── Feedback ─────────────────────────────────────────────────────────────────

function feedbackView(cx: FeatureCtx) {
  let kind: "bug" | "idea" = "bug";
  let details = false;
  let about: About | null = null;

  const text = field({ multiline: true, placeholder: cx.t("What happened, or what would you like?"), onInput: () => draw() });
  const count = h("small", { class: `${P}count` });
  const preview = h("pre", { class: `${P}cmd` });
  const hint = h("p", { class: `${P}hint` });
  const kinds = (["bug", "idea"] as const).map((k) =>
    tab(k === "bug" ? "alert" : "sparkle", cx.t(k === "bug" ? "A problem" : "An idea"), () => {
      kind = k;
      draw();
    }));

  // What goes to the issue page, and nothing else. Its few fixed words stay in English: the project reads them.
  const message = () => text.input.value.trim();
  // Cut by characters: half of an emoji cannot be put in a link.
  const title = () => `[${kind}] ${[...message().split("\n")[0]].slice(0, 80).join("")}`;
  const body = () => (details && about ? `${message()}\n\n---\n${BRAND.name} ${about.version} · ${about.windows} · ${about.language}` : message());
  const url = () => `${about?.issues ?? ""}?title=${encodeURIComponent(title())}&body=${encodeURIComponent(body())}`;

  const open = btn(cx.t("Open the issue page"), "primary", () => void Bridge.openUrl(url()));

  function draw() {
    const n = message().length;
    // A link carries about 8 KB; text outside Latin takes nine characters a letter once encoded.
    const long = url().length > 7500;
    kinds.forEach((b, i) => b.classList.toggle("on", (i === 0) === (kind === "bug")));
    count.textContent = `${n} / 2000`;
    open.disabled = !about || n < 10 || n > 2000 || long;
    preview.textContent = n ? `${title()}\n\n${body()}` : "";
    preview.hidden = !n;
    hint.textContent = long || n > 2000 ? cx.t("That is too long for a link. Shorten the message.")
      : n < 10 ? cx.t("Write at least ten characters.")
      : cx.t("This is the text the issue page will be filled with.");
  }

  void cx.call<About>("about").then((a) => {
    about = a;
    draw();
  });
  draw();
  return {
    el: h("div", { class: `${P}scroll` },
      h("div", { class: `${P}kinds` }, ...kinds, count),
      text,
      card(row(cx.t("Include technical details"), toggle(false, (on) => {
        details = on;
        draw();
      }), cx.t("The app's version, your Windows version and the app's language. Nothing else."))),
      hint,
      preview,
      h("p", { class: `${P}hint`, text: cx.t("Nothing is sent from here. The button opens the project's issue page in your browser with this text filled in, and you post it there.") }),
      h("div", { class: `${P}actions` }, open)),
  };
}

// ── The feature ──────────────────────────────────────────────────────────────

function section(cx: FeatureCtx): SectionHost {
  const the = lane(cx);
  const updates = updatesView(cx, the, true);
  const draw = (progressOnly = false) => {
    the.draw(progressOnly);
    if (!progressOnly) updates.draw();
  };
  views.set("section", draw);
  const news = h("div", { class: `${P}news` });
  drawOffer = () => {
    const o = offer;
    news.hidden = !o;
    if (!o) return;
    news.replaceChildren(card([
      row(cx.t("Version {version} is out.", { version: o.latest ?? "" }), h("span", {},
        small(btn(cx.t("Update now"), "primary", () => updateNow(cx, o))),
        small(btn(cx.t("Later"), "secondary", () => { offer = null; drawOffer(); })))),
      o.notes ? h("pre", { class: `${P}cmd`, text: o.notes }) : "",
      offerError ? h("p", { class: `${P}hint`, text: offerError }) : "",
    ]));
  };
  drawOffer();
  return { el: h("div", { class: `${P}page` }, news, the.el, updates.el), show: () => { draw(); drawOffer(); }, hide() {}, sync() {} };
}

export default defineFeature({
  id: "apps",

  sections: [{ id: "apps", title: "Apps", icon: BOX, letter: "a", height: 300, build: section }],

  settings(cx) {
    const box = h("div", { class: `${P}settings` });
    const open = (to: string) => () => void cx.call("open", { tab: to });
    const draw = (s: Value) => {
      const rules = (s.rules as Rule[] | undefined) ?? [];
      const set = (key: string) => (on: boolean) => void cx.settings.patch({ [key]: on });
      const show = (rule: Rule) => async () => {
        await cx.settings.patch({ rules: rules.filter((r) => r !== rule) });
        draw(await cx.settings.get());
      };
      box.replaceChildren(
        card([
          row(cx.t("Check for app updates"),
            select([["off", cx.t("Only when I ask")], ["daily", cx.t("Every day")], ["weekly", cx.t("Every week")]], String(s.frequency ?? "off"), (v) => void cx.settings.patch({ frequency: v })),
            cx.t("A check asks winget, which reads its sources over the network.")),
          row(cx.t("Tell me what a check found"), toggle(s.notify !== false, set("notify")), cx.t("Only for the checks that run by themselves, and once per update.")),
          row(cx.t("Include Microsoft Store apps"), toggle(s.store !== false, set("store"))),
          row(cx.t("Get new versions from the island"), toggle(s.selfCheck === true, (on) => void cx.settings.patch({ selfCheck: on, selfAsked: true })),
            cx.t("Asks GitHub for the list of releases a few times a day. A new version only opens the island once a day, and nothing is installed without your click.")),
        ]),
        h("div", { class: "section-title", text: cx.t("Hidden updates") }),
        card(
          rules.length
            ? rules.map((r) => row(r.name || r.id, small(btn(cx.t("Show again"), "secondary", show(r))), r.version ? cx.t("Version {version} is skipped", { version: r.version }) : cx.t("Never checked")))
            : h("p", { class: `${P}hint`, text: cx.t("Nothing is hidden. An update's own menu can skip a version or stop checking the app.") }),
        ),
        h("div", { class: "section-title", text: BRAND.name }),
        h("div", { class: `${P}actions ${P}left` },
          btn(cx.t("Updates and packages"), "secondary", open("updates")),
          btn(cx.t("Release notes"), "secondary", open("notes")),
          btn(cx.t("Send feedback"), "secondary", open("feedback"))),
      );
    };
    void cx.settings.get().then(draw);
    return box;
  },

  windows: {
    main(root, cx) {
      const the = lane(cx);
      const updates = updatesView(cx, the, false);
      const packages = packagesView(cx, the);
      const notes = notesView(cx);
      const pages: Record<string, HTMLElement> = { updates: updates.el, packages: packages.el, notes: notes.el, feedback: feedbackView(cx).el };
      const body = h("div", { class: `${P}body` });
      const tabs: [string, HTMLButtonElement][] = (
        [["updates", "download", "Updates"], ["packages", BOX, "Packages"], ["notes", "text", "Release notes"], ["feedback", "chat", "Feedback"]] as const
      ).map(([id, ico, title]): [string, HTMLButtonElement] => [id, tab(ico, cx.t(title), () => go(id))]);
      function go(id: string) {
        if (!(id in pages)) return;
        tabs.forEach(([k, b]) => b.classList.toggle("on", k === id));
        body.replaceChildren(pages[id]);
        // What winget is doing belongs to the two pages that start it.
        the.el.hidden = id === "notes" || id === "feedback";
        if (id === "packages") packages.focus();
      }
      views.set("window", (progressOnly) => {
        the.draw(progressOnly);
        if (progressOnly) return;
        updates.draw();
        notes.draw();
      });
      root.append(
        h("div", { class: `win ${P}win` },
          h("div", { class: "win-bar" }, icon(BOX, 14), h("span", { text: cx.t("Apps") }),
            h("button", { class: "close", title: cx.t("Close"), onclick: () => void cx.call("close"), text: "×" })),
          h("div", { class: `${P}tabs` }, ...tabs.map(([, b]) => b)),
          the.el,
          body),
      );
      go("updates");
      redraw();
      watch(cx);
      cx.on<string>("tab", go);
      void cx.call<Live>("state").then((s) => s && go(s.tab));
    },
  },

  start(cx) {
    const off = watch(cx);
    const offFound = cx.on<{ count: number; names: string[] }>("found", (e) =>
      cx.island.notice({
        kind: "notification",
        title: tn("{n} app update", "{n} app updates", e.count, { n: e.count }),
        detail: e.names.join(", "),
        icon: "download",
        tone: "info",
        section: "apps",
      }));
    // A newer Lintel: asked once, then checked a few times a day; announced at most once a day.
    const check = async () => {
      const s = await cx.settings.get();
      if (s.selfCheck !== true) return;
      const r = await cx.call<SelfCheck>("selfCheck", { auto: true });
      if (!r?.announce) return;
      offer = r;
      offerError = "";
      cx.island.notice({
        kind: "notification",
        title: cx.t("Version {version} is out.", { version: r.latest ?? "" }),
        detail: cx.t("Update now, or see what is new."),
        icon: "download",
        tone: "info",
        section: "apps",
        action: { label: cx.t("Update now"), run: () => updateNow(cx, r) },
      });
    };
    const ask = async () => {
      const s = await cx.settings.get();
      if (s.selfAsked === true || s.selfCheck != null) return;
      await cx.settings.patch({ selfAsked: true });
      cx.island.notice({
        kind: "notification",
        title: cx.t("Get new versions from the island?"),
        detail: cx.t("Lintel would ask GitHub for its releases a few times a day."),
        icon: "download",
        tone: "info",
        action: { label: cx.t("Yes"), run: () => void cx.settings.patch({ selfCheck: true }).then(check) },
      });
    };
    const offSelf = cx.on<{ error?: string }>("selfUpdate", (e) => {
      offerError = e.error ?? "";
      drawOffer();
    });
    const first = window.setTimeout(() => void ask().then(check), 45_000);
    const every = window.setInterval(() => void check(), 6 * 3600_000);
    return () => {
      off();
      offFound();
      offSelf();
      clearTimeout(first);
      clearInterval(every);
    };
  },
});
