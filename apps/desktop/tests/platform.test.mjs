import { test } from "node:test";
import assert from "node:assert/strict";
import { nativePlatform, platformFor } from "../src/lib/platform.ts";

test("a browser preview is desktop without asking the native shell", async () => {
  const ask = () => assert.fail("must not ask outside Tauri");
  assert.deepEqual(await nativePlatform(false, ask), { name: "desktop" });
});

test("the native shell's identity is used as reported", async () => {
  assert.deepEqual(await nativePlatform(true, async () => "android"), { name: "android" });
});

test("an unreadable native identity is an error, never desktop", async () => {
  const failed = await nativePlatform(true, async () => {
    throw new Error("command client_platform not found");
  });
  assert.match(failed.error, /command client_platform not found/);
  assert.equal(failed.name, undefined);

  const unknown = await nativePlatform(true, async () => "windows-phone");
  assert.match(unknown.error, /unknown client platform "windows-phone"/);
});

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
