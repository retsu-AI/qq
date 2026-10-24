import { test } from "node:test";
import assert from "node:assert/strict";
import { platformFor } from "../src/lib/platform.ts";

test("desktop retains every host capability", () => {
  assert.deepEqual(platformFor("desktop"), {
    name: "desktop",
    mobile: false,
    features: {
      localEngine: true, localProjects: true, git: true, worktrees: true,
      editor: true, fileDrop: true, hostConfiguration: true, nativeNotifications: true,
    },
  });
});

for (const name of ["android", "ios"]) {
  test(`${name} cannot expose host operations or background notifications`, () => {
    assert.deepEqual(platformFor(name), {
      name, mobile: true,
      features: {
        localEngine: false, localProjects: false, git: false, worktrees: false,
        editor: false, fileDrop: false, hostConfiguration: false, nativeNotifications: false,
      },
    });
    assert.ok(Object.isFrozen(platformFor(name).features));
  });
}
