// Builds the release `qq` engine and stages it as a Tauri sidecar:
// src-tauri/binaries/qq-<host-triple>[.exe]. The installer places it beside
// the app executable as plain `qq.exe`, the first place the app looks.
import { execFileSync } from "node:child_process";
import { copyFileSync, mkdirSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const app = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const root = resolve(app, "..", "..");
const ext = process.platform === "win32" ? ".exe" : "";

const host = execFileSync("rustc", ["-vV"], { cwd: root, encoding: "utf8" })
  .split("\n")
  .find((line) => line.startsWith("host:"))
  ?.slice("host:".length)
  .trim();
if (!host) throw new Error("could not read the host target triple from `rustc -vV`");

execFileSync("cargo", ["build", "--release", "--locked", "--bin", "qq"], {
  cwd: root,
  stdio: "inherit",
});

const target = join(app, "src-tauri", "binaries", `qq-${host}${ext}`);
mkdirSync(dirname(target), { recursive: true });
copyFileSync(join(root, "target", "release", `qq${ext}`), target);
console.log(`staged engine sidecar ${target}`);
