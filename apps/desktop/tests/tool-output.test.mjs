import { test } from "node:test";
import assert from "node:assert/strict";
import { parseToolOutput, toneOf } from "../src/lib/tool-output.ts";

test("read output splits its header, numbered lines, gaps and notes", () => {
  const output = parseToolOutput(
    "read Docs/ROADMAP.md L1-181/323 h:f5c79af7291c truncated=bytes\n" +
      "  1\t# Roadmap\n  2\t\n--\n100\tline 100\n…[qq: 1 more lines; continue from offset=3]…\n",
  );
  assert.deepEqual(output.header, {
    tool: "read",
    subject: "Docs/ROADMAP.md",
    fields: [
      ["lines", "1-181/323"],
      ["truncated", "bytes"],
    ],
  });
  assert.deepEqual(output.lines, [
    { kind: "code", number: "1", text: "# Roadmap" },
    { kind: "code", number: "2", text: "" },
    { kind: "gap" },
    { kind: "code", number: "100", text: "line 100" },
    { kind: "note", text: "…[qq: 1 more lines; continue from offset=3]…" },
  ]);
});

test("search output has file headings and numbered matches", () => {
  const output = parseToolOutput(
    'search "needle" mode=content matches=1/1 files=1 scanned=1\nsrc/needle.rs\nL2: needle here\n',
  );
  assert.equal(output.header?.subject, '"needle"');
  assert.deepEqual(output.header?.fields[0], ["mode", "content"]);
  assert.deepEqual(output.lines, [
    { kind: "path", text: "src/needle.rs" },
    { kind: "match", number: "2", text: "needle here" },
  ]);
});

test("shell output keeps its exit code and tones lines", () => {
  const output = parseToolOutput(
    "shell exit=1 elapsed=2.1\nCompiling retry\nwarning: unused import\nerror[E0425]: cannot find value\ntest result: ok. 3 passed\n",
  );
  assert.deepEqual(output.header?.fields, [
    ["exit", "1"],
    ["elapsed", "2.1"],
  ]);
  assert.deepEqual(
    output.lines.map((line) => (line.kind === "text" ? line.tone : line.kind)),
    [null, "warn", "error", "ok"],
  );
});

test("text without a known header is plain output", () => {
  const output = parseToolOutput("hello world\n");
  assert.equal(output.header, null);
  assert.deepEqual(output.lines, [{ kind: "text", text: "hello world", tone: null }]);
  assert.equal(toneOf("bookkeeping"), null);
  assert.equal(toneOf("0 errors"), null);
  assert.equal(toneOf("test result: ok. 3 passed; 0 failed"), "ok");
  assert.equal(toneOf("2 failed"), "error");
});
