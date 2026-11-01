// Matching and ranking for the search window. Pure, so it is tested alone.

export type SearchTab = "all" | "sessions" | "projects" | "commands";

export const SEARCH_TABS: Array<[SearchTab, string]> = [
  ["all", "All"],
  ["sessions", "Sessions"],
  ["projects", "Projects"],
  ["commands", "Commands"],
];

/** A half-open character range of a label to highlight. */
export type Range = [number, number];

export interface Match {
  score: number;
  ranges: Range[];
}

/**
 * Every whitespace-separated term must appear in `label` or `detail`.
 * Terms in the label score higher, more so at the start of a word and most
 * at the start of the label. Returns null when a term is missing.
 */
export function match(query: string, label: string, detail = ""): Match | null {
  const terms = query.toLocaleLowerCase().trim().split(/\s+/).filter(Boolean);
  if (terms.length === 0) return { score: 0, ranges: [] };
  const text = label.toLocaleLowerCase();
  const extra = detail.toLocaleLowerCase();
  let score = 0;
  const ranges: Range[] = [];
  for (const term of terms) {
    const at = wordStart(text, term);
    if (at >= 0) {
      score += at === 0 ? 30 : 20;
      ranges.push([at, at + term.length]);
      continue;
    }
    const inside = text.indexOf(term);
    if (inside >= 0) {
      score += 10;
      ranges.push([inside, inside + term.length]);
    } else if (extra.includes(term)) score += 1;
    else return null;
  }
  return { score, ranges: merge(ranges) };
}

/** First index where `term` starts a word in `text`, or -1. */
function wordStart(text: string, term: string): number {
  for (let at = text.indexOf(term); at >= 0; at = text.indexOf(term, at + 1)) {
    if (at === 0 || /[^\p{L}\p{N}]/u.test(text[at - 1])) return at;
  }
  return -1;
}

function merge(ranges: Range[]): Range[] {
  const sorted = [...ranges].sort((a, b) => a[0] - b[0]);
  const out: Range[] = [];
  for (const range of sorted) {
    const last = out[out.length - 1];
    if (last && range[0] <= last[1]) last[1] = Math.max(last[1], range[1]);
    else out.push([...range]);
  }
  return out;
}

/** Splits a label into plain and highlighted runs for rendering. */
export function highlight(
  label: string,
  ranges: Range[],
): Array<{ text: string; hit: boolean }> {
  const runs: Array<{ text: string; hit: boolean }> = [];
  let at = 0;
  for (const [start, end] of ranges) {
    if (start > at) runs.push({ text: label.slice(at, start), hit: false });
    runs.push({ text: label.slice(start, end), hit: true });
    at = end;
  }
  if (at < label.length) runs.push({ text: label.slice(at), hit: false });
  return runs;
}

/**
 * Best matches first; with an empty query, `recency` orders them (newest
 * first) and the original order breaks ties.
 */
export function rank<T>(
  items: T[],
  query: string,
  text: (item: T) => { label: string; detail?: string; recency?: number },
  limit = Infinity,
): Array<{ item: T; ranges: Range[] }> {
  const found: Array<{ item: T; ranges: Range[]; score: number; recency: number; index: number }> = [];
  items.forEach((item, index) => {
    const { label, detail, recency = 0 } = text(item);
    const result = match(query, label, detail);
    if (result) found.push({ item, ranges: result.ranges, score: result.score, recency, index });
  });
  found.sort(
    (a, b) => b.score - a.score || b.recency - a.recency || a.index - b.index,
  );
  return found.slice(0, limit).map(({ item, ranges }) => ({ item, ranges }));
}
