// Session code view: opens from the focus line of a session. The file just
// changed (a few lines around the edit, the changed ones marked) and the last
// command it ran. "Follow" keeps it on the newest edit as they come.
// The command's output is not shown: the relay drops tool_response (privacy).

import { h, svg } from "./dom";
import { ICONS } from "../ui/icons";
import { fileName, type FileDiff } from "../core/diff";
import { N_, tl } from "../i18n/i18n";

const STRINGS = {
  back: N_("Back"),
  open: N_("Open in VS Code"),
  follow: N_("Follow edits"),
  nothing: N_("Nothing changed yet"),
  ran: N_("Last command"),
};

const SHOWN = 14;

export interface CodeViewHooks {
  dismiss(): void;
  open(path: string): void;
  follow: boolean;
  setFollow(on: boolean): void;
  /** Lines of the file from `from`, or null when it is not readable inside the session folder. */
  lines(path: string, from: number, count: number): Promise<string[] | null>;
}

/** The first line the edit touched, in the new file. */
function firstChanged(diff: FileDiff): number {
  for (const hunk of diff.hunks) {
    const add = hunk.lines.find((l) => l.kind === "added" && l.newLine > 0);
    if (add) return add.newLine;
  }
  return diff.hunks[0]?.newStart ?? 1;
}

export function buildCodeView(diff: FileDiff | null, command: string | null | undefined, hooks: CodeViewHooks): HTMLElement {
  const head = h(
    "div",
    { class: "diff-head" },
    h(
      "button",
      { class: "diff-back", title: tl(STRINGS.back), onclick: () => hooks.dismiss() },
      svg(ICONS.chevronLeft, 9, { stroke: 2.4 }),
      h("b", { text: diff ? fileName(diff.path) : tl(STRINGS.ran) }),
    ),
    h("span", { class: "grow" }),
    h(
      "label",
      { class: "diff-note", title: tl(STRINGS.follow) },
      h("input", { type: "checkbox", checked: hooks.follow, onchange: (e: Event) => hooks.setFollow((e.target as HTMLInputElement).checked) }),
      ` ${tl(STRINGS.follow)}`,
    ),
    diff ? h("button", { class: "icon-btn", title: tl(STRINGS.open), onclick: () => hooks.open(diff.path) }, svg(ICONS.arrowUpRight, 8)) : null,
  );

  const code = h("div", { class: "diff-lines" });
  if (diff) {
    // An edit's diff counts lines inside the edit, not in the file: find the new text in the file first.
    const added = diff.hunks.flatMap((x) => x.lines).filter((l) => l.kind === "added" && l.newLine > 0);
    const first = added[0]?.newLine ?? 1;
    let offset = 0;
    const locate = async () => {
      // debt: a page of 60 lines at a time, the first 600 lines of the file; past that the diff's own line numbers stand.
      for (let at = 1; at <= 600 && added.length; at += 40) {
        const page = await hooks.lines(diff.path, at, 60);
        const i = page?.findIndex((l, k) => l === added[0].text && added.slice(1, 3).every((a, j) => page[k + 1 + j] === undefined || page[k + 1 + j] === a.text)) ?? -1;
        if (i >= 0) return at + i - first;
        if (!page || page.length < 60) break;
      }
      return 0;
    };
    const changed = () => new Set(added.map((l) => l.newLine + offset));
    void locate().then(async (o) => {
      offset = o;
      const from = Math.max(1, firstChanged(diff) + offset - 3);
      const lines = await hooks.lines(diff.path, from, SHOWN);
      if (!lines) {
        // debt: outside the folder or unreadable, the diff's own lines stand in.
        const own = diff.hunks.flatMap((x) => x.lines).slice(0, SHOWN);
        code.replaceChildren(...own.map((l) => h("div", { class: `diff-line ${l.kind}` }, h("span", { class: "sym", text: l.kind === "added" ? "+" : l.kind === "removed" ? "−" : " " }), h("span", { class: "txt", text: l.text }))));
        return;
      }
      code.replaceChildren(...lines.map((text, i) =>
        h("div", { class: `diff-line ${changed().has(from + i) ? "added" : ""}` }, h("span", { class: "sym", text: String(from + i) }), h("span", { class: "txt", text }))));
    });
  } else {
    code.append(h("div", { class: "diff-note", text: tl(STRINGS.nothing) }));
  }

  return h(
    "div",
    { class: "diff-card" },
    head,
    code,
    command ? h("div", { class: "diff-note", title: command }, h("b", { text: "› " }), command.split("\n")[0].slice(0, 120)) : null,
  );
}
