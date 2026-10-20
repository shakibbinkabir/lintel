// Controls toggles — the page side of the feature (docs/ARCHITECTURE.md §2.3).
// Two sections: Controls (volume, brightness, quick toggles, a rail of the other features)
// and Notes (the scratchpad). Windows is asked every time a section is shown; nothing is remembered.

import "./style.css";
import { defineFeature, type FeatureCtx, type SectionHost } from "../../core/feature";
import { FEATURES, featureCtx, featureOn } from "../../core/registry";
import { btn, iconBtn, slider, setMeter, tile } from "../../ui/components";
import { icon } from "../../ui/icons";
import { h } from "../../views/dom";
import { tn } from "../../i18n/i18n";

interface State {
  themeApp: boolean;
  hidden: boolean;
  icons: boolean | null;
  bin: { items: number; bytes: number } | null;
  volume: { level: number; mute: boolean } | null;
}

const size = (b: number) =>
  b < 1024 ? `${b} B` : b < 1048576 ? `${(b / 1024).toFixed(0)} KB` : b < 1073741824 ? `${(b / 1048576).toFixed(1)} MB` : `${(b / 1073741824).toFixed(2)} GB`;

// ── Controls ──────────────────────────────────────────────────────────────────

function buildControls(cx: FeatureCtx): SectionHost {
  const t = cx.t;
  let st: State | null = null;
  let poll = 0;

  // Volume
  let lastSend = 0;
  const vol = slider(0, {
    label: t("Volume"),
    onInput: (f) => {
      if (Date.now() - lastSend > 60) {
        lastSend = Date.now();
        void cx.call("volume", { level: f });
      }
    },
    onChange: (f) => void cx.call("volume", { level: f }),
  });
  const mute = iconBtn("volume", t("Mute"), () => void cx.call("volume", { mute: !st?.volume?.mute }).then(refresh));
  const volRow = h("div", { class: "f-controls-toggles-slide" }, mute, vol);

  // Brightness. With the displays feature on it goes that feature's way (WMI for the panel, DDC/CI for a
  // monitor, paced and coalesced in Rust), to the first display whose backlight can be moved. With it off,
  // the built-in panel through this feature's own PowerShell call. Hidden when neither finds a backlight.
  const DISPLAYS = "displays-keepawake";
  let display: string | null = null;
  let lastBri = 0;
  const setBri = (f: number) =>
    void (display ? featureCtx(DISPLAYS).call("set", { id: display, level: f }) : cx.call("brightness", { level: f }));
  const readBri = async (): Promise<number | null> => {
    display = null;
    if (!featureOn(DISPLAYS)) return cx.call<number | null>("brightness_get");
    const list = await featureCtx(DISPLAYS).call<{ id: string; method: string; level: number }[]>("displays");
    const d = list?.find((x) => x.method !== "none");
    display = d?.id ?? null;
    return d?.level ?? null;
  };
  const bri = slider(0, {
    label: t("Brightness"),
    // While dragged, only the displays feature is written to: a PowerShell start per step would pile up.
    onInput: (f) => {
      if (display && Date.now() - lastBri > 120) {
        lastBri = Date.now();
        setBri(f);
      }
    },
    onChange: setBri,
  });
  const briRow = h("div", { class: "f-controls-toggles-slide", hidden: true }, h("span", { class: "f-controls-toggles-ico" }, icon("sun", 16)), bri);

  const flash = (el: HTMLButtonElement, text: string, err?: string) => {
    const span = el.lastElementChild as HTMLElement;
    const was = span.textContent;
    el.classList.add("bad");
    span.textContent = text;
    if (err) el.title = err;
    setTimeout(() => {
      span.textContent = was;
      el.classList.remove("bad");
      paint();
    }, 2500);
  };
  const toggleTile = (ic: string, label: string, what: string) => {
    const el = tile(ic, label, async () => {
      const r = await cx.call<State>("toggle", { what });
      if (r) {
        st = r;
        paint();
      } else flash(el, t("Not applied"), t("Windows did not apply it"));
    });
    return el;
  };
  const dark = toggleTile("moon", t("Dark mode"), "theme");
  const hid = toggleTile("eye", t("Hidden files"), "hidden");
  const icons = toggleTile("grid", t("Desktop icons"), "icons");
  const lock = tile("lock", t("Lock"), () => void cx.call("lock"));
  const off = tile("monitor", t("Display off"), () => void cx.call("display_off"));
  const bin = tile("trash", t("Recycle Bin"), async () => {
    await refresh();
    if (!st?.bin) return flash(bin, t("Unavailable"));
    if (st.bin.items === 0) return flash(bin, t("Already empty"));
    ask.hidden = false;
    askText.textContent = tn(
      "Empty the Recycle Bin? {n} item, {size}. It is deleted for good.",
      "Empty the Recycle Bin? {n} items, {size}. They are deleted for good.",
      st.bin.items,
      { n: st.bin.items, size: size(st.bin.bytes) },
    );
  });

  // The only thing here that deletes: it names the count and size, every time.
  const ask = h("div", { class: "f-controls-toggles-ask", hidden: true });
  const askText = h("span");
  ask.append(
    askText,
    btn(t("Cancel"), "secondary", () => (ask.hidden = true)),
    btn(t("Empty Recycle Bin"), "danger", async () => {
      ask.hidden = true;
      const r = await cx.call<State>("bin_empty", { confirm: true });
      if (r) {
        st = r;
        paint();
      } else flash(bin, t("Not applied"));
    }),
  );

  const rail = h("div", { class: "f-controls-toggles-rail" });

  function paint() {
    if (!st) return;
    dark.classList.toggle("ok", st.themeApp);
    hid.classList.toggle("ok", st.hidden);
    icons.hidden = st.icons === null; // the 0x7402 command needs the desktop window: no tile without it
    icons.classList.toggle("ok", st.icons === true);
    const n = st.bin?.items ?? 0;
    (bin.lastElementChild as HTMLElement).textContent = n ? `${t("Recycle Bin")} ${n}` : t("Recycle Bin");
    if (st.volume) {
      if (!vol.classList.contains("dragging")) setMeter(vol, st.volume.mute ? 0 : st.volume.level);
      mute.replaceChildren(icon(st.volume.mute ? "mute" : "volume", 14));
    }
  }

  async function refresh() {
    const r = await cx.call<State>("state");
    if (r) {
      st = r;
      paint();
    }
  }

  function paintRail() {
    // A section that is a tab of the header already has its button: no tile says it twice.
    const elsewhere = (f: (typeof FEATURES)[number]) => !featureCtx(f.id).island.inHeader(f.sections![0].id);
    const others = FEATURES.filter((f) => f.id !== "controls-toggles" && featureOn(f.id) && f.sections?.length && elsewhere(f)).map((f) => {
      const def = f.sections![0];
      return tile(def.icon, t(def.title), () => featureCtx(f.id).island.open(def.id));
    });
    rail.replaceChildren(dark, hid, icons, bin, lock, off, ...others);
  }

  const el = h("div", { class: "f-controls-toggles-page" }, volRow, briRow, rail, ask);
  return {
    el,
    show() {
      ask.hidden = true;
      paintRail();
      void refresh();
      void readBri().then((b) => {
        briRow.hidden = b == null;
        if (b != null) setMeter(bri, b);
      });
      poll = window.setInterval(refresh, 2000);
    },
    hide() {
      clearInterval(poll);
    },
    sync() {},
    command: () => (ask.hidden ? false : (ask.hidden = true)),
  };
}

// ── Scratchpad ────────────────────────────────────────────────────────────────

interface Tab {
  id: number;
  name: string;
  text: string;
}
interface Pad {
  active: number;
  tabs: Tab[];
  trash: Tab[];
}

/** Escapes first, then a small subset of Markdown; links only to http(s). */
function markdown(src: string): string {
  const esc = (s: string) => s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
  const inline = (s: string) =>
    esc(s)
      .replace(/`([^`]+)`/g, "<code>$1</code>")
      .replace(/\*\*([^*]+)\*\*/g, "<b>$1</b>")
      .replace(/(^|[^*])\*([^*\n]+)\*/g, "$1<i>$2</i>")
      .replace(/\[([^\]]+)\]\((https?:\/\/[^)\s]+)\)/g, '<a href="$2" target="_blank" rel="noreferrer">$1</a>');
  const out: string[] = [];
  let list = "";
  let fence = false;
  const close = () => {
    if (list) {
      out.push(`</${list}>`);
      list = "";
    }
  };
  for (const [at, line] of src.split("\n").entries()) {
    if (line.startsWith("```")) {
      close();
      out.push(fence ? "</pre>" : "<pre>");
      fence = !fence;
      continue;
    }
    if (fence) {
      out.push(esc(line) + "\n");
      continue;
    }
    const hd = /^(#{1,3})\s+(.*)/.exec(line);
    const li = /^\s*([-*]|\d+\.)\s+(.*)/.exec(line);
    if (li) {
      const kind = /\d/.test(li[1]) ? "ol" : "ul";
      if (list !== kind) {
        close();
        out.push(`<${kind}>`);
        list = kind;
      }
      // "- [ ] x" and "- [x] x" are checkboxes; a click finds its line by `data-line`.
      const box = /^\[([ xX])\]\s+(.*)/.exec(li[2]);
      out.push(box ? `<li class="task"><input type="checkbox" data-line="${at}"${box[1] === " " ? "" : " checked"}> ${inline(box[2])}</li>` : `<li>${inline(li[2])}</li>`);
      continue;
    }
    close();
    if (hd) out.push(`<h${hd[1].length}>${inline(hd[2])}</h${hd[1].length}>`);
    else if (/^---+$/.test(line)) out.push("<hr>");
    else if (line.startsWith("> ")) out.push(`<blockquote>${inline(line.slice(2))}</blockquote>`);
    else if (line.trim()) out.push(`<p>${inline(line)}</p>`);
  }
  close();
  if (fence) out.push("</pre>");
  return out.join("");
}

function buildNotes(cx: FeatureCtx): SectionHost {
  const t = cx.t;
  let pad: Pad = { active: 1, tabs: [{ id: 1, name: "Note 1", text: "" }], trash: [] };
  let loaded = false;
  let preview = false;
  let saveTimer = 0;
  let noteTimer = 0;

  const cur = () => pad.tabs.find((x) => x.id === pad.active) ?? pad.tabs[0];
  const save = () => {
    clearTimeout(saveTimer);
    void cx.call("scratch_save", { data: pad });
  };
  const later = () => {
    clearTimeout(saveTimer);
    saveTimer = window.setTimeout(() => {
      keep();
      save();
    }, 500);
  };
  /** Text on screen goes into the current tab. */
  function keep() {
    if (!preview && loaded) cur().text = area.value;
  }

  const tabs = h("div", { class: "f-controls-toggles-tabs" });
  const area = h("textarea", { class: "f-controls-toggles-area", spellcheck: "false", placeholder: t("Write anything. It is kept on this computer.") });
  const view = h("div", { class: "f-controls-toggles-md", hidden: true });
  const note = h("div", { class: "f-controls-toggles-note" });
  const find = h("input", { class: "f-controls-toggles-find", placeholder: t("Find"), spellcheck: "false" });

  const say = (text: string, undo?: () => void) => {
    clearTimeout(noteTimer);
    note.replaceChildren(...(text ? [text] : []), ...(undo ? [btn(t("Undo"), "ghost", () => { undo(); say(""); })] : []));
    if (text) noteTimer = window.setTimeout(() => note.replaceChildren(), 8000);
  };

  function drawTabs() {
    tabs.replaceChildren(
      ...pad.tabs.map((x) => {
        const b = h(
          "button",
          {
            class: "f-controls-toggles-tab" + (x.id === pad.active ? " on" : ""),
            title: t("Double-click to rename"),
            onclick: () => {
              if (x.id === pad.active) return;
              keep();
              pad.active = x.id;
              drawAll();
              later();
            },
            ondblclick: () => {
              const inp = h("input", { class: "f-controls-toggles-rename", value: x.name });
              inp.addEventListener("blur", () => {
                x.name = inp.value.trim() || x.name;
                drawTabs();
                later();
              });
              inp.addEventListener("keydown", (e) => (e as KeyboardEvent).key === "Enter" && inp.blur());
              b.replaceChildren(inp);
              inp.focus();
              inp.select();
            },
          },
          x.name,
        );
        return b;
      }),
      iconBtn("plus", t("New tab"), () => {
        keep();
        const id = Math.max(0, ...pad.tabs.map((x) => x.id), ...pad.trash.map((x) => x.id)) + 1;
        pad.tabs.push({ id, name: `Note ${id}`, text: "" });
        pad.active = id;
        drawAll();
        save();
        area.focus();
      }),
    );
  }

  function drawBody() {
    area.hidden = preview;
    view.hidden = !preview;
    if (preview) view.innerHTML = markdown(cur().text);
    else area.value = cur().text;
  }
  const drawAll = () => {
    drawTabs();
    drawBody();
  };

  // A tick in the preview changes that line of the text.
  view.addEventListener("change", (e) => {
    const box = e.target as HTMLInputElement;
    if (!box.matches("input[data-line]")) return;
    const lines = cur().text.split("\n");
    const at = Number(box.dataset.line);
    lines[at] = lines[at].replace(/\[[ xX]\]/, box.checked ? "[x]" : "[ ]");
    cur().text = lines.join("\n");
    later();
  });
  area.addEventListener("input", later);
  find.addEventListener("keydown", (e) => {
    if ((e as KeyboardEvent).key !== "Enter" || !find.value || preview) return;
    const text = area.value.toLowerCase();
    const q = find.value.toLowerCase();
    let i = text.indexOf(q, area.selectionEnd);
    if (i < 0) i = text.indexOf(q);
    if (i < 0) return say(t("Not found"));
    area.focus();
    area.setSelectionRange(i, i + q.length);
    say("");
  });

  const bar = h(
    "div",
    { class: "f-controls-toggles-bar" },
    tabs,
    find,
    iconBtn("eye", t("Markdown preview"), () => {
      keep();
      preview = !preview;
      drawBody();
    }),
    iconBtn("download", t("Export to Downloads"), async () => {
      keep();
      const p = await cx.call<string>("scratch_export", { name: cur().name, text: cur().text, ext: "md" });
      say(p ? t("Saved to {path}", { path: p }) : t("Could not export"));
    }),
    // Closing a tab moves it to the pad's own trash (last ten, kept in the same file); Undo brings it back.
    iconBtn("trash", t("Close tab"), () => {
      keep();
      const gone = cur();
      pad.tabs = pad.tabs.filter((x) => x !== gone);
      pad.trash = [gone, ...pad.trash].slice(0, 10);
      if (!pad.tabs.length) pad.tabs.push({ id: gone.id + 1, name: `Note ${gone.id + 1}`, text: "" });
      pad.active = pad.tabs[0].id;
      drawAll();
      save();
      say(t("Closed {name}.", { name: gone.name }), () => {
        pad.trash = pad.trash.filter((x) => x !== gone);
        pad.tabs.push(gone);
        pad.active = gone.id;
        drawAll();
        save();
      });
    }),
  );

  const el = h("div", { class: "f-controls-toggles-pad" }, bar, area, view, note);
  return {
    el,
    show() {
      void (async () => {
        if (!loaded) {
          const p = await cx.call<Pad | null>("scratch_load");
          if (p?.tabs?.length) pad = { ...p, trash: p.trash ?? [] };
          loaded = true;
        }
        drawAll();
        if (!preview) area.focus();
      })();
    },
    hide() {
      if (loaded) {
        keep();
        save();
      }
    },
    sync() {},
  };
}

export default defineFeature({
  id: "controls-toggles",
  voice: () => import("./voice"),
  sections: [
    { id: "controls", title: "Controls", icon: "sliders", letter: "o", height: 280, build: buildControls },
    { id: "notes", title: "Notes", icon: "text", letter: "n", height: 280, keyboard: true, build: buildNotes },
  ],
});
