// The file edits of agents other than Claude Code, as live diffs: Codex's
// `apply_patch` (one patch, several files), Antigravity's file tools and Kiro's
// `fs_write`. They are turned into the same FileDiff Claude Code's Edit, MultiEdit
// and Write give (core/diff.ts), so the ticker step and the diff card are the same.
// Nothing is applied or run: the patch text is only read.

import { buildFileDiff, fromEdit, fromNew, type FileDiff } from "../core/diff";

const str = (v: unknown): string | null => (typeof v === "string" ? v : null);

/** A relative path made absolute against the session's folder, so the diff's ↗ can open it. */
function resolvePath(path: string | null, cwd: string): string | null {
  if (!path) return null;
  if (/^(?:[A-Za-z]:[\\/]|[\\/]{2}|\/)/.test(path) || !cwd) return path;
  const sep = cwd.includes("\\") ? "\\" : "/";
  return `${cwd.replace(/[\\/]+$/, "")}${sep}${path.replace(/^(?:\.[\\/])+/, "").replace(/[\\/]+/g, sep)}`;
}

/** The changes of one file, joined: each piece is a diff of its own, as in a MultiEdit. */
function joined(path: string, parts: FileDiff[], isNewFile = false): FileDiff | null {
  const added = parts.reduce((n, d) => n + d.added, 0);
  const removed = parts.reduce((n, d) => n + d.removed, 0);
  if (added === 0 && removed === 0) return null;
  return { id: 0, path, added, removed, hunks: parts.flatMap((d) => d.hunks), tooLarge: parts.some((d) => d.tooLarge), isNewFile };
}

/** Codex's `apply_patch` body: `*** Update File: path`, then `@@` hunks of ` `, `-` and `+` lines. */
export function patchDiffs(patch: string, cwd = ""): FileDiff[] {
  const out: FileDiff[] = [];
  let path: string | null = null;
  let add = false;
  let parts: FileDiff[] = [];
  let before: string[] = [];
  let after: string[] = [];
  const hunk = () => {
    if (path && (before.length || after.length)) {
      parts.push(add ? fromNew(after.join("\n"), path) : fromEdit(before.join("\n"), after.join("\n"), path));
    }
    before = [];
    after = [];
  };
  const file = () => {
    hunk();
    const d = path ? joined(path, parts, add) : null;
    if (d) out.push(d);
    parts = [];
  };
  for (const line of patch.replace(/\r\n/g, "\n").split("\n")) {
    const head = line.match(/^\*\*\* (Update|Add|Delete) File: (.+)$/);
    if (head) {
      file();
      // A deleted file lists nothing to show.
      path = head[1] === "Delete" ? null : resolvePath(head[2].trim(), cwd);
      add = head[1] === "Add";
      continue;
    }
    const move = line.match(/^\*\*\* Move to: (.+)$/);
    if (move && path) {
      path = resolvePath(move[1].trim(), cwd);
      continue;
    }
    if (!path || line.startsWith("***")) continue;
    if (line.startsWith("@@")) hunk();
    else if (line.startsWith("+")) after.push(line.slice(1));
    else if (line.startsWith("-")) before.push(line.slice(1));
    else if (line.startsWith(" ")) {
      before.push(line.slice(1));
      after.push(line.slice(1));
    }
  }
  file();
  return out;
}

/** The diffs of one finished tool call: none when it is not a file edit we can read. */
export function buildFileDiffs(tool: string, input: Record<string, unknown>, cwd = ""): FileDiff[] {
  const target = resolvePath(str(input.TargetFile) ?? str(input.file_path) ?? str(input.path), cwd);
  const one = (d: FileDiff | null) => (d && (d.added > 0 || d.removed > 0) ? [d] : []);
  switch (tool) {
    case "apply_patch":
      return patchDiffs(str(input.command) ?? str(input.input) ?? "", cwd);
    case "write_to_file": {
      const content = str(input.CodeContent);
      return target && content ? one(fromNew(content, target)) : [];
    }
    case "replace_file_content": {
      const old = str(input.TargetContent);
      const next = str(input.ReplacementContent);
      return target && old != null && next != null ? one(fromEdit(old, next, target)) : [];
    }
    case "multi_replace_file_content": {
      const chunks = Array.isArray(input.ReplacementChunks) ? (input.ReplacementChunks as Record<string, unknown>[]) : [];
      if (!target) return [];
      const parts = chunks.flatMap((c) => (str(c?.TargetContent) != null && str(c?.ReplacementContent) != null ? [fromEdit(str(c.TargetContent)!, str(c.ReplacementContent)!, target)] : []));
      return one(joined(target, parts));
    }
    // Kiro CLI: one tool, the operation in `command`, the file in `path`.
    case "fs_write":
    case "write": {
      const file = target;
      if (!file) return [];
      switch (input.command) {
        case "create":
          return str(input.file_text) ? one(fromNew(str(input.file_text)!, file)) : [];
        case "str_replace":
          return one(fromEdit(str(input.old_str) ?? "", str(input.new_str) ?? "", file));
        case "insert":
        case "append":
          return one(fromEdit("", str(input.new_str) ?? "", file));
        default:
          return [];
      }
    }
    default: {
      const d = buildFileDiff(tool, input);
      return d ? [{ ...d, path: resolvePath(d.path, cwd) ?? d.path }] : [];
    }
  }
}
