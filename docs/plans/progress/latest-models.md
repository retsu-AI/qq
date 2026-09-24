# Latest model support (ENG-885)

## Effort implementation session

- In progress: shared Max value, Anthropic output_config.effort encoding, per-model Claude ladders and GPT-6 Sol/Luna Max.
- Picker now offers only advertised choices plus default; unknown models no longer receive an invented global ladder. Explicit none is no longer prepended to Claude choices.
- Stacked branch `feat/eng-885-model-efforts` targets `feat/eng-885-latest-models` (PR 142).
- Protocol 28 fixtures generated, historical fixtures retained; store schema 36 gates older readers and decodes Max. All 49 migration tests pass.
- Added bounded Anthropic pagination and authoritative listing, with two-page wire regression.
- Final workspace tests, all-target/all-feature Clippy, formatting and build pass.
- Discovery capability import, explicit provider-default versus configured inheritance semantics, complete model metadata audit and Opus 5.5 signed replay remain unfinished. Stack is partial and must remain draft.


| Slice | Goal | Status | Branch/PR | Notes |
| --- | --- | --- | --- | --- |
| ENG-885.1 | GPT-6 catalogs and authoritative Codex discovery | In review | [PR 142](https://github.com/retsu-AI/qq/pull/142) | Preserve explicit models and offline fallback |
| ENG-885.2 | Opus 5.5 signed thinking replay | Planned | follow-up | Separate durable-storage/recovery slice; not advertised as supported |

## 2026-09-23

- Created isolated worktree from `8089a0e`; clean baseline.
- Linear tracking: https://linear.app/retsu-ai/issue/ENG-885.
- Confirmed official model IDs `gpt-6-sol`, `gpt-6-luna`, `claude-opus-5-5`.
- Codex discovery currently unions live results with every builtin, retaining stale models.
- Anthropic thinking signatures are discarded before core turn persistence. Official thinking docs require their exact replay during tool rounds; adding only a catalog entry is insufficient.

## 2026-09-23 verification

- Codex upstream release `rust-v0.156.1` explicitly adds Sol/Luna; discovery version updated accordingly.
- Added explicit model provenance, authoritative Codex picker/spawn validation, hidden-entry filtering, and malformed/oversized response fallback.
- Regression tests cover selected/explicit routes, empty/live/offline catalogs, malformed entries, GPT-6 metadata, and client-version wire contract.
- Independent read-only review identified malformed catalog eviction and unnecessary validation requests; both fixed before final checks.
- Passed `cargo fmt --all -- --check`, workspace all-target/all-feature Clippy with denied warnings, `cargo test --workspace`, and `cargo build --workspace`.
- `cargo bench -p qq-core --bench plan_compile`: 23,485 ns compile; 2,313 ns descriptor digest. Informational only; no pre-change baseline captured, so no regression claim.
- No live inference or account-specific availability verification. Pricing remains unknown; Codex context uses conservative existing 272K convention; `max` effort remains unsupported.
- Opus 5.5 deferred to a separate durable replay slice: complete signature/redacted-block capture, ordered persistence, restart/tool-loop tests, byte bounds, incompatible-provider projection, and compaction/prefix invalidation. This PR must not close ENG-885.

