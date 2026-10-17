import { test } from "node:test";
import assert from "node:assert/strict";
import { checkVersion, workspaceVersion } from "../scripts/check-version.mjs";
test("release metadata matches the engine workspace version", () =>
  assert.match(checkVersion(), /^\d+\.\d+\.\d+/));
test("workspace version comes from the package section rather than a dependency", () => {
  assert.equal(
    workspaceVersion(
      '[package]\nversion="9"\n[workspace.package]\n# release metadata\nversion = "1.2.3"\n[dependencies]\nx="4"',
    ),
    "1.2.3",
  );
  assert.throws(
    () => workspaceVersion('[dependencies]\nversion="4"'),
    /missing/,
  );
});
