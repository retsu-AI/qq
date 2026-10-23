// Temporary behavioral probe for Microsoft's native process-security API.
// This is diagnostic code, not a QQ runtime dependency or a backend selection.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { run } from '@microsoft/mxc-sdk/v1';

assert.equal(process.platform, 'win32');
// The C# launcher verifies the exact primary token before creating this process.

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'qq-native-api-'));
const workspace = process.env.QQ_NATIVE_WRITE_SCOPE;
const writeSid = process.env.QQ_NATIVE_WRITE_SID;
assert.ok(workspace && path.isAbsolute(workspace), 'the host must provide the private write scope');
assert.match(writeSid ?? '', /^S-1-5-21-(?:[0-9]+-){3}[0-9]+$/);
const home = path.join(workspace, 'home');
const softwareRoot = path.join(root, 'installed-input');
fs.mkdirSync(softwareRoot);
const softwareFile = path.join(softwareRoot, 'read-only-input.txt');
fs.writeFileSync(softwareFile, 'host-installed-input');
fs.mkdirSync(path.join(home, 'tmp'), { recursive: true });
fs.mkdirSync(path.join(home, 'AppData', 'Roaming'), { recursive: true });
fs.mkdirSync(path.join(home, 'AppData', 'Local'), { recursive: true });
const secret = process.env.QQ_NATIVE_SCOPE_CANARY;
const markerSecret = process.env.QQ_NATIVE_MARKER_CANARY;
const publicWritable = process.env.QQ_NATIVE_PUBLIC_WRITABLE;
assert.ok(publicWritable, 'host must provide a disposable publicly writable file');
assert.equal(fs.readFileSync(publicWritable, 'utf8'), 'public-input');
assert.ok(process.env.QQ_NATIVE_FUTURE_DIRECTORY);
const futureSecret = path.join(process.env.QQ_NATIVE_FUTURE_DIRECTORY, 'created-after-launch.txt');
assert.ok(markerSecret, 'the host must provide a separate marker-only canary');
assert.ok(secret, 'the host must provide a canary with explicit public AppContainer read access');
assert.equal(fs.readFileSync(secret, 'utf8'), 'outside-canary');
assert.equal(fs.readFileSync(markerSecret, 'utf8'), 'outside-marker');
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
  TMPDIR: path.join(home, 'tmp'),
  TEMP: path.join(home, 'tmp'), TMP: path.join(home, 'tmp'),
  APPDATA: path.join(home, 'AppData', 'Roaming'),
  LOCALAPPDATA: path.join(home, 'AppData', 'Local'),
  CARGO_HOME: path.join(home, '.cargo'), RUSTUP_HOME: rustup,
};
if (process.env.RUSTUP_TOOLCHAIN) environment.RUSTUP_TOOLCHAIN = process.env.RUSTUP_TOOLCHAIN;
// Read installation metadata only: discovery never runs a host tool.
function directories(root) {
  let entries;
  try { entries = fs.readdirSync(root, { withFileTypes: true }); }
  catch (error) { if (error.code === 'ENOENT') return []; throw error; }
  assert.ok(entries.length <= 256, 'bounded installation discovery');
  return entries.filter(entry => entry.isDirectory() && !entry.isSymbolicLink())
    .map(entry => entry.name).sort((a, b) => b.localeCompare(a, 'en', { numeric: true }));
}
const programFiles = [...new Set(['ProgramFiles', 'ProgramFiles(x86)', 'ProgramW6432']
  .map(name => process.env[name]).filter(Boolean))];
const architecture = { arm64: 'arm64', x64: 'x64', ia32: 'x86' }[process.arch];
assert.ok(architecture, 'native compiler architecture');
let compiler;
for (const base of programFiles) {
  const installation = path.join(base, 'Microsoft Visual Studio');
  for (const year of directories(installation).filter(name => /^\d+$/.test(name))) {
    for (const edition of ['Enterprise', 'Professional', 'Community', 'BuildTools']) {
      const toolsets = path.join(installation, year, edition, 'VC', 'Tools', 'MSVC');
      for (const version of directories(toolsets).filter(name => /^\d+(\.\d+)+$/.test(name))) {
        const toolset = path.join(toolsets, version);
        const host = process.arch === 'arm64' ? ['Hostarm64', 'Hostx64'] : ['Hostx64', 'Hostx86'];
        const binaries = host.map(name => path.join(toolset, 'bin', name, architecture))
          .find(directory => fs.existsSync(path.join(directory, 'link.exe')));
        if (!binaries || !fs.existsSync(path.join(toolset, 'lib', architecture))) continue;
        compiler ??= { installation, toolset, binaries };
      }
    }
  }
}
assert.ok(compiler, 'an installed native MSVC linker is required');
let sdk;
for (const base of programFiles) {
  const root = path.join(base, 'Windows Kits', '10');
  for (const version of directories(path.join(root, 'Lib')).filter(name => /^\d+(\.\d+)+$/.test(name))) {
    if (['ucrt', 'um'].every(name => fs.existsSync(path.join(root, 'Lib', version, name, architecture)))) {
      sdk ??= { root, version };
    }
  }
}
assert.ok(sdk, 'an installed native Windows SDK is required');
environment.PATH = compiler.binaries + path.delimiter + environment.PATH;
environment.LIB = [path.join(compiler.toolset, 'lib', architecture),
  ...['ucrt', 'um'].map(name => path.join(sdk.root, 'Lib', sdk.version, name, architecture))].join(path.delimiter);
environment.INCLUDE = [path.join(compiler.toolset, 'include'),
  ...['ucrt', 'um', 'shared'].map(name => path.join(sdk.root, 'Include', sdk.version, name))].join(path.delimiter);
console.log(JSON.stringify({ compiler: compiler.binaries, sdk: path.join(sdk.root, sdk.version) }));
const restrictedHelper = path.join(workspace, 'restricted-child.exe');
const helperBuild = spawnSync(path.join(compiler.binaries, 'cl.exe'), [
  '/nologo', '/EHsc', '/W4', '/WX', '/std:c++20', '/MT',
  fileURLToPath(new URL('./windows-restricted-child.cpp', import.meta.url)),
  `/Fe:${restrictedHelper}`, `/Fo:${path.join(workspace, 'restricted-child.obj')}`,
// Host fixture compilation is outside the sandbox timing measurements.
], { cwd: workspace, env: environment, encoding: 'utf8', timeout: 120_000 });
assert.equal(helperBuild.status, 0, `token probe build: ${helperBuild.error ?? ''} ${helperBuild.stdout} ${helperBuild.stderr}`);

const expected = Object.fromEntries(Object.entries(tools).map(([name, executable]) => {
  const output = spawnSync(executable, ['--version'], { cwd: workspace, env: environment, encoding: 'utf8', timeout: 10_000 });
  assert.equal(output.status, 0, `host ${name}: ${output.error ?? output.stderr}`);
  return [name, output.stdout.trim()];
}));
let requests = 0;
function assertHostState() {
  assert.equal(requests, 0);
  assert.equal(fs.readFileSync(secret, 'utf8'), 'outside-canary');
  assert.equal(fs.readFileSync(markerSecret, 'utf8'), 'outside-marker');
  if (fs.existsSync(futureSecret)) assert.equal(fs.readFileSync(futureSecret, 'utf8'), 'future-outside');
  assert.equal(fs.readFileSync(softwareFile, 'utf8'), 'host-installed-input',
    'read-only software must remain immutable through a writable hardlink');
}
const server = http.createServer((_request, response) => { requests++; response.end('loopback-canary'); });
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const endpoint = `http://127.0.0.1:${server.address().port}`;
const rustTask = path.join(workspace, 'rust-task');
fs.mkdirSync(path.join(rustTask, 'src'), { recursive: true });
fs.writeFileSync(path.join(rustTask, 'Cargo.toml'), '[package]\nname="qq_native_api_probe"\nversion="0.1.0"\nedition="2024"\n');
fs.writeFileSync(path.join(rustTask, 'src', 'main.rs'), 'fn main() { println!("private rust artifact"); }\n');
const payload = path.join(workspace, 'payload.cjs');
fs.writeFileSync(path.join(workspace, 'input.json'), JSON.stringify({ secret, markerSecret, futureSecret, publicWritable, endpoint, softwareFile, restrictedHelper }));
fs.writeFileSync(payload, `
const fs = require('node:fs');
const { spawnSync } = require('node:child_process');
const path = require('node:path');
(async () => {
  const input = JSON.parse(fs.readFileSync('input.json', 'utf8'));
  const report = { tools: {} };
  // Event-driven handshake: the host creates a new public file only after
  // this payload is running inside the already-created security environment.
  await new Promise((resolve, reject) => {
    let observer;
    const timer = setTimeout(() => { observer.close(); reject(new Error('host creation handshake timed out')); }, 5000);
    const check = () => {
      if (fs.existsSync('host-created-file.txt')) {
        clearTimeout(timer); observer.close(); resolve();
      }
    };
    observer = fs.watch('.', check);
    observer.on('error', error => { clearTimeout(timer); observer.close(); reject(error); });
    fs.writeFileSync('payload-ready.txt', 'ready');
    check();
  });
  try { fs.readFileSync(input.futureSecret); report.futureRead = true; } catch { report.futureRead = false; }
  try { fs.writeFileSync(input.futureSecret, 'changed'); report.futureWrite = true; } catch { report.futureWrite = false; }
  try { fs.writeFileSync(input.publicWritable, 'payload-write'); report.publicWrite = true; } catch (error) { report.publicWrite = false; report.publicWriteError = error.code; }

  const token = spawnSync(input.restrictedHelper, ['inspect'], { encoding: 'utf8', timeout: 5000 });
  report.token = { exit: token.status, stdout: token.stdout, stderr: token.stderr, error: token.error?.message };
  report.softwareRead = fs.readFileSync(input.softwareFile, 'utf8');
  const alias = 'software-hardlink';
  try {
    fs.linkSync(input.softwareFile, alias);
    report.hardlinkCreated = true;
    try { fs.writeFileSync(alias, 'changed-through-hardlink'); report.hardlinkWrite = true; }
    catch (error) { report.hardlinkWrite = false; report.hardlinkWriteError = error.code; }
  } catch (error) { report.hardlinkCreated = false; report.hardlinkCreateError = error.code; }
  finally { try { fs.rmSync(alias, { force: true }); } catch (error) { report.hardlinkCleanupError = error.code; } }

  for (const name of ['git', 'node', 'cargo']) {
    const output = spawnSync(name, ['--version'], { encoding: 'utf8', timeout: 5000 });
    report.tools[name] = { exit: output.status, stdout: output.stdout, stderr: output.stderr, error: output.error?.message };
  }
  report.profiles = {};
  for (const name of ['HOME', 'USERPROFILE', 'TMPDIR', 'TEMP', 'TMP', 'APPDATA', 'LOCALAPPDATA']) {
    const directory = process.env[name];
    const proof = path.join(directory, 'proof-' + name);
    const command = 'echo private-' + name + '>"' + proof + '"';
    const profile = { directory, exists: fs.existsSync(directory) };
    try { fs.writeFileSync(proof, 'node-private-profile'); profile.nodeWrite = true; }
    catch (error) { profile.nodeError = { code: error.code, message: error.message }; }
    const output = spawnSync(path.join(process.env.SystemRoot, 'System32', 'cmd.exe'), ['/d', '/s', '/c', '"' + command + '"'], { windowsVerbatimArguments: true, encoding: 'utf8', timeout: 5000 });
    report.profiles[name] = { ...profile, exit: output.status, stderr: output.stderr, written: fs.existsSync(proof) };
  }
  const rust = spawnSync('cargo', ['test', '--offline', '--manifest-path', 'rust-task/Cargo.toml'], {
    env: { ...process.env, RUSTC_LOG: 'rustc_codegen_ssa::back::link=info' },
    encoding: 'utf8', timeout: 30000,
  });
  report.rustCheck = { exit: rust.status, stdout: rust.stdout, stderr: rust.stderr, error: rust.error?.message };
  try { fs.readFileSync(input.secret); report.secretRead = true; } catch { report.secretRead = false; }
  try { fs.writeFileSync(input.secret, 'changed'); report.secretWrite = true; } catch { report.secretWrite = false; }
  try { fs.readFileSync(input.markerSecret); report.markerRead = true; } catch { report.markerRead = false; }
  try { fs.writeFileSync(input.markerSecret, 'changed'); report.markerWrite = true; } catch { report.markerWrite = false; }
  try { await fetch(input.endpoint, { signal: AbortSignal.timeout(2500) }); report.loopback = true; } catch { report.loopback = false; }
  fs.writeFileSync('artifact.txt', 'native-api-output');
  fs.writeFileSync('report.json', JSON.stringify(report));
})().catch(error => { console.error(error.message); process.exitCode = 1; });
`);
const toolchainDirectory = path.join(rustup, 'toolchains');
const toolchainEntries = fs.readdirSync(toolchainDirectory);
assert.ok(toolchainEntries.length <= 32, 'bounded installed toolchain inputs');
const resolvedToolchains = toolchainEntries.map(name => path.join(toolchainDirectory, name))
  .filter(root => fs.existsSync(path.join(root, 'bin', 'rustc.exe')))
  .map(root => fs.realpathSync.native(root))
  .filter(root => {
    const relative = path.relative(rustup, root);
    return path.isAbsolute(relative) || relative === '..' || relative.startsWith(`..${path.sep}`);
  });
console.log(JSON.stringify({ resolvedToolchains }));
const readonlyPaths = [...new Set([
  process.env.SystemRoot,
  ...Object.values(tools).map(file => path.dirname(file)),
  // Git's bin launcher needs the rest of its installation. Cargo's proxy
  // needs Rustup's installed toolchains, but not Cargo's credential directory.
  path.dirname(path.dirname(tools.git)),
  rustup, ...resolvedToolchains, compiler.installation, sdk.root, softwareRoot,
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
fs.writeFileSync(path.join(workspace, 'captured-environment.json'), JSON.stringify(environment));
const relay = `
const fs = require('node:fs');
const {spawnSync} = require('node:child_process');
const environment = JSON.parse(fs.readFileSync('captured-environment.json', 'utf8'));
const child = spawnSync(process.execPath, ['payload.cjs'], { env: environment, stdio: 'inherit', timeout: 45000 });
if (child.error) console.error(child.error.message);
process.exitCode = child.status ?? 1;
`;
const relayCommand = `${quote(tools.node)} -e ${quote(`eval(Buffer.from('${Buffer.from(relay).toString('base64')}','base64').toString())`)}`;
const relayPath = path.join(workspace, 'contained-relay.cjs');
fs.writeFileSync(relayPath, relay);

const cases = [
  { name: 'public-read-baseline', command: relayCommand, payload: true },
  { name: 'deny-canary-explicitly', command: relayCommand, payload: true, deniedPaths: [secret, markerSecret, publicWritable, process.env.QQ_NATIVE_FUTURE_DIRECTORY] },
  { name: 'deny-volume-root', command: relayCommand, payload: true, deniedPaths: [path.parse(workspace).root] },
  { name: 'deny-volume-root-without-enumeration', command: relayCommand, payload: true, deniedPaths: [path.parse(workspace).root], enumeratePaths: [] },
  { name: 'deny-canary-directory', command: relayCommand, payload: true, deniedPaths: [path.dirname(secret)] },
  { name: 'deny-parent-with-allowed-children', command: relayCommand, payload: true, deniedPaths: [root, path.dirname(secret)] },
  { name: 'token-creation-control',
    command: `${quote(restrictedHelper)} ${quote(tools.node)} ${quote(payload)} control`, payload: true },
  { name: 'token-user-sid-control',
    command: `${quote(restrictedHelper)} ${quote(tools.node)} ${quote(payload)} user`, payload: true },
  ...['package', 'arap', 'package-arap', 'package-arap-registry', 'package-write', 'disable-marker', 'disable-marker-lpac', 'reduce-capabilities', 'reduce-capabilities-lpac', 'lpac-inherit', 'lpac-recreate'].map(mode => ({
    name: 'restricted-' + mode,
    command: `${quote(restrictedHelper)} ${quote(tools.node)} ${quote(mode.startsWith('lpac-') || mode.endsWith('-lpac') || mode === 'reduce-capabilities' ? relayPath : payload)} ${mode}`,
    payload: true,
  })),
];
for (const mode of ['write-sid', 'write-sid-lpac', 'write-sid-no-window']) {
  cases.push({ name: 'restricted-' + mode, payload: true,
    command: `${quote(restrictedHelper)} ${quote(tools.node)} ${quote(relayPath)} ${mode} ${quote(writeSid)}` });
}
for (const mode of ['write-sid-native', 'write-sid-native-no-window']) {
  cases.push({ name: 'restricted-' + mode, payload: false,
    command: `${quote(restrictedHelper)} ${quote(restrictedHelper)} inspect ${mode} ${quote(writeSid)}` });
}
const baseUi = { disable: false, clipboard: 'none', allowInputInjection: false };
const outcomes = [];
try {
  for (const test of cases) {
    for (const name of ['report.json', 'artifact.txt', 'software-hardlink', 'payload-ready.txt', 'host-created-file.txt']) fs.rmSync(path.join(workspace, name), { force: true });
    fs.rmSync(path.join(rustTask, 'target'), { recursive: true, force: true });
    fs.rmSync(futureSecret, { force: true });
    fs.writeFileSync(publicWritable, 'public-input');
    let futureCreated = false;
    let creationError;
    const creationObserver = fs.watch(workspace, () => {
      if (futureCreated || creationError || !fs.existsSync(path.join(workspace, 'payload-ready.txt'))) return;
      try {
        fs.writeFileSync(futureSecret, 'future-outside');
        futureCreated = true;
        fs.writeFileSync(path.join(workspace, 'host-created-file.txt'), 'created');
      } catch (error) { creationError = error; }
    });
    creationObserver.on('error', error => { creationError = error; });
    const started = performance.now();
    let output;
    try {
      output = await run({
        containment: { type: 'processcontainer', config: {
          capabilities: ['registryRead'],
          filesystem: { enumeratePaths: test.enumeratePaths ?? [...ancestorPaths] },
        } },
        command: test.command,
        workingDirectory: workspace,
        inheritDefaultEnvironment: false,
        environment: test.environment ?? environment,
        filesystem: {
          readonlyPaths: readonlyPaths.map(value => test.namespacedGrants ? path.toNamespacedPath(value) : value),
          readwritePaths: [workspace],
          deniedPaths: test.deniedPaths ?? [],
        },
        network: { egress: { default: 'deny' }, ingress: { default: 'deny', hostLoopback: 'deny' } },
        ui: baseUi,
        timeoutMs: 60_000,
      });
    } catch (error) {
      outcomes.push({ name: test.name, error: error.message });
      console.log(JSON.stringify(outcomes.at(-1)));
      assertHostState();
      continue;
    } finally { creationObserver.close(); }
    if (creationError) throw creationError;
    const outcome = { name: test.name, elapsedMs: performance.now() - started, ...output };
    const reportPath = path.join(workspace, 'report.json');
    if (test.payload && fs.existsSync(reportPath)) {
      const report = JSON.parse(fs.readFileSync(reportPath, 'utf8'));
      assert.equal(futureCreated, true, 'outside file must be created after payload launch');
      outcome.report = report;
      assert.equal(fs.readFileSync(publicWritable, 'utf8'), report.publicWrite ? 'payload-write' : 'public-input');
      // This probe isolates public-file containment. Rust compilation has its
      // own reported result and remains required by QQ's production fixture.
      outcome.fileBoundary = output.exitCode === 0 && !output.timedOut
        && report.tools.node.exit === 0 && report.secretRead === false
        && report.secretWrite === false && report.markerRead === false && report.markerWrite === false && report.futureRead === false && report.futureWrite === false && report.publicWrite === false && report.loopback === false
        && fs.existsSync(path.join(workspace, 'artifact.txt'));
      outcome.frozenProfiles = ['HOME', 'USERPROFILE', 'TMPDIR', 'TEMP', 'TMP', 'APPDATA', 'LOCALAPPDATA'].every(name => {
        const profile = report.profiles[name];
        return profile.directory === environment[name] && profile.nodeWrite === true
          && profile.exit === 0 && profile.written === true;
      });
      outcome.qualified = outcome.frozenProfiles && report.token.exit === 0 && output.exitCode === 0 && !output.timedOut && ['git', 'node', 'cargo'].every(name =>
        report.tools[name].exit === 0 && report.tools[name].stdout.trim() === expected[name])
        && report.rustCheck.exit === 0
        && report.secretRead === false && report.secretWrite === false && report.markerRead === false && report.markerWrite === false && report.futureRead === false && report.futureWrite === false && report.publicWrite === false && report.loopback === false
        && fs.existsSync(path.join(workspace, 'artifact.txt'))
        && fs.readFileSync(path.join(workspace, 'artifact.txt'), 'utf8') === 'native-api-output';
    }
    outcomes.push(outcome);
    console.log(JSON.stringify(outcome));
    // These remain unconditional safety assertions, including failed startups.
    assertHostState();
  }
  assertHostState();
  assert.equal(outcomes.find(outcome => outcome.name === 'public-read-baseline')?.report?.secretRead, true,
    'the public-read control must demonstrate the outside read before evaluating deny policies');
  assert.equal(outcomes.find(outcome => outcome.name === 'public-read-baseline')?.report?.markerRead, true,
    'the marker-read control must demonstrate its separate outside read');
  assert.equal(outcomes.find(outcome => outcome.name === 'public-read-baseline')?.report?.futureRead, true,
    'the baseline must read the outside file created after sandbox launch');
  assert.equal(outcomes.find(outcome => outcome.name === 'public-read-baseline')?.report?.publicWrite, true,
    'the write control must actually modify the same outside file before restricted write denial can qualify');
  assert.ok(outcomes.some(outcome => outcome.name.startsWith('restricted-') && outcome.qualified),
    'No additional token restriction both ran real tools and denied the public outside canary');
} finally {
  server.closeAllConnections();
  await new Promise(resolve => server.close(resolve));
  fs.rmSync(workspace, { recursive: true, force: true });
  fs.rmSync(root, { recursive: true, force: true });
}
