# Codex device authorization progress

This ledger owns the bounded offline authorization slice that supports the
existing ENG-809/ENG-791 qualification work. It does not create another
evaluation backlog or authorize a paid run.

| Slice | Goal | Status | Branch/PR | Notes |
| --- | --- | --- | --- | --- |
| ENG-809.AUTH | Opt-in Codex device authorization with existing QQ storage and refresh | In review | [PR #206](https://github.com/retsu-AI/qq/pull/206) | Base `bd8c580b38eab0ec71c047150223989512dadd44`; accepted offline candidate `9897c6aebaec869195a84aaa17a02b34254c92d4` |

## 2026-09-26 — implementation start

- Source: remote `main` at `bd8c580b38eab0ec71c047150223989512dadd44` in isolated worktree `/Users/romanmondello/Developer/qq-device-auth-20260926`; the held ENG-791 checkout is unchanged.
- Owned paths: `crates/qq-auth/`, the `auth login` CLI wiring, focused provider/auth guides, and this ledger. Browser login remains the default.
- First red check: `cargo test -p qq-auth codex_device_login_honors_poll_interval_and_exchanges_once --no-run` failed because the device-flow API did not exist. The first green run passed the new interval, PKCE exchange, exactly-once exchange, and storage-path fixture.
- The first full auth run passed all new device tests but one existing oversized browser-login fixture ended with macOS `ECONNRESET`; its isolated rerun passed. Exact comparison found the already-reviewed seven-line accepted-socket repair and deadline regression in held commit `118e3d94cdb294aaa506e96720b55c1a77ae272f`. This slice reuses only that demonstrated callback repair; it does not cherry-pick the held branch.
- No live OAuth or model call is part of this slice. Provider support on a real headless host, protected storage for its service identity, and any paid qualification remain separate ENG-809 evidence gates.

## 2026-09-26 — review candidate

- The opt-in command is `qq auth login openai-codex --device-auth`. It uses the published device endpoints, prints the fixed verification URL and a bounded printable user code, treats HTTP 403/404 as pending, enforces the server interval and a 15-minute deadline, verifies the returned PKCE proof, exchanges exactly once, and stores through the existing `openai-codex` credential path. The loopback browser flow remains the default.
- Offline tests cover request bodies, both published user-code spellings, pending and terminal statuses, interval bounds, malformed/oversized/control-character responses, PKCE mismatch, timeout/cancellation, ambiguous exchange and storage failures, identity rejection, durable reopen, refresh, and the unchanged browser path. They make no live OAuth or model request.
- A pre-existing headless recovery fixture failed under the host's `/var/...` `TMPDIR` because SQLite uses `SQLITE_OPEN_NOFOLLOW` while that spelling traverses the `/var` symlink. The fixture now canonicalizes its temporary root exactly like the neighboring shared helper. The exact focused test passes under the host-default spelling (1 passed); production session paths are unchanged. A configured production data directory with symlinked ancestors remains untested and intentionally receives no claim here.
- Final local gates used the pinned Rust 1.97.1 binaries. `cargo test --workspace` passed 1,922 tests with 5 ignored under a canonical sibling temporary root and a clean credential environment; `cargo fmt --all -- --check`, workspace/all-target/all-feature Clippy with `-D warnings`, `cargo build --workspace`, and `git diff --check` passed.
- Complete logs, commands, and hashes are retained in `qq-cloud-auth-20260926/device-auth-worker-evidence/` in the manager review packet. Disposable build output remains at `/Users/romanmondello/Developer/qq-device-auth-20260926/target-device-auth`; test temporary roots remain at `/Users/romanmondello/Developer/qq-device-auth-20260926/.tmp-device-auth-tests` and `/Users/romanmondello/Developer/.tmp-qq-device-auth-tests-20260926` until independent review finishes.

## 2026-09-26 — independent-review repair

- Independent review of `58d4bf3828e8670181ec641a345c2e940db167c9` required two changes: a successful blocking poll could return after the authorization deadline and still exchange/store, and the existing refresh coalescing test used one shared request-provider cache rather than independent store owners.
- Poll and device-flow token-exchange requests now receive only the remaining authorization duration. The state machine rechecks cancellation and deadline after each blocking request and immediately before storage. Deterministic injected-clock regressions prove that a poll or exchange which crosses the deadline performs no later exchange or storage.
- Two independently constructed stores and request providers now share one credential directory and coordinated fake refresh transport. The test proves one effective refresh, the same newest epoch and access token in both waiters, and the same durable value after reopening the store.
- The repair's red compile evidence, focused green tests, and final exact-head gates are retained with the independent review packet. Real persistent-runner qualification remains the separate row 11 gate.
