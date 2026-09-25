#!/usr/bin/env node
// Generates the documentation pages from the repository's user guide.
//
// `docs/guide/*.md` is the single source of truth; this script copies each
// guide into `src/content/docs/docs/<name>.md` with Starlight frontmatter,
// rewrites relative links so they resolve on the site, and copies the
// reviewed installer to `public/install.sh`. The generated files are
// gitignored; `pnpm build` runs this first.
//
// Runs on plain Node (no dependencies) so CI and the Nix shell need nothing
// beyond the toolchain that builds the site.

import { readdir, readFile, writeFile, mkdir, rm, copyFile } from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const website = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const repo = resolve(website, '..');
const guide = join(repo, 'docs', 'guide');
const out = join(website, 'src', 'content', 'docs', 'docs');
const sidebar = JSON.parse(await readFile(join(website, 'sidebar.json'), 'utf8'));

// Sidebar descriptions are the only text that lives outside docs/guide; the
// title is always the guide's H1.
const descriptions = new Map(
  sidebar.flatMap((group) => group.items.map((item) => [item.page, item.description])),
);

await rm(out, { recursive: true, force: true });
await mkdir(out, { recursive: true });

const names = (await readdir(guide)).filter((name) => name.endsWith('.md'));
const pages = new Set(names.map((name) => (name === 'README.md' ? 'index' : name.slice(0, -3))));

let generated = 0;
const problems = [];
for (const name of names) {
  const page = name === 'README.md' ? 'index' : name.slice(0, -3);
  const source = await readFile(join(guide, name), 'utf8');

  const heading = source.match(/^# (.+)$/m);
  if (!heading) {
    problems.push(`${name}: no H1`);
    continue;
  }
  const title = heading[1].replace(/`/g, '');
  if (!descriptions.has(page)) {
    problems.push(`${name}: no sidebar entry in website/sidebar.json`);
    continue;
  }

  // Starlight renders the title itself; the H1 in the body would duplicate it.
  let body = source.replace(heading[0] + '\n', '').replace(/^\n+/, '');

  // Relative links. Sibling guides become site routes; anything that leaves
  // docs/guide (design notes, runbooks, repository files) points at GitHub,
  // since those documents are not part of the site.
  body = body.replace(/\]\(([^)\s]+)\)/g, (whole, target) => {
    if (/^(https?:|mailto:|#)/.test(target)) return whole;
    const [path, fragment] = target.split('#');
    const anchor = fragment ? `#${fragment}` : '';
    const sibling = path.match(/^(?:\.\/)?([a-z0-9-]+)\.md$/);
    if (sibling) {
      const slug = sibling[1] === 'README' ? 'index' : sibling[1];
      if (!pages.has(slug)) problems.push(`${name}: link to missing guide ${path}`);
      // Relative, so the same output works under any deployment base. Pages
      // live at /docs/<slug>/ and the index at /docs/, so a link's prefix
      // depends on where it is written from.
      const up = page === 'index' ? './' : '../';
      return slug === 'index' ? `](${up}${anchor})` : `](${up}${slug}/${anchor})`;
    }
    if (path === '') return `](${anchor})`;
    const repoPath = resolve('/docs/guide', path).slice(1);
    return `](https://github.com/retsu-AI/qq/blob/main/${repoPath}${anchor})`;
  });

  // The repository README is the developer entry point; on the site the
  // guide index is the landing page's "Read the docs" target instead.
  const frontmatter = [
    '---',
    `title: ${JSON.stringify(title)}`,
    `description: ${JSON.stringify(descriptions.get(page))}`,
    `editUrl: https://github.com/retsu-AI/qq/edit/main/docs/guide/${name}`,
    '---',
    '',
  ].join('\n');
  await writeFile(join(out, `${page}.md`), frontmatter + body);
  generated += 1;
}

for (const page of descriptions.keys()) {
  if (!pages.has(page)) problems.push(`sidebar.json names ${page}, but docs/guide has no ${page}.md`);
}

await copyFile(join(repo, 'install.sh'), join(website, 'public', 'install.sh'));

// Build-time facts the site config cannot read itself: the config is bundled
// into the build, where the repository is no longer on disk.
const cargo = await readFile(join(repo, 'Cargo.toml'), 'utf8');
const version = cargo.match(/^\[workspace\.package\][^[]*?^version = "([^"]+)"/ms)?.[1];
if (!version) problems.push('Cargo.toml: no [workspace.package] version');
await mkdir(join(website, 'src', 'generated'), { recursive: true });
await writeFile(join(website, 'src', 'generated', 'site.json'), JSON.stringify({ version }, null, 2) + '\n');

if (problems.length > 0) {
  console.error('sync-docs: the guide and the site disagree:\n  ' + problems.join('\n  '));
  process.exit(1);
}
console.log(`sync-docs: ${generated} pages from docs/guide, install.sh copied, version ${version}`);
