// Files dropped on the composer become `@` mentions. The mention grammar is
// workspace-relative and ends at whitespace, so files outside the project and
// paths with spaces cannot be mentioned and are reported instead.

export interface DropResult {
  /** Workspace-relative paths with `/` separators, ready to follow `@`. */
  mentions: string[];
  skipped: Array<{ path: string; reason: "outside" | "spaces" }>;
}

/** Strips the `\\?\` verbatim prefix and normalizes separators to `/`. */
function normalize(path: string): string {
  return path
    .replace(/^\\\\\?\\UNC\\/i, "\\\\")
    .replace(/^\\\\\?\\/, "")
    .replace(/\\/g, "/")
    .replace(/\/+$/, "");
}

export function dropMentions(projectPath: string, paths: string[]): DropResult {
  const root = normalize(projectPath);
  // Windows paths compare case-insensitively.
  const rootKey = root.toLowerCase() + "/";
  const result: DropResult = { mentions: [], skipped: [] };
  for (const path of paths) {
    const full = normalize(path);
    if (!full.toLowerCase().startsWith(rootKey)) {
      result.skipped.push({ path, reason: "outside" });
      continue;
    }
    const relative = full.slice(rootKey.length);
    if (!relative) {
      result.skipped.push({ path, reason: "outside" });
    } else if (/\s/.test(relative)) {
      result.skipped.push({ path, reason: "spaces" });
    } else if (!result.mentions.includes(relative)) {
      result.mentions.push(relative);
    }
  }
  return result;
}

/** `text` with the mentions inserted at `caret`, separated by spaces. */
export function insertMentions(
  text: string,
  caret: number,
  mentions: string[],
): { text: string; caret: number } {
  if (mentions.length === 0) return { text, caret };
  const before = text.slice(0, caret);
  const after = text.slice(caret);
  const lead = before && !/\s$/.test(before) ? " " : "";
  const trail = after && /^\s/.test(after) ? "" : " ";
  const inserted = lead + mentions.map((m) => `@${m}`).join(" ") + trail;
  return { text: before + inserted + after, caret: caret + inserted.length };
}
