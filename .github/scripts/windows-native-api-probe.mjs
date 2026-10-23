// Temporary behavioral probe for Microsoft's native process-security API.
// This is diagnostic code, not a QQ runtime dependency or a backend selection.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { run } from '@microsoft/mxc-sdk/v1';

assert.equal(process.platform, 'win32');
const role = spawnSync(path.join(process.env.SystemRoot, 'System32', 'WindowsPowerShell', 'v1.0', 'powershell.exe'), [
  '-NoProfile', '-NonInteractive', '-Command',
  '([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)',
], { encoding: 'utf8', timeout: 10_000 });
assert.equal(role.status, 0, role.stderr);
console.log(JSON.stringify({ phase: 'host-role', administrator: role.stdout.trim() }));
assert.equal(role.stdout.trim(), 'False', 'native API must work without an administrator token');

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'qq-native-api-'));
const workspace = path.join(root, 'task');
const home = path.join(workspace, 'home');
fs.mkdirSync(path.join(home, 'tmp'), { recursive: true });
fs.mkdirSync(path.join(home, 'AppData', 'Roaming'), { recursive: true });
fs.mkdirSync(path.join(home, 'AppData', 'Local'), { recursive: true });
const secret = path.join(root, 'outside-canary.txt');
fs.writeFileSync(secret, 'outside-canary');
function installed(name) {
  const found = process.env.PATH.split(path.delimiter)
    .filter(directory => path.isAbsolute(directory))
    .map(directory => path.join(directory, `${name}.exe`))
    .find(candidate => fs.existsSync(candidate));
  assert.ok(found, `installed ${name} is required`);
  return fs.realpathSync.native(found);
}
const tools = Object.fromEntries(['git', 'node', 'cargo'].map(name => [name, installed(name)]));
const rustup = process.env.RUSTUP_HOME ?? path.join(os.homedir(), '.rustup');
const environment = {
  PATH: [...new Set(Object.values(tools).map(file => path.dirname(file))), path.join(process.env.SystemRoot, 'System32')].join(path.delimiter),
  SystemRoot: process.env.SystemRoot,
  PATHEXT: '.COM;.EXE;.BAT;.CMD',
  HOME: home, USERPROFILE: home,
  TEMP: path.join(home, 'tmp'), TMP: path.join(home, 'tmp'),
  APPDATA: path.join(home, 'AppData', 'Roaming'),
  LOCALAPPDATA: path.join(home, 'AppData', 'Local'),
  CARGO_HOME: path.join(home, '.cargo'), RUSTUP_HOME: rustup,
};
if (process.env.RUSTUP_TOOLCHAIN) environment.RUSTUP_TOOLCHAIN = process.env.RUSTUP_TOOLCHAIN;
const expected = Object.fromEntries(Object.entries(tools).map(([name, executable]) => {
  const output = spawnSync(executable, ['--version'], { cwd: workspace, env: environment, encoding: 'utf8', timeout: 10_000 });
  assert.equal(output.status, 0, `host ${name}: ${output.error ?? output.stderr}`);
  return [name, output.stdout.trim()];
}));
let requests = 0;
const server = http.createServer((_request, response) => { requests++; response.end('loopback-canary'); });
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const endpoint = `http://127.0.0.1:${server.address().port}`;
const rustTask = path.join(workspace, 'rust-task');
fs.mkdirSync(path.join(rustTask, 'src'), { recursive: true });
fs.writeFileSync(path.join(rustTask, 'Cargo.toml'), '[package]\nname="qq_native_api_probe"\nversion="0.1.0"\nedition="2024"\n');
fs.writeFileSync(path.join(rustTask, 'src', 'main.rs'), 'fn main() { println!("private rust artifact"); }\n');
const payload = path.join(workspace, 'payload.cjs');
fs.writeFileSync(path.join(workspace, 'input.json'), JSON.stringify({ secret, endpoint }));
fs.writeFileSync(payload, `
const fs = require('node:fs');
const { spawnSync } = require('node:child_process');
(async () => {
  const input = JSON.parse(fs.readFileSync('input.json', 'utf8'));
  const report = { tools: {} };
  for (const name of ['git', 'node', 'cargo']) {
    const output = spawnSync(name, ['--version'], { encoding: 'utf8', timeout: 5000 });
    report.tools[name] = { exit: output.status, stdout: output.stdout, stderr: output.stderr, error: output.error?.message };
  }
  const rust = spawnSync('cargo', ['check', '--offline', '--manifest-path', 'rust-task/Cargo.toml'], { encoding: 'utf8', timeout: 30000 });
  report.rustCheck = { exit: rust.status, stdout: rust.stdout, stderr: rust.stderr, error: rust.error?.message };
  try { fs.readFileSync(input.secret); report.secretRead = true; } catch { report.secretRead = false; }
  try { fs.writeFileSync(input.secret, 'changed'); report.secretWrite = true; } catch { report.secretWrite = false; }
  try { await fetch(input.endpoint, { signal: AbortSignal.timeout(2500) }); report.loopback = true; } catch { report.loopback = false; }
  fs.writeFileSync('artifact.txt', 'native-api-output');
  fs.writeFileSync('report.json', JSON.stringify(report));
})().catch(error => { console.error(error.message); process.exitCode = 1; });
`);
const readonlyPaths = [...new Set([
  process.env.SystemRoot,
  ...Object.values(tools).map(file => path.dirname(file)),
  // Git's bin launcher needs the rest of its installation. Cargo's proxy
  // needs Rustup's installed toolchains, but not Cargo's credential directory.
  path.dirname(path.dirname(tools.git)),
  rustup,
])];
const ancestorPaths = new Set();
for (const granted of [...readonlyPaths, workspace]) {
  let current = granted;
  for (;;) {
    const parent = path.dirname(current);
    if (parent === current) break;
    ancestorPaths.add(parent);
    current = parent;
  }
}
const quote = value => { assert.ok(!/["\r\n]/.test(value)); return `"${value}"`; };
const copiedNode = path.join(workspace, 'node-control.exe');
fs.copyFileSync(tools.node, copiedNode);
const payloadCommand = executable => `${quote(executable)} ${quote(payload)}`;
const baseUi = { disable: false, clipboard: 'none', allowInputInjection: false };
const namespacedEnvironment = Object.fromEntries(Object.entries(environment).map(([name, value]) =>
  [name, ['HOME', 'USERPROFILE', 'TEMP', 'TMP', 'APPDATA', 'LOCALAPPDATA', 'CARGO_HOME', 'RUSTUP_HOME'].includes(name)
    ? path.toNamespacedPath(value) : value]));
const qqEnvironment = { ...namespacedEnvironment, PATH: process.env.PATH };
delete qqEnvironment.PATHEXT;
const cmd = path.join(process.env.SystemRoot, 'System32', 'cmd.exe');
const cases = [
  { name: 'baseline-short-paths', command: payloadCommand(tools.node), payload: true },
  { name: 'namespaced-environment', command: payloadCommand(tools.node), payload: true, environment: namespacedEnvironment },
  { name: 'namespaced-grants', command: payloadCommand(tools.node), payload: true, namespacedGrants: true },
  { name: 'qq-environment-and-grants', command: payloadCommand(tools.node), payload: true, environment: qqEnvironment, namespacedGrants: true },
  { name: 'qq-cmd-environment-and-grants', command: `${quote(cmd)} /d /s /c "${payloadCommand(tools.node)}"`, payload: true, environment: qqEnvironment, namespacedGrants: true },
];
const outcomes = [];
try {
  for (const test of cases) {
    for (const name of ['report.json', 'artifact.txt']) fs.rmSync(path.join(workspace, name), { force: true });
    fs.rmSync(path.join(rustTask, 'target'), { recursive: true, force: true });
    const started = performance.now();
    let output;
    try {
      output = await run({
        containment: { type: 'processcontainer', config: {
          capabilities: ['registryRead'],
          filesystem: { enumeratePaths: [...ancestorPaths].map(value => test.namespacedGrants ? path.toNamespacedPath(value) : value) },
        } },
        command: test.command,
        workingDirectory: workspace,
        inheritDefaultEnvironment: false,
        environment: test.environment ?? environment,
        filesystem: {
          readonlyPaths: readonlyPaths.map(value => test.namespacedGrants ? path.toNamespacedPath(value) : value),
          readwritePaths: [test.namespacedGrants ? path.toNamespacedPath(workspace) : workspace],
        },
        network: { egress: { default: 'deny' }, ingress: { default: 'deny', hostLoopback: 'deny' } },
        ui: baseUi,
        timeoutMs: 60_000,
      });
    } catch (error) {
      outcomes.push({ name: test.name, error: error.message });
      console.log(JSON.stringify(outcomes.at(-1)));
      assert.equal(requests, 0);
      assert.equal(fs.readFileSync(secret, 'utf8'), 'outside-canary');
      continue;
    }
    const outcome = { name: test.name, elapsedMs: performance.now() - started, ...output };
    const reportPath = path.join(workspace, 'report.json');
    if (test.payload && fs.existsSync(reportPath)) {
      const report = JSON.parse(fs.readFileSync(reportPath, 'utf8'));
      outcome.report = report;
      outcome.qualified = output.exitCode === 0 && !output.timedOut && ['git', 'node', 'cargo'].every(name =>
        report.tools[name].exit === 0 && report.tools[name].stdout.trim() === expected[name])
        && report.rustCheck.exit === 0
        && report.secretRead === false && report.secretWrite === false && report.loopback === false
        && fs.existsSync(path.join(workspace, 'artifact.txt'))
        && fs.readFileSync(path.join(workspace, 'artifact.txt'), 'utf8') === 'native-api-output';
    }
    outcomes.push(outcome);
    console.log(JSON.stringify(outcome));
    // These remain unconditional safety assertions, including failed startups.
    assert.equal(requests, 0);
    assert.equal(fs.readFileSync(secret, 'utf8'), 'outside-canary');
  }
  assert.equal(requests, 0);
  assert.equal(fs.readFileSync(secret, 'utf8'), 'outside-canary');
  assert.ok(outcomes.some(outcome => outcome.qualified), 'No native API candidate completed the tool/denial fixture');
} finally {
  server.closeAllConnections();
  await new Promise(resolve => server.close(resolve));
  fs.rmSync(root, { recursive: true, force: true });
}
