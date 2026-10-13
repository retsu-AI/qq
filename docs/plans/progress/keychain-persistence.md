# Keychain persistence progress

## KP1

Source owner `/root/qq_keychain_persistence` owns this ledger and the isolated
`fix/qq-keychain-persistence-20261013` branch. Root owns live runtime acceptance,
publication, and the shared continuity notebook. Base is
`d377cf2e47628643130cf1ddc2820af4faf69bf5`. No source implementation is accepted.

### 2026-10-13 source diagnosis

- Read the pstack Codex adapter, Bug Fix playbook, root-cause and domain
  principles, signing guidance, repository workflow, and compact root checkpoint.
- Confirmed stable item identity, in-place backend update, attached-client
  metadata-only bootstrap, bounded same-store singleflight, and five-minute TTL.
- Root retained OS evidence for Always Allow decisions on old requesters and
  two current-server items. Both current backend reads returned success, followed
  by cache hits. SDK headers distinguish `-67065` from `-67034`.
- No live credential call, signing action, process change, capture, or test suite
  was run. No cache or credential mutation is justified by the incident evidence.
- Root authorized the finite installer-only scope and a bounded credential-free
  installer/process-metadata probe. Same-credential restart/update acceptance
  remains root-owned.

The Fix Root Causes principle excludes a cache-duration workaround. Model the
Domain keeps executable selection separate from credential identity and consent.
Delegation and test-first playbook steps are narrowed by the explicit one-worker
and no-new-test instructions. The live reproduction and nonauthor verification
belong to the existing root and reviewer, not another worker or credential probe.

### 2026-10-13 installer candidate

`install.sh` stages each payload into `.qq-versions/qq-<sha256>` without replacing
an existing payload and atomically selects a symlink. A regular-file macOS
migration checks `lsof` first and refuses observed use or inspection failure.
The guide records the session-preserving transition and the snapshot limit.
No auth source, item identity, cache, ACL, protocol, or schema changed.

The authorized probe compiled a credential-free executable with `proc_pidpath`,
ran the real installer against local release archives, and observed immutable
path selection, exact path/inode reuse, running old-payload retention, new-payload
execution, busy migration refusal, corrupt-payload refusal, and staging cleanup.
The first compiler invocation failed from the ambient SDK 27 linker mismatch;
the repair used the installed Xcode compiler and SDK explicitly. No global
toolchain setting changed. All owned child processes exited and temporary
payloads were removed.

Raw command output and process metadata are retained outside Git under
`reviews/alignment-20261011/qq-recovery-20261012/qq-keychain-persistence-20261013/`
in the manager workspace. These receipts prove installer behavior only, not
Keychain persistence, real QQ authentication, or provider completion.
Shell syntax, ShellCheck, and whitespace checks pass. No new repository test
suite was added or run. No standalone deslop tool was installed; the author
reviewed the small diff directly and added no explanatory code comments.
