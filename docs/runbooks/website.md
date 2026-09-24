# Website

The documentation site at <https://retsu-ai.github.io/qq/> is built from
`website/` and deployed by `.github/workflows/website.yml`. Its
documentation pages are generated from `docs/guide/`; nothing under
`website/` holds guide text.

## Change the guide

Edit `docs/guide/<page>.md`. That is the whole procedure; the site rebuilds
and redeploys when the change reaches `main`. A PR that touches the guide
also gets a site build, which fails when

- a guide has no entry in `website/sidebar.json`, or an entry has no guide;
- a guide links to a sibling that does not exist;
- a rendered page has an internal link or `#fragment` that does not resolve.

Heading anchors are Starlight's slugs of the heading text; renaming a heading
that another page links to fails the build until the link follows.

## Add a page

1. `docs/guide/<name>.md` with one H1 (it becomes the page title).
2. Add `{ "page": "<name>", "label": "…", "description": "…" }` to the right
   group in `website/sidebar.json`. The description is the page's meta
   description and search summary.
3. `cd website && pnpm build`.

## Build locally

```sh
nix develop            # node 24 + pnpm
cd website
pnpm install --frozen-lockfile
pnpm build             # sync-docs, astro check, astro build, pagefind, check-links
pnpm preview           # http://localhost:4321/qq/
```

`pnpm dev` serves with hot reload; search only works against a production
build because Pagefind indexes `dist/`.

## Deploy

Push to `main` touching `website/**`, `docs/guide/**`, `install.sh`, or
`Cargo.toml` runs the `Website` workflow: `build` uploads `website/dist` as
the Pages artifact, `deploy` publishes it through the `github-pages`
environment. Pull requests only run `build`. `workflow_dispatch` redeploys
`main` without a change.

One-time repository setup (done for `retsu-AI/qq`): *Settings → Pages →
Source: GitHub Actions*.

## Move to a custom domain

1. `website/site.config.mjs`: `url: 'https://docs.example'`, `base: '/'`.
2. `website/public/CNAME` containing `docs.example`.
3. DNS: `CNAME docs.example → retsu-ai.github.io`, then *Settings → Pages →
   Custom domain* and enforce HTTPS.

Every internal link is relative or built from `href()`, so nothing else
changes.

## What the landing page may claim

The landing sections (`website/src/components/*Section.astro`,
`TerminalPreview.astro`) show keys, verdicts, session groups, and commands.
Each must be something the guide says today; when the product changes, the
guide changes first and the landing page follows in the same PR.
