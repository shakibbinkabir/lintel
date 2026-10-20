// Cleaner — the page side of the feature (docs/ARCHITECTURE.md §2.3).
// Rust scans, moves to the Recycle Bin and runs uninstallers; this file draws
// the Cleaner section of the island (the scan and its review list), the
// Cleaner window (the same, the uninstaller, the startup list) and the
// settings. A row is named to Rust by the number the scan gave it, never by a
// path, and nothing moves without a tick and a click on a card that says what
// will move.

import "./style.css";
import { defineFeature, type FeatureCtx, type SectionHost, type Value } from "../../core/feature";
import { h } from "../../views/dom";
import { language, N_, tn } from "../../i18n/i18n";
import { icon } from "../../ui/icons";
import { btn, card, field, iconBtn, meter, row, setMeter, tab, toggle } from "../../ui/components";

const P = "f-cleaner-uninstaller-";

type Kind = "caches" | "dev" | "downloads" | "screenshots" | "backups" | "leftovers" | "duplicates";
interface Row { id: number; kind: Kind; title: string; name: string | null; place: string; bytes: number; count: number; checked: boolean; note: "" | "running" | "left"; modified: number | null }
interface Done { moved: number; left: number; bytes: number; why: string[] }
interface Un { key: string; name: string; phase: "running" | "waiting" | "gone" | "still" | "failed"; message: string; winget: string | null }
interface Live { phase: "idle" | "scanning" | "cleaning" | "dupes"; scanning: string; rows: Row[]; left: Row[]; dupes: Row[][]; dupesAt: number | null; done: Done | null; progress: [number, number]; bin: [number, number] | null; scannedAt: number | null; un: Un | null; tab: string }
interface App { key: string; name: string; version: string; publisher: string; store: boolean; cmd: string | null; winget: string | null }
interface Entry { name: string; command: string; place: "user" | "machine" | "folder"; missing: boolean }
interface Lane {
  el: HTMLElement;
  /** Shows what would move; it moves only on the card's own button. */
  ask(rows: Row[]): void;
  /** Shows the command; it runs only on the card's own button. */
  askRun(name: string, cmd: string, run: () => void): void;
  draw(progressOnly: boolean): void;
}

// The groups of the list, in the order they are shown.
const GROUPS: [Kind, string][] = [
  ["caches", N_("Caches and temporary files")],
  ["dev", N_("Developer files")],
  ["downloads", N_("Telegram downloads")],
  ["screenshots", N_("Old screenshots")],
  ["backups", N_("Device backups")],
];
const NOTE = { running: N_("Its program is running. Close it to clean this."), left: N_("Could not be moved, so it stays where it is.") } as const;
// Why a file stayed, by the word Rust gives; anything else means it was no longer the file that was listed.
const WHY: Record<string, string> = {
  busy: N_("Windows did not move it: it is open in a program, needs an administrator or is too large for the Recycle Bin."),
  link: N_("A link or a file kept in the cloud is never touched."),
  drive: N_("A network or removable drive has no Recycle Bin."),
};
const CHANGED = N_("It changed after the scan: scan again.");
const PLACE = { user: N_("This user"), machine: N_("All users"), folder: N_("Startup folder") } as const;
const UNITS = [N_("{n} B"), N_("{n} KB"), N_("{n} MB"), N_("{n} GB"), N_("{n} TB")];
const FILE_KINDS: [string, string][] = [["image", N_("Pictures")], ["video", N_("Videos")], ["audio", N_("Audio")], ["document", N_("Documents")], ["archive", N_("Archives")], ["other", N_("Other files")]];

// What Rust last said, and who draws it in this window.
let live: Live | null = null;
let watching = false;
const views = new Map<string, (progressOnly: boolean) => void>();
const redraw = (progressOnly = false) => views.forEach((draw) => draw(progressOnly));
// The ticks of this window. A row the user has not touched is ticked as the scan offered it.
const ticks = new Map<number, boolean>();
const ticked = (r: Row) => ticks.get(r.id) ?? r.checked;
const sum = (rows: Row[]) => rows.reduce((a, r) => a + r.bytes, 0);

/** Follows the feature's state from now on. Listening costs nothing: Rust says something only when it works. */
function watch(cx: FeatureCtx) {
  if (watching) return;
  watching = true;
  cx.on<Live>("state", (s) => {
    live = s;
    redraw();
  });
  cx.on<[number, number]>("progress", (p) => {
    if (!live) return;
    live.progress = p;
    redraw(true);
  });
  void cx.call<Live>("state").then((s) => {
    if (!s || live) return;
    live = s;
    redraw();
  });
}

const small = (b: HTMLButtonElement) => (b.classList.add("sm"), b);

/** "512 B", "1.4 MB", "118 GB": binary steps, as Windows counts. */
function sizeText(cx: FeatureCtx, value: number): string {
  let n = Math.max(0, value);
  let unit = 0;
  while (n >= 1000 && unit < UNITS.length - 1) {
    n /= 1024;
    unit++;
  }
  return cx.t(UNITS[unit], { n: unit === 0 || n >= 100 ? Math.round(n) : n.toFixed(1) });
}

/** "5 minutes ago", in the app's language. */
function ago(secs: number): string {
  const d = Math.max(0, Math.round(Date.now() / 1000 - secs));
  const f = new Intl.RelativeTimeFormat(language(), { numeric: "auto" });
  if (d < 60) return f.format(0, "second");
  if (d < 3600) return f.format(-Math.round(d / 60), "minute");
  return d < 86400 ? f.format(-Math.round(d / 3600), "hour") : f.format(-Math.round(d / 86400), "day");
}

const day = (secs: number) => new Date(secs * 1000).toLocaleDateString(language(), { dateStyle: "medium" });

/** A `.field` around a native select. */
function select(opts: [string, string][], value: string, on: (v: string) => void): HTMLElement {
  const sel = h("select");
  for (const [v, label] of opts) sel.append(h("option", { value: v, text: label }));
  sel.value = value;
  sel.addEventListener("change", () => on(sel.value));
  return h("label", { class: "field" }, sel);
}

/** A tick box: flips itself and reports the new value. */
function check(on: boolean, label: string, onChange: (on: boolean) => void): HTMLButtonElement {
  const el = h("button", { class: `${P}check`, role: "checkbox", "aria-checked": String(on), "aria-label": label, title: label }, icon("check", 12));
  el.addEventListener("click", () => {
    const next = el.getAttribute("aria-checked") !== "true";
    el.setAttribute("aria-checked", String(next));
    onChange(next);
  });
  return el;
}

const rowName = (cx: FeatureCtx, r: Row) => r.name ?? cx.t(r.title);

/** An icon, a title and a line under it, then what the card offers. */
function line(tone: string, ico: string, title: string, text: string, ...rest: (Node | null)[]): HTMLElement {
  return h("div", { class: `${P}op` },
    h("span", { class: `isl-ico ${tone}` }, icon(ico, 14)),
    h("div", { class: `${P}op-text` }, h("b", { text: title }), text ? h("small", { text }) : null),
    ...rest);
}

// ── The review list ──────────────────────────────────────────────────────────

/** Rows under their group titles, each with its tick. `changed` runs after any tick. */
function rowList(cx: FeatureCtx, changed: () => void) {
  const el = h("div", { class: `${P}list` });

  function draw(groups: [string, Row[]][], tail: (HTMLElement | null)[] = []) {
    const top = el.scrollTop;
    el.replaceChildren();
    for (const [title, rows] of groups) {
      if (!rows.length) continue;
      const boxes: HTMLButtonElement[] = [];
      const head = check(rows.every(ticked), cx.t("Tick all of {group}", { group: title }), (on) => {
        for (const r of rows) ticks.set(r.id, on);
        for (const box of boxes) box.setAttribute("aria-checked", String(on));
        changed();
      });
      if (title) el.append(h("div", { class: `section-title ${P}group` }, head, h("span", { text: title }), h("span", { class: `${P}size`, text: sizeText(cx, sum(rows)) })));
      for (const r of rows) {
        const name = rowName(cx, r);
        const detail = r.note ? cx.t(NOTE[r.note])
          : r.name == null ? `${tn("{n} item", "{n} items", r.count, { n: r.count })} · ${r.place}`
          : r.modified ? `${day(r.modified)} · ${r.place}` : r.place;
        const box = check(ticked(r), name, (on) => {
          ticks.set(r.id, on);
          head.setAttribute("aria-checked", String(rows.every(ticked)));
          changed();
        });
        boxes.push(box);
        el.append(
          h("div", { class: `${P}row` },
            box,
            h("div", { class: `${P}name${r.note ? ` ${P}noted` : ""}` }, h("b", { text: name }), h("small", { text: detail, title: r.place })),
            h("span", { class: `${P}size`, text: sizeText(cx, r.bytes) }),
            small(iconBtn("external", cx.t("Show in Explorer"), () => void cx.call("reveal", { id: r.id })))),
        );
      }
    }
    el.append(...tail.filter((n): n is HTMLElement => !!n));
    el.scrollTop = top;
  }

  return { el, draw };
}

// ── The lane: what is about to happen, what is happening, how it ended ───────

function lane(cx: FeatureCtx): Lane {
  const el = h("div", { class: `${P}lane` });
  const bar = meter(0);
  const count = h("small");
  let asking: HTMLElement | null = null;
  let cleaning = false;

  const close = () => {
    asking = null;
    draw(false);
  };
  const show = (kids: (Node | string | null)[], tone: true | "warn") => {
    asking = card(kids, tone);
    asking.classList.add(`${P}asking`);
    draw(false);
  };

  const doneCard = (d: Done) => {
    const why = [...new Set(d.why.map((code) => WHY[code] ?? CHANGED))].map((text) => cx.t(text));
    const title = d.moved
      ? `${tn("{n} item moved to the Recycle Bin", "{n} items moved to the Recycle Bin", d.moved, { n: d.moved })} · ${sizeText(cx, d.bytes)}`
      : cx.t("Nothing was moved");
    const text = [
      d.left ? tn("{n} item stays where it is.", "{n} items stay where they are.", d.left, { n: d.left }) : "",
      ...why,
      d.moved ? cx.t("The space is freed when you empty the Recycle Bin.") : "",
    ].filter(Boolean).join(" ");
    const el = card(line(d.left ? "warn" : "ok", d.left ? "alert" : "check", title, text, iconBtn("close", cx.t("Dismiss"), () => void cx.call("dismiss"))), d.left ? "warn" : "ok");
    el.classList.add(`${P}wrap`);
    return el;
  };

  function draw(progressOnly: boolean) {
    const s = live;
    if (!s) return;
    if (s.phase === "cleaning") {
      const [n, of] = s.progress;
      setMeter(bar, of ? n / of : 0);
      count.textContent = cx.t("{done} of {total}", { done: n, total: of, count: of });
    }
    if (progressOnly && cleaning) return;
    cleaning = s.phase === "cleaning";
    const working = cleaning
      ? card(h("div", { class: `${P}op` },
          h("span", { class: "isl-ico info" }, icon("trash", 14)),
          h("div", { class: `${P}op-text` }, h("b", { text: cx.t("Moving to the Recycle Bin") }), count),
          bar,
          small(btn(cx.t("Stop"), "ghost", () => void cx.call("cancel")))))
      : null;
    el.replaceChildren(...[asking, working, !cleaning && s.done ? doneCard(s.done) : null].filter((n): n is HTMLElement => !!n));
  }

  return {
    el,
    draw,
    ask(rows) {
      const n = rows.reduce((a, r) => a + r.count, 0);
      const names = rows.slice(0, 4).map((r) => rowName(cx, r)).join(", ");
      const more = rows.length - 4;
      show(
        [
          h("b", { class: `${P}ask`, text: tn("Move {n} item to the Recycle Bin?", "Move {n} items to the Recycle Bin?", n, { n }) }),
          h("p", { class: `${P}picked`, text: `${sizeText(cx, sum(rows))} · ${names}${more > 0 ? ` ${tn("and {n} more", "and {n} more", more, { n: more })}` : ""}` }),
          h("p", { class: `${P}hint`, text: cx.t("Nothing is deleted for good. Everything can be put back from the Recycle Bin, and what Windows cannot move there stays where it is.") }),
          h("div", { class: `${P}actions` },
            btn(cx.t("Cancel"), "ghost", close),
            btn(cx.t("Move to the Recycle Bin"), "primary", () => {
              close();
              void cx.call("clean", { ids: rows.map((r) => r.id) });
            })),
        ],
        true,
      );
    },
    askRun(name, cmd, run) {
      show(
        [
          h("b", { class: `${P}ask`, text: cx.t("Uninstall {name}?", { name }) }),
          h("pre", { class: `${P}cmd`, text: cmd }),
          h("p", { class: `${P}hint`, text: cx.t("This starts the program's own uninstaller: follow its windows, and Windows asks you if it needs an administrator. Afterwards, what it left behind is listed for you to tick. Nothing else is removed.") }),
          h("div", { class: `${P}actions` },
            btn(cx.t("Cancel"), "ghost", close),
            btn(cx.t("Run the uninstaller"), "danger", () => {
              close();
              run();
            })),
        ],
        "warn",
      );
    },
  };
}

// ── Clean ────────────────────────────────────────────────────────────────────

function cleanView(cx: FeatureCtx, the: Lane, compact: boolean) {
  const head = h("div", { class: `${P}head` });
  const list = rowList(cx, paintHead);
  const el = h("div", { class: `${P}view${compact ? ` ${P}compact` : ""}` }, head, list.el);
  let listed = "";

  function paintHead() {
    const s = live;
    if (!s) return;
    const picked = s.rows.filter(ticked);
    const scanning = s.phase === "scanning";
    const waiting = s.un?.phase === "running" || s.un?.phase === "waiting";
    const title = scanning ? (s.scanning ? cx.t("Reading {what}…", { what: cx.t(s.scanning) }) : cx.t("Scanning…"))
      : s.scannedAt == null ? cx.t("Not scanned yet")
      : s.rows.length ? cx.t("{size} can be cleaned", { size: sizeText(cx, sum(s.rows)) })
      : cx.t("Nothing to clean");
    const detail = scanning ? cx.t("Only sizes are read. Nothing is removed.")
      : s.scannedAt == null ? cx.t("A scan only reads sizes. Nothing moves until you tick it and confirm.")
      : s.rows.length ? cx.t("{size} ticked · scanned {when}", { size: sizeText(cx, sum(picked)), when: ago(s.scannedAt) })
      : cx.t("Scanned {when}", { when: ago(s.scannedAt) });
    const scan = scanning ? btn(cx.t("Stop"), "ghost", () => void cx.call("cancel")) : btn(cx.t("Scan"), "secondary", () => void cx.call("scan"));
    scan.disabled = s.phase === "cleaning" || waiting;
    const clean = btn(cx.t("Clean…"), "primary", () => the.ask(picked));
    clean.disabled = s.phase !== "idle" || waiting || !picked.length;
    head.replaceChildren(
      h("div", { class: `${P}status` }, h("b", { text: title }), h("small", { text: detail })),
      scan,
      clean,
      ...(compact ? [iconBtn("external", cx.t("Open in a window"), () => void cx.call("open", { tab: "clean" }))] : []),
    );
  }

  const binRow = (bytes: number, items: number) =>
    h("div", {},
      h("div", { class: `section-title ${P}group` }, h("span", { text: cx.t("Recycle Bin") }), h("span", { class: `${P}size`, text: sizeText(cx, bytes) })),
      h("div", { class: `${P}row` },
        h("span", { class: "isl-ico" }, icon("trash", 14)),
        h("div", { class: `${P}name` },
          h("b", { text: tn("{n} item in the Recycle Bin", "{n} items in the Recycle Bin", items, { n: items }) }),
          h("small", { text: cx.t("Emptying it is what frees the space, and it deletes for good. That stays with Windows.") })),
        small(btn(cx.t("Open"), "secondary", () => void cx.call("openBin")))));

  function draw() {
    const s = live;
    if (!s) return;
    paintHead();
    // Drawn again only when it changed: the list keeps its place and its focus through a clean.
    const now = JSON.stringify([s.rows.map((r) => [r.id, r.count, r.bytes, r.note]), s.bin, s.scannedAt == null]);
    if (now === listed) return;
    listed = now;
    const empty = !s.rows.length && s.scannedAt == null
      ? h("p", { class: `${P}empty`, text: cx.t("Caches, temporary files, developer leftovers, old screenshots and device backups are looked for in fixed places. Each find is listed with its size before anything happens.") })
      : null;
    list.draw(GROUPS.map(([kind, title]): [string, Row[]] => [cx.t(title), s.rows.filter((r) => r.kind === kind)]), [empty, s.bin ? binRow(s.bin[0], s.bin[1]) : null]);
  }

  return { el, draw };
}

// ── Duplicates ───────────────────────────────────────────────────────────────

function dupesView(cx: FeatureCtx, the: Lane) {
  const head = h("div", { class: `${P}head` });
  const list = rowList(cx, paintHead);
  const el = h("div", { class: `${P}view` }, head, list.el);
  let listed = "";
  let progress = "";
  cx.on<[string, number, number]>("dupesProgress", ([stage, n, of]) => {
    progress = stage === "reading" ? cx.t("Reading your folders…") : cx.t("Comparing {done} of {total}", { done: n, total: of });
    paintHead();
  });

  /** Every group keeps one copy: all but the first are ticked, and the user sees each tick. */
  const extras = () => {
    for (const g of live?.dupes ?? []) g.forEach((r, i) => ticks.set(r.id, i > 0));
    listed = "";
    draw();
  };

  function paintHead() {
    const s = live;
    if (!s) return;
    const picked = s.dupes.flat().filter(ticked);
    const finding = s.phase === "dupes";
    const spare = s.dupes.reduce((a, g) => a + sum(g) - Math.max(...g.map((r) => r.bytes)), 0);
    const all = s.dupes.some((g) => g.every(ticked));
    const title = finding ? progress || cx.t("Looking for copies…")
      : s.dupesAt == null ? cx.t("Not searched yet")
      : s.dupes.length ? tn("{n} file has copies", "{n} files have copies", s.dupes.length, { n: s.dupes.length })
      : cx.t("No copies found");
    const detail = finding ? cx.t("Files are compared by size, then by their contents. Nothing is removed.")
      : s.dupesAt == null ? cx.t("Looks in Documents, Downloads, Desktop, Pictures and Videos, at files over 256 KB.")
      : s.dupes.length ? (all ? cx.t("Keep at least one copy of every file.") : cx.t("{size} could be freed · {ticked} ticked", { size: sizeText(cx, spare), ticked: sizeText(cx, sum(picked)) }))
      : cx.t("Searched {when}", { when: ago(s.dupesAt) });
    const find = finding ? btn(cx.t("Stop"), "ghost", () => void cx.call("cancel")) : btn(cx.t("Find copies"), "secondary", () => void cx.call("dupes"));
    find.disabled = s.phase !== "idle" && !finding;
    const tick = btn(cx.t("Tick the extra copies"), "ghost", extras);
    tick.disabled = s.phase !== "idle" || !s.dupes.length;
    const clean = btn(cx.t("Clean…"), "primary", () => the.ask(picked));
    clean.disabled = s.phase !== "idle" || !picked.length || all;
    head.replaceChildren(h("div", { class: `${P}status` }, h("b", { text: title }), h("small", { text: detail })), find, tick, clean);
  }

  function draw() {
    const s = live;
    if (!s) return;
    paintHead();
    const now = JSON.stringify(s.dupes.map((g) => g.map((r) => [r.id, r.note])));
    if (now === listed) return;
    listed = now;
    list.draw(s.dupes.map((g): [string, Row[]] => [
      `${tn("{n} copy", "{n} copies", g.length, { n: g.length })} · ${sizeText(cx, g[0].bytes)} ${cx.t("each")}`,
      g,
    ]));
  }

  return { el, draw };
}

// ── Disk ─────────────────────────────────────────────────────────────────────

interface DNode { n: string; d: boolean; b: number; p: number | null; k: number[]; path: string }
interface DState { phase: "idle" | "scanning"; root: string; nodes: DNode[]; at: number | null }
interface Arc { id: number; a0: number; a1: number; r0: number; r1: number }

const TAU = Math.PI * 2;

/** A drive or a folder as rings: the folder in the middle, what is in it around it. Click a part to zoom in, the middle to go back. It only reads. */
function diskView(cx: FeatureCtx) {
  let st: DState | null = null;
  let reading = "";
  let focus = 0;
  let hover = -1;
  let arcs: Arc[] = [];
  const canvas = h("canvas", { class: `${P}sun` }) as HTMLCanvasElement;
  const info = h("div", { class: `${P}status` });
  const bar = h("div", { class: `${P}head` });
  const el = h("div", { class: `${P}view` }, bar, canvas, h("div", { class: `${P}head` }, info));
  const path = field({ placeholder: cx.t("A folder, such as C:\\Users"), onInput: () => {} });
  const RINGS = 4;

  function paintBar() {
    const scanning = st?.phase === "scanning";
    const go = (root: string) => () => void cx.call("diskScan", { root });
    const kids: HTMLElement[] = [path];
    const scan = btn(cx.t("Scan folder"), "secondary", () => path.input.value.trim() && go(path.input.value.trim())());
    scan.disabled = scanning;
    kids.push(scan);
    if (scanning) kids.push(btn(cx.t("Stop"), "ghost", () => void cx.call("diskCancel")));
    bar.replaceChildren(...kids, ...drives.map((d) => small(btn(d, "ghost", go(d)))), ...(home ? [small(btn(cx.t("Home"), "ghost", go(home)))] : []));
  }
  let drives: string[] = [];
  let home = "";
  void cx.call<{ drives: string[]; home: string }>("diskDrives").then((r) => {
    drives = r?.drives ?? [];
    home = r?.home ?? "";
    paintBar();
  });

  const color = (nodes: DNode[], id: number, level: number) => {
    // The hue follows the part of the root the node lies in; deeper rings are lighter; files are paler.
    let top = id;
    while (nodes[top].p !== null && nodes[top].p !== focus && nodes[top].p !== 0) top = nodes[top].p!;
    const hue = (nodes[focus].k.indexOf(top) * 47 + 20) % 360;
    return `hsl(${hue} ${nodes[id].d ? 55 : 25}% ${38 + level * 7}%)`;
  };

  function draw() {
    const ratio = window.devicePixelRatio || 1;
    const box = canvas.getBoundingClientRect();
    const size = Math.max(160, Math.min(box.width, box.height));
    canvas.width = Math.round(box.width * ratio);
    canvas.height = Math.round(box.height * ratio);
    const g = canvas.getContext("2d")!;
    g.setTransform(ratio, 0, 0, ratio, 0, 0);
    g.clearRect(0, 0, box.width, box.height);
    arcs = [];
    const nodes = st?.nodes ?? [];
    const ink = getComputedStyle(canvas).getPropertyValue("--ink").trim() || "#ddd";
    if (!nodes.length) {
      g.fillStyle = ink;
      g.globalAlpha = 0.6;
      g.textAlign = "center";
      g.font = "13px sans-serif";
      const text = st?.phase === "scanning" ? `${cx.t("Reading")} ${reading}` : cx.t("Pick a drive or type a folder. Only sizes are read.");
      g.fillText(text.length > 70 ? `${text.slice(0, 69)}…` : text, box.width / 2, box.height / 2);
      return;
    }
    const cxn = box.width / 2;
    const cyn = box.height / 2;
    const hole = size * 0.13;
    const ring = (size / 2 - 4 - hole) / RINGS;
    const root = nodes[focus];
    const walk = (id: number, a0: number, level: number) => {
      let a = a0;
      for (const k of nodes[id].k) {
        const span = (nodes[k].b / root.b) * TAU;
        if (span < 0.004) continue;
        const r0 = hole + (level - 1) * ring;
        arcs.push({ id: k, a0: a, a1: a + span, r0, r1: r0 + ring });
        if (level < RINGS) walk(k, a, level + 1);
        a += span;
      }
    };
    if (root.b > 0) walk(focus, -Math.PI / 2, 1);
    for (const arc of arcs) {
      const level = Math.round((arc.r0 - hole) / ring) + 1;
      g.beginPath();
      g.arc(cxn, cyn, arc.r1 - 1, arc.a0, arc.a1);
      g.arc(cxn, cyn, arc.r0, arc.a1, arc.a0, true);
      g.closePath();
      g.fillStyle = color(nodes, arc.id, level);
      g.globalAlpha = hover === arc.id ? 1 : 0.85;
      g.fill();
      g.globalAlpha = 1;
      g.strokeStyle = "rgba(0,0,0,.35)";
      g.lineWidth = 0.75;
      g.stroke();
    }
    g.fillStyle = ink;
    g.textAlign = "center";
    g.font = "600 12px sans-serif";
    g.fillText(root.n.length > 16 ? `${root.n.slice(0, 15)}…` : root.n, cxn, cyn - 2);
    g.globalAlpha = 0.65;
    g.font = "11px sans-serif";
    g.fillText(sizeText(cx, root.b), cxn, cyn + 13);
    g.globalAlpha = 1;
  }

  function paintInfo() {
    const nodes = st?.nodes ?? [];
    const n = nodes[hover >= 0 ? hover : focus];
    if (!n) {
      info.replaceChildren(h("b", { text: st?.phase === "scanning" ? cx.t("Scanning…") : cx.t("Nothing scanned yet") }), h("small", { text: st?.phase === "scanning" ? reading : cx.t("The chart never deletes anything. Show a part in Explorer to deal with it there.") }));
      return;
    }
    info.replaceChildren(
      h("b", { text: `${n.n} · ${sizeText(cx, n.b)}` }),
      h("small", { text: n.path, title: n.path }),
    );
  }

  function at(e: MouseEvent): number {
    const box = canvas.getBoundingClientRect();
    const x = e.clientX - box.left - box.width / 2;
    const y = e.clientY - box.top - box.height / 2;
    const r = Math.hypot(x, y);
    let a = Math.atan2(y, x);
    if (a < -Math.PI / 2) a += TAU;
    const hit = arcs.find((arc) => r >= arc.r0 && r < arc.r1 && a >= arc.a0 && a < arc.a1) ?? arcs.find((arc) => r >= arc.r0 && r < arc.r1 && a + TAU >= arc.a0 && a + TAU < arc.a1);
    return hit ? hit.id : -1;
  }

  canvas.addEventListener("mousemove", (e) => {
    const id = at(e);
    if (id !== hover) {
      hover = id;
      draw();
      paintInfo();
    }
  });
  canvas.addEventListener("mouseleave", () => {
    hover = -1;
    draw();
    paintInfo();
  });
  canvas.addEventListener("click", (e) => {
    const nodes = st?.nodes ?? [];
    const id = at(e);
    if (id >= 0) {
      if (nodes[id].d && nodes[id].k.length) focus = id;
    } else if (nodes[focus]?.p != null) {
      focus = nodes[focus].p!;
    }
    hover = -1;
    draw();
    paintInfo();
    paintReveal();
  });
  canvas.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    const id = at(e);
    void cx.call("diskReveal", { id: id >= 0 ? id : focus });
  });

  const revealBtn = btn(cx.t("Show in Explorer"), "secondary", () => void cx.call("diskReveal", { id: hover >= 0 ? hover : focus }));
  function paintReveal() {
    revealBtn.disabled = !st?.nodes.length;
  }
  el.lastElementChild!.append(revealBtn);

  cx.on<DState>("disk", (s) => {
    const fresh = s.root !== st?.root || s.phase === "scanning";
    st = s;
    if (fresh || focus >= s.nodes.length) focus = 0;
    hover = -1;
    paintBar();
    draw();
    paintInfo();
    paintReveal();
  });
  cx.on<string>("diskProgress", (dir) => {
    reading = dir;
    if (st?.phase === "scanning") {
      draw();
      paintInfo();
    }
  });
  new ResizeObserver(() => draw()).observe(canvas);
  paintBar();
  paintInfo();
  paintReveal();

  return {
    el,
    show() {
      void cx.call("disk");
      window.requestAnimationFrame(draw);
    },
  };
}

// ── Uninstall ────────────────────────────────────────────────────────────────

function uninstallView(cx: FeatureCtx, the: Lane) {
  const status = h("div", { class: `${P}un ${P}wrap` });
  const listEl = h("div", { class: `${P}list` });
  let apps: App[] | null = null;
  let query = "";
  let loading = false;
  let was = "";
  let listed = "";

  const box = field({
    icon: "search",
    placeholder: cx.t("Search the installed programs"),
    onInput: (v) => {
      query = v.trim().toLowerCase();
      drawList();
    },
  });
  const el = h("div", { class: `${P}view` }, status, box, listEl);
  const say = (text: string) => listEl.replaceChildren(h("p", { class: `${P}empty`, text }));

  async function load() {
    loading = true;
    drawList();
    apps = await cx.call<App[]>("apps");
    loading = false;
    drawList();
  }

  function drawList() {
    if (loading || !apps) return say(loading ? cx.t("Reading the installed programs…") : cx.t("The installed programs could not be read."));
    const shown = apps.filter((a) => !query || `${a.name} ${a.publisher}`.toLowerCase().includes(query));
    if (!shown.length) return say(cx.t("Nothing installed matches “{query}”.", { query }));
    listEl.replaceChildren(
      ...shown.map((a) => {
        const cmd = a.cmd ?? a.winget;
        const about = [a.publisher, a.version, a.store ? cx.t("Microsoft Store") : ""].filter(Boolean).join(" · ");
        const act = cmd
          ? btn(cx.t("Uninstall"), "secondary", () => the.askRun(a.name, cmd, () => void cx.call("uninstall", { key: a.key, winget: !a.cmd })))
          : btn(cx.t("Open in Windows"), "ghost", () => void cx.call("openApps"));
        if (!cmd) act.title = cx.t("Its uninstaller cannot be started from here. Windows' own list can remove it.");
        return h("div", { class: `${P}row` }, h("div", { class: `${P}name` }, h("b", { text: a.name }), h("small", { text: about })), small(act));
      }),
    );
  }

  // What the uninstalled program left: one list and one button, kept through every redraw so a tick stays where it is.
  const move: HTMLButtonElement = btn(cx.t("Move to the Recycle Bin…"), "primary", () => live && the.ask(live.left.filter(ticked)));
  const paintMove = () => {
    move.disabled = !live || live.phase !== "idle" || !live.left.some(ticked);
  };
  const leftovers = rowList(cx, paintMove);
  const forget = () => iconBtn("close", cx.t("Dismiss"), () => void cx.call("forget"));

  function unCard(u: Un, s: Live): HTMLElement {
    const name = { name: u.name };
    switch (u.phase) {
      case "running":
        return card(line("info", "trash", cx.t("Uninstalling {name}", name), cx.t("Its uninstaller is open. Finish it there.")));
      case "waiting":
        return card(line("info", "timer", cx.t("Waiting for {name} to be gone", name), cx.t("Windows still lists it. Some uninstallers take a minute after their window closes."),
          small(btn(cx.t("Stop waiting"), "ghost", () => void cx.call("stopWaiting")))));
      case "still":
        return card(line("warn", "alert", cx.t("{name} is still installed", name), cx.t("If its uninstaller is still open, finish it there, then check again."),
          small(btn(cx.t("Check again"), "secondary", () => void cx.call("recheck"))), forget()), "warn");
      case "failed": {
        const winget = u.winget;
        const text = u.message === "admin" ? cx.t("Its uninstaller needs administrator rights and could not be started from here.") : u.message;
        const next = winget
          ? btn(cx.t("Use winget instead"), "secondary", () => the.askRun(u.name, winget, () => void cx.call("uninstall", { key: u.key, winget: true })))
          : btn(cx.t("Open in Windows"), "secondary", () => void cx.call("openApps"));
        return card(line("bad", "alert", cx.t("{name} was not uninstalled", name), text, small(next), forget()), "bad");
      }
      case "gone": {
        const text = s.left.length
          ? cx.t("These carry its name and were left behind. Tick what you want moved to the Recycle Bin. Its settings and saved data may be among them: keep those if you will install it again.")
          : cx.t("Nothing with its name was left in the usual places.");
        return card([line("ok", "check", cx.t("{name} is uninstalled", name), text, forget()), s.left.length ? leftovers.el : null, s.left.length ? h("div", { class: `${P}actions` }, move) : null], "ok");
      }
    }
  }

  function draw() {
    const s = live;
    if (!s) return;
    const u = s.un;
    const busy = u?.phase === "running" || u?.phase === "waiting";
    const now = JSON.stringify(s.left.map((r) => [r.id, r.note]));
    if (now !== listed) leftovers.draw([["", s.left]]);
    listed = now;
    paintMove();
    status.replaceChildren(...(u ? [unCard(u, s)] : []));
    // While an uninstaller is open there is nothing else to start.
    box.hidden = listEl.hidden = busy;
    // The program is gone, or another list is due: read what Windows lists again.
    const key = u ? `${u.key}:${u.phase}` : "";
    if (key !== was && u?.phase === "gone" && apps) void load();
    was = key;
  }

  return {
    el,
    draw,
    show() {
      if (!apps && !loading) void load();
      box.input.focus();
    },
  };
}

// ── Startup ──────────────────────────────────────────────────────────────────

function startupView(cx: FeatureCtx) {
  const head = h("div", { class: `${P}head` });
  const listEl = h("div", { class: `${P}list` });
  let asked = false;

  async function load() {
    asked = true;
    const rows = (await cx.call<Entry[]>("startup")) ?? [];
    const gone = rows.filter((e) => e.missing).length;
    head.replaceChildren(
      h("div", { class: `${P}status` },
        h("b", { text: tn("{n} program starts with Windows", "{n} programs start with Windows", rows.length, { n: rows.length }) }),
        h("small", { text: gone ? tn("{n} of them names a program that is gone.", "{n} of them name a program that is gone.", gone, { n: gone }) : cx.t("Every one of them names a program that is there.") })),
      iconBtn("refresh", cx.t("Read again"), () => void load()),
      btn(cx.t("Open Startup apps"), "secondary", () => void cx.call("openStartup")),
    );
    listEl.replaceChildren(
      ...rows.map((e) =>
        h("div", { class: `${P}row` },
          h("span", { class: `isl-ico${e.missing ? " warn" : ""}` }, icon(e.missing ? "alert" : "play", 14)),
          h("div", { class: `${P}name${e.missing ? ` ${P}noted` : ""}` },
            h("b", { text: e.name }),
            h("small", { text: e.missing ? `${cx.t("Its program is gone")} · ${e.command}` : e.command, title: e.command })),
          h("span", { class: `${P}size`, text: cx.t(PLACE[e.place]) }))),
      h("p", { class: `${P}hint ${P}foot`, text: cx.t("This list is only read here. Windows turns an entry on or off in Startup apps; nothing in the registry is changed by this app.") }),
    );
  }

  return { el: h("div", { class: `${P}view` }, head, listEl), show: () => void (asked || load()) };
}

// ── The feature ──────────────────────────────────────────────────────────────

function section(cx: FeatureCtx): SectionHost {
  const the = lane(cx);
  const clean = cleanView(cx, the, true);
  const draw = (progressOnly = false) => {
    the.draw(progressOnly);
    if (!progressOnly) clean.draw();
  };
  views.set("section", draw);
  watch(cx);
  return { el: h("div", { class: `${P}page` }, the.el, clean.el), show: () => draw(), hide() {}, sync() {} };
}

export default defineFeature({
  id: "cleaner-uninstaller",

  sections: [{ id: "cleaner", title: "Cleaner", icon: "trash", letter: "z", height: 300, build: section }],

  settings(cx) {
    const box = h("div", { class: `${P}settings` });
    const days = (list: number[]) => list.map((n): [string, string] => [String(n), tn("{n} day", "{n} days", n, { n })]);
    const draw = (s: Value) => {
      const set = (key: string) => (on: boolean) => void cx.settings.patch({ [key]: on });
      const kinds = new Set((s.downloadsKinds as string[] | undefined) ?? []);
      box.replaceChildren(
        card([
          row(cx.t("Scan by itself"),
            select([["off", cx.t("Only when I ask")], ["daily", cx.t("Every day")], ["weekly", cx.t("Every week")]], String(s.schedule ?? "off"), (v) => void cx.settings.patch({ schedule: v })),
            cx.t("That scan reads sizes and tells you what it found. Cleaning always waits for your click.")),
          row(cx.t("Tell me what that scan found"), toggle(s.notify !== false, set("notify"))),
          row(cx.t("List screenshots older than"),
            select([["0", cx.t("Never")], ...days([7, 14, 30, 60, 90])], String(s.screenshotDays ?? 30), (v) => void cx.settings.patch({ screenshotDays: Number(v) })),
            cx.t("Only the ones Windows named itself, in your Screenshots folder. They are listed, never ticked for you.")),
        ]),
        h("div", { class: "section-title", text: cx.t("Telegram downloads") }),
        card([
          row(cx.t("List old Telegram downloads"), toggle(s.downloads === true, set("downloads")), cx.t("Files in Downloads\\Telegram Desktop that nobody changed for the time below.")),
          row(cx.t("Older than"), select(days([1, 2, 7, 14, 30]), String(s.downloadsDays ?? 7), (v) => void cx.settings.patch({ downloadsDays: Number(v) }))),
          h("div", { class: `${P}kinds` },
            ...FILE_KINDS.map(([kind, title]) => {
              const b: HTMLButtonElement = tab("file", cx.t(title), () => {
                if (!kinds.delete(kind)) kinds.add(kind);
                b.classList.toggle("on", kinds.has(kind));
                void cx.settings.patch({ downloadsKinds: [...kinds] });
              });
              b.classList.toggle("on", kinds.has(kind));
              return b;
            })),
        ]),
        h("div", { class: `${P}actions ${P}left` },
          btn(cx.t("Open the cleaner"), "secondary", () => void cx.call("open", { tab: "clean" })),
          btn(cx.t("Uninstall a program"), "secondary", () => void cx.call("open", { tab: "uninstall" }))),
      );
    };
    void cx.settings.get().then(draw);
    return box;
  },

  windows: {
    main(root, cx) {
      const the = lane(cx);
      const clean = cleanView(cx, the, false);
      const uninstall = uninstallView(cx, the);
      const startup = startupView(cx);
      const dupes = dupesView(cx, the);
      const disk = diskView(cx);
      const pages: Record<string, { el: HTMLElement; show?: () => void }> = { clean, uninstall, startup, dupes, disk };
      const body = h("div", { class: `${P}body` });
      const tabs: [string, HTMLButtonElement][] = (
        [["clean", "trash", "Clean"], ["uninstall", "close", "Uninstall"], ["startup", "play", "Startup"], ["dupes", "copy", "Duplicates"], ["disk", "monitor", "Disk"]] as const
      ).map(([id, ico, title]): [string, HTMLButtonElement] => [id, tab(ico, cx.t(title), () => go(id))]);
      function go(id: string) {
        if (!(id in pages)) return;
        tabs.forEach(([k, b]) => b.classList.toggle("on", k === id));
        body.replaceChildren(pages[id].el);
        pages[id].show?.();
      }
      views.set("window", (progressOnly) => {
        the.draw(progressOnly);
        if (progressOnly) return;
        clean.draw();
        uninstall.draw();
        dupes.draw();
      });
      root.append(
        h("div", { class: `win ${P}win` },
          h("div", { class: "win-bar" }, icon("trash", 14), h("span", { text: cx.t("Cleaner") }),
            h("button", { class: "close", title: cx.t("Close"), onclick: () => void cx.call("close"), text: "×" })),
          h("div", { class: `${P}tabs` }, ...tabs.map(([, b]) => b)),
          the.el,
          body),
      );
      go("clean");
      watch(cx);
      cx.on<string>("tab", go);
      void cx.call<Live>("state").then((s) => s && go(s.tab));
    },
  },

  // The command bar filters these by what is typed.
  commands: (_query, cx) => [
    { id: "cleaner:clean", title: cx.t("Clean up this PC"), subtitle: cx.t("Scan, review, move to the Recycle Bin"), icon: "trash", keywords: ["clean", "cache", "temp", "junk", "space"], run: () => void cx.call("open", { tab: "clean" }) },
    { id: "cleaner:uninstall", title: cx.t("Uninstall a program"), subtitle: cx.t("Run its uninstaller, then review what it left"), icon: "close", keywords: ["uninstall", "remove", "app", "program"], run: () => void cx.call("open", { tab: "uninstall" }) },
  ],

  start(cx) {
    // The scan that ran by itself: it only says what it found.
    return cx.on<{ bytes: number; count: number }>("found", (e) =>
      cx.island.notice({ kind: "notification", title: cx.t("{size} can be cleaned", { size: sizeText(cx, e.bytes) }), detail: cx.t("Review it in the Cleaner"), icon: "trash", tone: "info", section: "cleaner" }));
  },
});
