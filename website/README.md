# QQ website

The landing page and the user guide at <https://retsu-ai.github.io/qq/>.
A static [Astro](https://astro.build) 5 + [Starlight](https://starlight.astro.build)
site; no server, client framework, analytics, or third-party scripts.

## The guide is generated, not copied

`docs/guide/*.md` is the only source for the documentation pages.
`scripts/sync-docs.mjs` runs before every build and

- writes each guide to `src/content/docs/docs/<name>.md` with Starlight
  frontmatter (`README.md` becomes the `/docs/` index; the title is the
  guide's H1; the description comes from `sidebar.json`);
- rewrites relative links: sibling guides become site routes, anything that
  leaves `docs/guide/` (design notes, runbooks) points at GitHub;
- copies the reviewed `install.sh` to `public/install.sh`, so
  `curl -fsSL https://retsu-ai.github.io/qq/install.sh | sh` is the real
  installer;
- records the workspace version from `Cargo.toml` for the landing page.

It fails the build when a guide has no sidebar entry, a sidebar entry has no
guide, or a guide links to a guide that does not exist. Do not edit files under
`src/content/docs/docs/`; they are gitignored and overwritten. Edit the guide.

`scripts/check-links.mjs` runs after the build and fails on any internal link
or fragment that does not resolve in `dist/`.

## Add a guide page

1. Write `docs/guide/<name>.md` with one H1.
2. Add `{ "page": "<name>", "label": …, "description": … }` to the right group
   in `sidebar.json`.
3. `nub run build`.

## Develop

[nub](https://nubjs.com) and Node 24; `nix develop` provides both. nub reads
the pnpm-format lockfile and `pnpm-workspace.yaml` as they are, so plain
pnpm works too, but the lockfile is maintained with nub.

```sh
cd website
nub ci               # strict install from the lockfile
nub run dev          # http://localhost:4321/qq/  (search needs a production build)
nub run build        # sync, type-check, build, index search, check links
nub run preview
```

## Deploy

`.github/workflows/website.yml` builds on every pull request that touches the
site or the guide, and deploys to GitHub Pages on push to `main`. Nothing is
deployed from a PR. `site.config.mjs` is the one place the URL, base path, and
GitHub link live; moving to a custom domain is two lines there plus
`public/CNAME`.

## Layout

```text
website/
  astro.config.mjs         Starlight config; sidebar is read from sidebar.json
  site.config.mjs          URL, base, GitHub link, install commands
  sidebar.json             page order, labels, descriptions
  scripts/sync-docs.mjs    docs/guide -> src/content/docs/docs (generated)
  scripts/check-links.mjs  post-build internal link check
  src/
    content.config.ts
    content/docs/index.mdx landing page (the only authored content)
    components/            Starlight overrides and landing sections
    styles/custom.css      Ink palette tokens and Starlight mappings
    assets/                wordmark and OG source
  public/                  favicon, og-image
```

The nested `content/docs/docs/` is deliberate: Starlight owns `/` and `/docs/`
in one application.

## Design tokens

Raw terminal colors: paper `#d8dee9`, slate `#7b8497`, sky `#8fb8e8`, copper
`#e0a071`, gold `#e6c07b`, rose `#ec7b8d`, mint `#8fd3a6`, graphite `#20242c`,
background `#141720`. Light mode: background `#f6f7f9`, text `#1d222b`. Code and
terminal surfaces stay graphite in both modes. Inter Variable for prose,
JetBrains Mono 400/500 for code. Radii 4/6/8/10px; no gradients; the only
motion is the short `qq` typing sequence, disabled under reduced motion. All of
this lives in `src/styles/custom.css`.
