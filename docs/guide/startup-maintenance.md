# Maintain the documentation

Edit guide source and keep the static site reproducible. The site is Astro and
Starlight with no server, account system, or product database.

## Add or update a page

Write one page in one Diátaxis mode. Use one H1, sentence-case headings, real
source names, and links to the existing detailed QQ guides. Add one matching
entry to [website/sidebar.json](https://github.com/retsu-AI/qq/blob/9bae6937b50a013f652db8963ac3590103eb4b84/website/sidebar.json).
The [sync script](https://github.com/retsu-AI/qq/blob/9bae6937b50a013f652db8963ac3590103eb4b84/website/scripts/sync-docs.mjs)
copies guide pages into the site and rejects a missing or extra entry.

Use the status terms implemented source, candidate source, planned, and
runtime or deployment unverified. Include the source revision or source path
when a claim depends on a private candidate, and say repository access
required. Never include secrets, raw chats, account identifiers, local absolute paths, or private diagnostic dumps.

## Check the site

From `website/`, the existing scripts provide these checks:

```sh
pnpm sync
pnpm check
pnpm build
```

The build runs guide sync, Astro checks, static generation, and internal link
and fragment validation. Run the narrowest check while editing; the release
owner runs the full site and browser checks on the integrated revision.

## Preserve and roll back

Keep generated content and build output disposable. Do not commit generated
site pages, credentials, or local runtime data. Git preserves authored Markdown
and sidebar history. A documentation rollback restores the prior Git revision
and follows ordinary website release authority.

For a behavior change, update the relevant design or runbook with the source
change. Do not use this guide family to claim that an application, hosted
service, payment path, or customer workflow shipped.
