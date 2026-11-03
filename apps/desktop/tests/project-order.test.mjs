import { test } from "node:test";
import assert from "node:assert/strict";
import {
  dragLayout,
  moveGroup,
  parseProjectOrder,
  sortProjects,
} from "../src/lib/project-order.ts";

const project = (path, name, main) => ({
  id: path,
  path,
  name,
  worktree: main
    ? { branch: name, main_path: main, main_name: main.split("/").pop() }
    : null,
});

const qq = project("C:/qq", "qq");
const qqFix = project("C:/wt/fix", "fix", "C:/qq");
const app = project("C:/app", "app");
const zed = project("C:/zed", "zed");

test("projects sort by name until the user orders them", () => {
  const names = (list) => list.map((p) => p.path);
  assert.deepEqual(names(sortProjects([zed, qqFix, qq, app], [])), [
    "C:/app",
    "C:/qq",
    "C:/wt/fix",
    "C:/zed",
  ]);
  assert.deepEqual(names(sortProjects([zed, qqFix, qq, app], ["C:/zed", "C:/qq"])), [
    "C:/zed",
    "C:/qq",
    "C:/wt/fix",
    "C:/app",
  ]);
});

test("moving a group keeps the order of closed projects", () => {
  const shown = ["C:/app", "C:/qq", "C:/zed"];
  assert.deepEqual(moveGroup(["C:/gone"], shown, "C:/zed", 0), [
    "C:/zed",
    "C:/app",
    "C:/qq",
    "C:/gone",
  ]);
  assert.deepEqual(moveGroup([], shown, "C:/app", 9), ["C:/qq", "C:/zed", "C:/app"]);
});

test("a malformed saved order is ignored", () => {
  assert.deepEqual(parseProjectOrder("bad json"), []);
  assert.deepEqual(parseProjectOrder('{"a":1}'), []);
  assert.deepEqual(parseProjectOrder('["C:/a", 3, "C:/a", "C:/b"]'), ["C:/a", "C:/b"]);
});

test("dragging opens a slot and stays inside the list", () => {
  const boxes = [
    { key: "a", top: 0, bottom: 40 },
    { key: "b", top: 44, bottom: 84 },
    { key: "c", top: 88, bottom: 128 },
  ];
  assert.deepEqual(dragLayout(boxes, "a", 10, 4), {
    to: 0,
    slot: 0,
    offsets: { a: 10, b: 0, c: 0 },
  });
  assert.deepEqual(dragLayout(boxes, "a", 30, 4), {
    to: 1,
    slot: 44,
    offsets: { a: 30, b: -44, c: 0 },
  });
  assert.deepEqual(dragLayout(boxes, "c", -500, 4), {
    to: 0,
    slot: -88,
    offsets: { a: 44, b: 44, c: -88 },
  });
});

test("an expanded group can be dragged below a short last one", () => {
  const boxes = [
    { key: "tall", top: 0, bottom: 200 },
    { key: "short", top: 204, bottom: 244 },
  ];
  assert.deepEqual(dragLayout(boxes, "tall", 500, 4), {
    to: 1,
    slot: 44,
    offsets: { tall: 44, short: -204 },
  });
});
