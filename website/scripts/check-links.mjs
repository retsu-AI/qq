#!/usr/bin/env node
// Fails when a built page links to a site path that was not built. Runs after
// `astro build`; external links are not fetched.

import { readdir, readFile, stat } from 'node:fs/promises';
import { join, resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const website = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const dist = join(website, 'dist');
const { site } = await import(join(website, 'site.config.mjs'));
const base = site.base.replace(/\/$/, '');

async function* htmlFiles(dir) {
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) yield* htmlFiles(path);
    else if (entry.name.endsWith('.html')) yield path;
  }
}

async function exists(path) {
  try {
    const info = await stat(path);
    return info.isFile() || (info.isDirectory() && (await stat(join(path, 'index.html'))).isFile());
  } catch {
    return false;
  }
}

const broken = [];
let checked = 0;
for await (const file of htmlFiles(dist)) {
  const html = await readFile(file, 'utf8');
  const page = file.slice(dist.length);
  for (const match of html.matchAll(/\b(?:href|src)="([^"]+)"/g)) {
    const target = match[1];
    if (/^(https?:|mailto:|#|data:|javascript:)/.test(target)) continue;
    const [path, fragment] = target.split('#');
    if (path === '') continue;
    let resolved;
    if (path.startsWith('/')) {
      if (!path.startsWith(base + '/') && path !== base) {
        broken.push(`${page}: ${target} (outside base ${base}/)`);
        continue;
      }
      resolved = join(dist, path.slice(base.length));
    } else {
      resolved = resolve(dirname(file), path);
    }
    checked += 1;
    if (!(await exists(resolved))) broken.push(`${page}: ${target}`);
    else if (fragment && !path.endsWith('.xml')) {
      const targetHtml = resolved.endsWith('.html') ? resolved : join(resolved, 'index.html');
      const body = await readFile(targetHtml, 'utf8');
      if (!body.includes(`id="${fragment}"`)) broken.push(`${page}: ${target} (no #${fragment})`);
    }
  }
}

if (broken.length > 0) {
  console.error(`check-links: ${broken.length} broken internal links\n  ` + broken.join('\n  '));
  process.exit(1);
}
console.log(`check-links: ${checked} internal links resolve`);
