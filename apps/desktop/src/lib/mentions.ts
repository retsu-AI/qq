// `@` completion in the composer. The grammar is qq-protocol's mentions.rs:
// a mention starts at message start or after whitespace, `(` or `[`, and ends
// at whitespace, `)`, `]`, `,` or `;`. `@@` is a literal `@`, and fenced
// code is never scanned. Resolution happens in the native core on send.

export interface ActiveMention {
  /** Index of the `@`. */
  start: number;
  /** Text typed after the `@`, up to the caret. */
  query: string;
}

export interface Suggestion {
  /** Inserted after the `@`. */
  value: string;
  kind: "file" | "dir" | "special";
  hint?: string;
}

/** References that are not paths, offered when their prefix is typed. */
export const SPECIAL_MENTIONS: Suggestion[] = [
  { value: "diff", kind: "special", hint: "Working tree changes" },
  { value: "web:", kind: "special", hint: "Fetch a URL" },
  { value: "sha:", kind: "special", hint: "A commit" },
];

const TOKEN = /(?:^|[\s([])@([^\s()[\],;@]*)$/;

/** The mention being typed at `caret`, or null when the caret is not in one. */
export function findMention(text: string, caret: number): ActiveMention | null {
  const before = text.slice(0, caret);
  // Inside an open fenced block the grammar does not apply.
  if ((before.match(/```/g)?.length ?? 0) % 2 === 1) return null;
  const match = TOKEN.exec(before);
  if (!match) return null;
  const query = match[1];
  const start = caret - query.length - 1;
  // `@@` is an escaped literal.
  if (before[start - 1] === "@") return null;
  return { start, query };
}

export function specialSuggestions(query: string): Suggestion[] {
  if (query.includes("/")) return [];
  const lower = query.toLowerCase();
  return SPECIAL_MENTIONS.filter(
    (special) =>
      special.value.startsWith(lower) && special.value !== lower,
  );
}

/**
 * Replaces the active mention with `value`. Directories and `web:`-style
 * prefixes keep the caret inside the mention so completion can continue;
 * anything else is finished with a space.
 */
export function insertMention(
  text: string,
  caret: number,
  mention: ActiveMention,
  value: string,
): { text: string; caret: number } {
  const open = value.endsWith("/") || value.endsWith(":");
  const after = text.slice(caret);
  const separator = open || /^\s/.test(after) ? "" : " ";
  const inserted = `@${value}${separator}`;
  const next = text.slice(0, mention.start) + inserted + after;
  const nextCaret = mention.start + inserted.length + (!open && separator === "" && after ? 1 : 0);
  return { text: next, caret: Math.min(nextCaret, next.length) };
}
