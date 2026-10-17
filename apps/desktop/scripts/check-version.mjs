import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";

export function workspaceVersion(source) {
  const section = source.match(
    /^\[workspace\.package\]\s*\n([\s\S]*?)(?=^\[|(?![\s\S]))/m,
  )?.[1];
  const version = section?.match(/^version\s*=\s*"([^"]+)"/m)?.[1];
  if (!version) throw new Error("workspace.package.version is missing");
  return version;
}
export function checkVersion() {
  const app = resolve(dirname(fileURLToPath(import.meta.url)), "..");
  const version = workspaceVersion(
    readFileSync(resolve(app, "../../Cargo.toml"), "utf8"),
  );
  const sources = [
    [
      "package.json",
      JSON.parse(readFileSync(resolve(app, "package.json"), "utf8")).version,
    ],
    [
      "tauri.conf.json",
      JSON.parse(
        readFileSync(resolve(app, "src-tauri/tauri.conf.json"), "utf8"),
      ).version,
    ],
    [
      "desktop Cargo.toml",
      readFileSync(resolve(app, "src-tauri/Cargo.toml"), "utf8").match(
        /^version\s*=\s*"([^"]+)"/m,
      )?.[1],
    ],
  ];
  for (const [name, actual] of sources) {
    if (actual !== version)
      throw new Error(
        `${name} has version ${actual}; update it to workspace version ${version} before packaging`,
      );
  }
  return version;
}
if (
  process.argv[1] &&
  resolve(process.argv[1]) === fileURLToPath(import.meta.url)
) {
  console.log(`Desktop and engine version: ${checkVersion()}`);
}
