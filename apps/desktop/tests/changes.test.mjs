import { test } from "node:test";
import assert from "node:assert/strict";
import { collectChanges, diffStats, parseDiff } from "../src/lib/changes.ts";
import { summarizeTools } from "../src/lib/tools.ts";

const DIFF = [
  "--- a/src/a.rs",
  "+++ b/src/a.rs",
  "@@ -10,3 +10,4 @@ fn main() {",
  " keep",
  "-old",
  "+new",
  "+--flag",
  "---gone",
  " tail",
  "\\ No newline at end of file",
].join("\n");

test("diff lines carry old and new line numbers from the hunk header", () => {
  const lines = parseDiff(DIFF);
  assert.deepEqual(
    lines.map((line) => [line.kind, line.old ?? null, line.new ?? null]),
    [
      ["hunk", null, null],
      ["ctx", 10, 10],
      ["del", 11, null],
      ["add", null, 11],
      ["add", null, 12],
      ["del", 12, null],
      ["ctx", 13, 13],
      ["note", null, null],
    ],
  );
  // Inside a hunk, `+--flag` and `---gone` are content, not file headers.
  assert.equal(lines[4].text, "--flag");
  assert.equal(lines[5].text, "--gone");
  assert.deepEqual(diffStats(DIFF), { added: 2, removed: 2 });
});

const tool = (id, path, extra = {}) => ({
  kind: "tool",
  id,
  run_id: "r",
  name: "edit_file",
  arguments: "{}",
  state: "completed",
  result: null,
  is_error: false,
  diff: { path, diff: DIFF },
  live_output: null,
  duration_ms: null,
  settled: null,
  approval: null,
  child_session: null,
  ...extra,
});

test("changes group edits by file and skip failed or unfinished edits", () => {
  const changes = collectChanges([
    tool("1", "src/a.rs"),
    tool("2", "src/b.rs"),
    tool("3", "src/a.rs"),
    tool("4", "src/c.rs", { is_error: true }),
    tool("5", "src/d.rs", { state: "running" }),
    { kind: "user", id: "u", text: "hi", state: "complete", steering: false },
  ]);
  assert.deepEqual(
    changes.map((file) => [file.path, file.diffs.length, file.added, file.removed]),
    [
      ["src/a.rs", 2, 4, 4],
      ["src/b.rs", 1, 2, 2],
    ],
  );
});

test("tool runs summarize by activity in first-seen order", () => {
  assert.equal(
    summarizeTools(["shell", "read_file", "shell", "read_file", "shell"]),
    "Ran 3 commands, read 2 files",
  );
  assert.equal(summarizeTools(["search"]), "Searched 1 pattern");
  assert.equal(
    summarizeTools(["mcp_thing", "tree"]),
    "Used 1 tool, listed 1 folder",
  );
});
