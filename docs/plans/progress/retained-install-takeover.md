# Retained installer takeover

| Slice | Goal | Status | Branch/PR | Notes |
| --- | --- | --- | --- | --- |
| Retained install | Preserve executable bytes, atomic selection and recoverable publication | In progress | `fix/retained-install-recovery`, takeover of #287 | Awaiting parent review; no commits or pushes authorized |

## 2026-10-17

- Baseline: owned `.worktrees/takeover-install`, clean branch at current `origin/main` `55936a1e8cd875c9a683f2e4028c391c307c04a8`. Inspected candidate `refs/qq-review/287` (`f267641d567ab389020b8aaba4388bda3d178b64`) with `git show`; no ancestry cherry-picked. Candidate publishes via `ln stage/payload retained; rm stage/payload`, so a kill between commands leaves a second hardlink that subsequent validation rejects.
- Procedure: qq-mode bug-fix playbook and workflow loaded. Todo: (1) reproduce user's surface, (2) isolate cause, (3) failing test (skip commit: explicit parent prohibition), (4) fix root publication mechanism, (5) same-surface verification, (6) cargo xtask check, (7) assess enforcer, (8) opening-a-PR (skip: parent review first). Public installer tests encode recovery; no separate lint warranted. Principles: test behavior, not implementation; subtract before you add (no native macOS busy/Keychain scope).
- Red on main: `bash tests/install_sh.sh` exits 1, including `FAIL legacy migrates to a selector`, `FAIL legacy bytes retained`, `FAIL refuses foreign selector`, `FAIL refuses fifo selector`, `FAIL refuses hardlink selector`. Existing overwrite changes foreign-link target bytes.
- Implementation: checksum-named retained executables, verified legacy copy before selecting, read-only staging, same-filesystem `mv -n` publication rather than hardlink/unlink, then atomic selector replacement. Reuse checks type, checksum, single hardlink. No existing retained file is chmodded/deleted. Interrupted private stages are not swept. No arbitrary same-user race resistance or fsync/power-loss durability claim.
- Green: `sh -n install.sh`; `bash tests/install_sh.sh` (67 checks, including four deterministic SIGKILL cases before/after legacy/requested publication, migration/update/reuse/rollback, corrupt/foreign/special-node/hardlink refusal). Expected `Killed` diagnostics come from injected SIGKILL, verified exit 137. Recovery retains legacy and requested bytes and leaves unrelated staging files intact.
- `cargo xtask check --base origin/main`: pass (fmt, lint, clippy, qq + xtask nextest); scope 2/13 packages. `cargo xtask smoke`: 6 passed, 0 failed. Initial evidence retrieval refused after tests changed, correctly requiring gate rerun; rerun result recorded below.
- Limits: native macOS not exercised on this Linux host; no lsof busy check added. Rollback does not reverse store migrations or repair Keychain authorization. No dependency/protocol/schema changes.


## Independent-review follow-up

- Finding reproduced: a checksum-correct retained payload with mode 0755 passed the existing regular/executable/single-hardlink checks. Added regression first: `bash tests/install_sh.sh` exited 1 with `FAIL refuses writable retained payload` and `install_sh.sh: 1 check(s) failed`.
- Resolution: retained payload validation now reads GNU `stat -c %a` / BSD `stat -f %Lp` octal permissions and rejects any owner/group/other write bit (`0222`). It never chmods an existing payload. Writable legacy executables still migrate through a new mode-0555 copy. Preserved parent's guide rollback using quoted `payload_sha`; no guide edits in this follow-up.
- Evidence: `bash tests/install_sh.sh` passes, including new/legacy payload mode 0555 and writable checksum-correct refusal with bytes, mode 0755 and selector unchanged; four injected SIGKILL cases still pass (expected `Killed` diagnostics). `sh -n install.sh` and `git diff --check` exit 0. `cargo xtask check` passes fmt/lint/clippy/nextest for qq + xtask (2/13 packages); `cargo xtask smoke` passes all 6 scenarios, 0 failures.
- Principle: test behavior, not implementation (exercise the installer and inspect filesystem state). Native BSD/macOS execution remains unverified on this Linux host. No dependencies, commits or pushes.


## Parent verification

- Parent independently ran the installer suite successfully and corrected the guide's shell placeholder to a quoted variable.
- Additional `cargo xtask check --full` failed: 2,469/2,471 tests passed; existing qq-core edit-approval and text-only-delegation tests hit elapsed-time deadlines. Failure details remain in `target/qq-check/last.json`; no runtime Rust source changed by this slice. Both failing tests passed in a narrow rerun (2/2). This is not a full-green result and does not justify weakening the tests.
- Native macOS validation and full-workspace qualification remain outstanding. Linear issue creation paused at the external approval gate; no issue identifier has been assigned.