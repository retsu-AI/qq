# Startup documentation website

## Delivery contract

Extend the existing QQ Astro and Starlight website with source-backed documentation for the current Retsu and QQ startup systems. Readers can find a task, understand which component owns it, follow the existing operational guide, and distinguish source behavior from planned work and verified deployment. This documentation PR does not merge application changes or deploy the website.

The documentation owner integrates one site writer and the separately owned master architecture assets. The application, infrastructure, full specification, and data-platform owners retain their source. The existing startup delivery record remains authoritative for acceptance. This plan describes a finite documentation delivery, not a new management queue.

## Grounding and design choice

QQ already has a static website in `website/`. `docs/guide/*.md` supplies its guide pages. `website/sidebar.json` defines navigation and descriptions. The generator checks guide and manifest agreement, while the post-build checker validates internal links and fragments. Search uses the built Pagefind index.

Two approaches were considered. A separate documentation application would split navigation, search, hosting, and content ownership. Extending the existing guide keeps those mechanisms together. The chosen design adds a clearly named startup systems navigation group, a task-oriented overview, and focused guides. It preserves existing QQ install, provider, CLI, and troubleshooting pages rather than copying them.

## Reader journeys

1. A new user opens the overview, chooses terminal, native client, assistant, or API, sees the current availability, and follows the appropriate getting-started instructions.
2. A contributor follows QQ development from an isolated branch through source checks, a real workflow, independent review, a PR, and an explicit adoption or rollback decision.
3. A maintainer finds a feature in the feature map, follows its source entrypoint, and sees its storage owner, protocol, evidence scope, and related guide.
4. An operator reads infrastructure and recovery instructions without mistaking a deployment plan, source candidate, or fixture for a live service.
5. A reader opens the architecture visual and retains its technical component names alongside optional aliases.
6. A keyboard or phone user navigates the same content, uses search, follows section anchors, and changes theme without losing readability.

## Content and data model

The existing sidebar registry remains the navigation source. Every added page has one H1, one manifest entry, a short description, and links to related guides. The feature reference records journey, owning component, source entrypoint, current basis, and documentation destination. Repeated status vocabulary is limited to implemented source, candidate source, planned, and runtime or deployment unverified. A source pin identifies a snapshot and does not establish acceptance.

The new guide family covers overview, getting started, feature map, QQ development, assistant gateway and nodes, direct and assistant native modes, API and billing, data and models, infrastructure, observability, MCP and ACP, recovery, and documentation maintenance. Existing detailed QQ guides remain the source for released terminal behavior. Cross-repository summaries link to exact source revisions where available. Private-repository links explicitly require access. Unpublished local candidates use source paths and a clearly stated candidate basis rather than invented public permalinks.

The master visual is supplied by its separate owner under `website/public/architecture/`. It can be opened from the overview. A text reference remains usable if the visual is unavailable or cannot be rendered.

## State, storage, authorization, and privacy

The site is static. It creates no user account, backend, credential store, database, or product record. Versioned Markdown, JSON, CSS, and static assets are the durable content. Browser search and navigation state are disposable. Existing theme preferences are the only persisted UI preference.

The site requires no login. Private source links retain their repository access controls. Public content contains no secrets, account identifiers, user records, raw chats, local absolute paths, private diagnostic dumps, or copied financial details. Code examples use placeholders and never solicit credentials. Documentation explains product authorization boundaries without implementing them.

## Preservation, recovery, concurrency, and failure

There is no product schema migration. Existing guide files remain unchanged unless a narrowly necessary navigation edit is justified. Each writer has disjoint files and an isolated branch. The documentation owner integrates the architecture commit after its review. No shared application checkout is reset or cleaned.

Git history preserves authored content. Static output is reproducible from the lockfile and build scripts. A documentation rollback uses the previous Git revision and the ordinary website release process under separate deployment authority. Generator, typecheck, build, or link failures must be repaired before publishing a ready PR. Broken or unavailable external source links are described as access or publication limitations, not verified interfaces.

Owned disposable outputs are the new worktree's `website/dist`, `website/.astro`, generated `website/src/content/docs/docs`, `website/src/generated`, generated font CSS, copied font files, and copied installer. They are removed after final browser verification. Shared dependencies and other owners' outputs are preserved. Browser screenshots, video, logs, and review receipts are retained outside the public repository by the documentation owner.

## Acceptance

- All thirteen topic guides exist, have manifest entries, and remain navigable alongside the existing guide.
- The feature reference links current QQ features to the existing detailed guides and source entrypoints, and maps current startup components with explicit candidate or planned status.
- Source summaries preserve QQ as the execution engine, the separate Rust assistant ownership, the same native clients' direct and assistant modes, and the selected hosted orchestration boundary. They do not turn plans into implemented features.
- The existing website build completes with Astro checks and all internal links and fragments valid. No new unit or integration tests are added or run.
- Actual browser checks cover overview navigation, a deep link, search, mobile layout, keyboard access, both themes, and the architecture entry. Real screenshots and a workflow video retain their observed revision and limits.
- An independent non-author reviews the final exact source head, content provenance, public-data boundaries, and browser evidence. Repairs receive a new final verdict.
- A scoped branch is committed and pushed, a PR is created and attached to the chat, and hosted checks are read back for that revision. No merge or live deployment is inferred.

## Throughput checkpoint

- Blocking first steps. Inspect existing site and source ownership, preserve concurrent changes, write this specification, then start implementation.
- Independent workstreams. The site writer owns new guides and navigation. The architecture owner supplies static assets on a separate branch. A supported Devin source-research task can supply a disjoint content brief after its effective models are verified.
- Shared mutable state. Root owns this plan, the documentation ledger, integration, and PR. Existing full-specification and startup-register writers remain unchanged.
- Smallest safe decomposition. One writer keeps guide content and sidebar coherent. One independent reviewer follows the integrated candidate. No competing site or application implementation is introduced.

## Deliberate omissions

This PR does not implement missing assistant nodes, native pairing, billing composition, ACP, error tracking, hosted deployment, or paid journeys. It records their actual source state and open acceptance. It does not publish the private full specification. It does not change the Devin plan, create recurring monitors, migrate user data, or merge or deploy.
