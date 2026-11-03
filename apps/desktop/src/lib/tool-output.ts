// Structure in a tool's text result, for display. QQ's tools start their
// result with one header line (`read src/a.rs L1-40/120 h:… truncated=bytes`,
// `shell exit=0 elapsed=1.2`), then a body whose shape depends on the tool.

export interface OutputHeader {
  tool: string;
  subject: string;
  /** `key=value` fields and the line range, in order; hashes are dropped. */
  fields: Array<[string, string]>;
}

export type Tone = "error" | "warn" | "ok";

export type OutputLine =
  /** A numbered file line from `read`. */
  | { kind: "code"; number: string; text: string }
  /** A matching or context line from `search`. */
  | { kind: "match"; number: string; text: string }
  /** A file heading from `search`. */
  | { kind: "path"; text: string }
  /** `--` between non-adjacent ranges. */
  | { kind: "gap" }
  /** QQ's own notes, such as `…[qq: 4 more lines]…`. */
  | { kind: "note"; text: string }
  | { kind: "text"; text: string; tone: Tone | null };

export interface ToolOutput {
  header: OutputHeader | null;
  lines: OutputLine[];
}

const HEADER_TOOLS = new Set([
  "read",
  "search",
  "tree",
  "shell",
  "write",
  "edit",
  "fetch",
]);

function parseHeader(line: string): OutputHeader | null {
  const tokens = line.match(/"(?:[^"\\]|\\.)*"|\S+/g) ?? [];
  const tool = tokens[0];
  if (!tool || !HEADER_TOOLS.has(tool)) return null;
  const subject: string[] = [];
  const fields: Array<[string, string]> = [];
  for (const token of tokens.slice(1)) {
    const field = /^([a-z_]+)=(.*)$/.exec(token);
    if (field) fields.push([field[1], field[2]]);
    else if (/^h:[0-9a-f]+$/.test(token)) continue;
    else if (/^L[\d,-]+(\/\d+)?$/.test(token)) fields.push(["lines", token.slice(1)]);
    else subject.push(token);
  }
  return { tool, subject: subject.join(" "), fields };
}

/** A line's tone in command output, from words that usually mean it. */
export function toneOf(line: string): Tone | null {
  // "0 errors" and "0 failed" are good news.
  const bad = line.replace(/\b0 (errors?|failed|failures?)\b/gi, "");
  if (/\b(error|errors|failed|failure|fatal|panicked)\b|✗|✘/i.test(bad))
    return "error";
  if (/\bwarn(ing|ings)?\b/i.test(line)) return "warn";
  if (/\b(passed|success|succeeded|finished)\b|\bok\b|✓|✔/i.test(line))
    return "ok";
  return null;
}

const NOTE = /^…\[qq:.*\]…$/;

export function parseToolOutput(text: string): ToolOutput {
  const raw = text.replace(/\n$/, "").split("\n");
  const header = raw.length > 0 ? parseHeader(raw[0]) : null;
  const body = header ? raw.slice(1) : raw;
  const lines: OutputLine[] = body.map((line) => {
    if (NOTE.test(line.trim())) return { kind: "note", text: line.trim() };
    if (header?.tool === "read") {
      const code = /^\s*(\d+)\t(.*)$/.exec(line);
      if (code) return { kind: "code", number: code[1], text: code[2] };
      if (line === "--") return { kind: "gap" };
    }
    if (header?.tool === "search") {
      const match = /^L(\d+)[:-] ?(.*)$/.exec(line);
      if (match) return { kind: "match", number: match[1], text: match[2] };
      if (line === "--") return { kind: "gap" };
      if (line.trim() && !/^\s/.test(line)) return { kind: "path", text: line };
    }
    return { kind: "text", text: line, tone: toneOf(line) };
  });
  return { header, lines };
}
