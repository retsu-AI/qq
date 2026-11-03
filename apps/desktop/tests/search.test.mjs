import { test } from "node:test";
import assert from "node:assert/strict";
import { highlight, match, rank } from "../src/lib/search.ts";

test("every term must match, in the label or its detail", () => {
  assert.deepEqual(match("auth flow", "Review the auth flow").ranges, [
    [11, 15],
    [16, 20],
  ]);
  assert.equal(match("auth gateway", "Review the auth flow", "api-gateway").ranges.length, 1);
  assert.equal(match("auth billing", "Review the auth flow"), null);
  // Case-insensitive, and the detail (recent output, project name) counts.
  assert.notEqual(match("RELEASE tests", "Release fix", "Tests passed"), null);
  assert.deepEqual(match("  ", "Anything"), { score: 0, ranges: [] });
});

test("label starts outrank word starts, which outrank substrings and details", () => {
  const items = [
    { label: "Unfixable flake", detail: "" },
    { label: "Notes", detail: "fix later" },
    { label: "Please fix the build", detail: "" },
    { label: "Fix flaky retry test", detail: "" },
  ];
  assert.deepEqual(
    rank(items, "fix", (item) => item).map(({ item }) => item.label),
    ["Fix flaky retry test", "Please fix the build", "Unfixable flake", "Notes"],
  );
});

test("an empty query orders by recency, then original order", () => {
  const items = [
    { label: "old", recency: 1 },
    { label: "new", recency: 3 },
    { label: "tie-a", recency: 2 },
    { label: "tie-b", recency: 2 },
  ];
  assert.deepEqual(
    rank(items, "", (item) => item, 3).map(({ item }) => item.label),
    ["new", "tie-a", "tie-b"],
  );
});

test("highlight splits a label into plain and matched runs", () => {
  assert.deepEqual(highlight("Fix the fixture", match("fix", "Fix the fixture").ranges), [
    { text: "Fix", hit: true },
    { text: " the fixture", hit: false },
  ]);
  // Overlapping terms merge into one run.
  assert.deepEqual(highlight("readme", match("read eadm", "readme").ranges), [
    { text: "readm", hit: true },
    { text: "e", hit: false },
  ]);
});
