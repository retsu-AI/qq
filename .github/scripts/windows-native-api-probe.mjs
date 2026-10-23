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
const quote = value => { assert.ok(!/["\r\n]/.test(value)); return `"${value}"`; };
try {
  const started = performance.now();
  const output = await run({
    containment: { type: 'processcontainer', config: { capabilities: ['registryRead'] } },
    command: `${quote(tools.node)} ${quote(payload)}`,
    workingDirectory: workspace,
    inheritDefaultEnvironment: false,
    environment,
    filesystem: { readonlyPaths, readwritePaths: [workspace] },
    network: { egress: { default: 'deny' }, ingress: { default: 'deny', hostLoopback: 'deny' } },
    timeoutMs: 15_000,
  });
  console.log(JSON.stringify({ phase: 'native-api-run', elapsedMs: performance.now() - started, ...output }));
  assert.equal(output.exitCode, 0);
  assert.equal(output.timedOut, false);
  const report = JSON.parse(fs.readFileSync(path.join(workspace, 'report.json'), 'utf8'));
  console.log(JSON.stringify({ phase: 'payload-report', report }));
  for (const name of ['git', 'node', 'cargo']) {
    assert.equal(report.tools[name].exit, 0, name);
    assert.equal(report.tools[name].stdout.trim(), expected[name], name);
  }
  assert.equal(report.secretRead, false);
  assert.equal(report.secretWrite, false);
  assert.equal(report.loopback, false);
  assert.equal(requests, 0);
  assert.equal(fs.readFileSync(secret, 'utf8'), 'outside-canary');
  assert.equal(fs.readFileSync(path.join(workspace, 'artifact.txt'), 'utf8'), 'native-api-output');
} finally {
  server.closeAllConnections();
  await new Promise(resolve => server.close(resolve));
  fs.rmSync(root, { recursive: true, force: true });
}
