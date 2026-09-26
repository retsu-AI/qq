#!/usr/bin/env node
// Run after the site build: node scripts/check-install.mjs [landing-html].
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';

const html = await readFile(process.argv[2] || new URL('../dist/index.html', import.meta.url), 'utf8');
const tabs = [...html.matchAll(/<a\b([^>]*\brole="tab"[^>]*)>([\s\S]*?)<\/a>/g)];
const windows = tabs.find(([, , label]) => label.replace(/<[^>]*>/g, '').trim() === 'Windows');
assert.ok(windows, 'Windows install tab exists');
const panelId = windows[1].match(/href="#([^"]+)"/)?.[1];
assert.ok(panelId, 'Windows tab names its panel');
const panelStart = html.indexOf(`id="${panelId}"`);
const panel = html.slice(panelStart, html.indexOf('</starlight-tabs>', panelStart));
assert.ok(panelStart >= 0, 'Windows panel exists');
assert.ok(!panel.includes('<qq-copy'), 'Windows instructions must not offer a no-op copy command');
assert.ok(panel.includes('https://github.com/retsu-AI/qq/releases/latest'), 'Windows has an actionable latest-release link');
assert.ok(panel.includes('/qq/docs/install/#windows'), 'Windows links to verification and setup');
assert.ok(panel.includes('SHA256SUMS'), 'Windows reminds users to verify the download');
assert.equal((html.match(/<qq-copy\b/g) || []).length, 4, 'Other four install methods retain copy commands');
console.log('check-install: Windows download/setup links and four command-copy methods pass');
