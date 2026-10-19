// The files a session's file tools changed, from the diffs on its tool calls.
// Edits made by shell commands carry no diff and are not listed.
import type { Item } from "./types";

export interface FileChange {
  path: string;
  /** One unified diff per edit, oldest first. */
  diffs: string[];
  added: number;
  removed: number;
}

export function diffStats(diff: string): { added: number; removed: number } {
  let added = 0;
  let removed = 0;
  for (const line of parseDiff(diff)) {
    if (line.kind === "add") added++;
    else if (line.kind === "del") removed++;
  }
  return { added, removed };
}

/** Changed files in first-edit order. Only completed, successful edits count. */
export function collectChanges(items: Item[]): FileChange[] {
  const files = new Map<string, FileChange>();
  for (const item of items) {
    if (item.kind !== "tool" || !item.diff) continue;
    if (item.state !== "completed" || item.is_error) continue;
    const { path, diff } = item.diff;
    const file = files.get(path) ?? { path, diffs: [], added: 0, removed: 0 };
    const stats = diffStats(diff);
    file.diffs.push(diff);
    file.added += stats.added;
    file.removed += stats.removed;
    files.set(path, file);
  }
  return [...files.values()];
}

export type DiffLine =
  | { kind: "hunk"; text: string }
  | { kind: "note"; text: string }
  | { kind: "add" | "del" | "ctx"; text: string; old: number | null; new: number | null };

/** Diff lines with old/new line numbers from the hunk headers. */
export function parseDiff(diff: string): DiffLine[] {
  const out: DiffLine[] = [];
  let oldLine = 0;
  let newLine = 0;
  let inHunk = false;
  for (const line of diff.replace(/\n$/, "").split("\n")) {
    // File headers precede the first hunk; inside one, `---` is a removed `--`.
    if (!inHunk && (line.startsWith("+++") || line.startsWith("---"))) continue;
    const hunk = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/.exec(line);
    if (hunk) {
      oldLine = Number(hunk[1]);
      newLine = Number(hunk[2]);
      inHunk = true;
      out.push({ kind: "hunk", text: line });
    } else if (line.startsWith("+")) {
      out.push({ kind: "add", text: line.slice(1), old: null, new: newLine++ });
    } else if (line.startsWith("-")) {
      out.push({ kind: "del", text: line.slice(1), old: oldLine++, new: null });
    } else if (line.startsWith(" ") || line === "") {
      out.push({ kind: "ctx", text: line.slice(1), old: oldLine++, new: newLine++ });
    } else {
      // `\ No newline at end of file`, `[diff truncated]` and other notes.
      out.push({ kind: "note", text: line });
    }
  }
  return out;
}
