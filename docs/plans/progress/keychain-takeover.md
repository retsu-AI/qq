# Keychain read reuse takeover (#299)

| Slice | Goal | Status | Branch/PR | Notes |
| --- | --- | --- | --- | --- |
| K299 | Coalesce repeated stored-keyring reads | In progress | `fix/qq-keychain-persistence-20261013`, #299 | Auth only; no Jev, diagnostics, installer or wire changes |

## Contract

Store clones share at most 64 distinct in-flight keyring resolutions and 64
successful cached values. Cache identity includes credential name, expected
endpoint and durable index epoch. Registration, endpoint binding and index
integrity are checked before reuse. Successful values may be reused for five
minutes; external keychain rotation, deletion or access-policy changes can
therefore take up to five minutes to affect ordinary stored resolutions.
Expiration is a reuse deadline, not guaranteed memory erasure. Entries are
pruned on later resolution or store drop. Independently constructed stores do
not share the memo. File and Windows-protected file reads remain uncached.
OAuth refresh/deadline paths bypass this memo for authoritative rereads.

The synchronous API still requires a blocking caller context. Followers and
capacity waiters use condition variables; a stuck OS backend read remains
noninterruptible and can hold waiters. The existing bounded asynchronous
request loader and timeout remain unchanged. This does not fix macOS signing,
Keychain ACL persistence across upgrades, or eliminate all OS prompts.

## Procedure

1. **Reproduce on the user's surface.**
2. **Binary-search the cause.**
3. **Commit the failing test first** (`test(<scope>): reproduce <bug>`).
4. **Fix at the root.**
5. **Verify on the same surface.**
6. **Run `cargo xtask check`.**
7. **Ask: could a rule have caught this?**
8. Run `.qq/skills/qq-mode/playbooks/opening-a-pr.md`.

## 2026-10-16

- Preserved the original PR branch/history, reconciled with current main, and
  removed the original unrelated stack from the resulting tree. Selected the
  pre-diagnostics auth implementation for focused review.
- Red regression at `547b247b`: sequential resolution across clones reads the
  backend twice (expected one). `cargo nextest run -p qq-auth -E
  'test(stored_resolution_reuses_success_and_invalidates_on_epoch_change)'`
  failed with `left: 2, right: 1`.
- Initial narrowed implementation: 74/74 auth tests passed. Added deterministic
  expiry/external deletion and cross-store mutation/endpoint/index regressions.
  Independent review requested. Full gate and smoke pending.


## Codex review follow-up

- Initial head `97bc28cd`: full workspace 2,655/2,655, minimal provider
  211/211, six smoke scenarios and hosted checks passed.
- Addressed Codex's three findings: signal-driven concurrency tests without
  polling deadlines; a clone-shared controllable test clock exercising the
  production five-minute expiry calculation at 299/300 seconds; cached initial
  xAI OAuth reads with uncached refresh-lock rereads.
- xAI regression failed before the fix (`left: 6, right: 1` backend reads),
  then all 77 auth tests passed. Signal-driven concurrency tests passed twenty
  stress iterations. The xAI request memo shares ordinary stored-resolution
  expiry; only the authoritative refresh-lock reread bypasses it.
