// Runs only in the normal host token on the controller's private desktop.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { run } from '@microsoft/mxc-sdk/v1';

const requestPath = process.argv[2];
assert.ok(requestPath && path.isAbsolute(requestPath));
assert.ok(fs.statSync(requestPath).size <= 1024 * 1024);
try {
  const result = await run(JSON.parse(fs.readFileSync(requestPath, 'utf8')));
  process.stdout.write(JSON.stringify({ ok: true, result }) + '\n');
} catch (error) {
  process.stdout.write(JSON.stringify({ ok: false, error: error.message }) + '\n');
}
