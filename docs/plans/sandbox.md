# Plan: autonomous OS sandbox

**Status:** Proposed. Supersedes the H10 placeholder in
[speed-first](speed-first.md#h10--os-process-sandbox) and answers finding F16
in [harness comparison](../research/harness-comparison.md). Nothing here is
built yet.

## Why

A goal should be able to run for hours or days on Linux, macOS, and Windows
with nobody watching. Today that is impossible for two reasons:

1. **Every risky action asks a person.** Approval is the only safety
   boundary: `shell` runs `/bin/sh -c` (or `cmd /C`) with the user's full
   authority (`crates/qq-core/src/tools/shell.rs`), so anything the classifier
   cannot prove harmless is held. In one real store, 519 shell calls waited on
   a human or timed out: pushes and `gh` (134), `/tmp` writes (71), python
   heredocs (28), git discards (14), and 266 ordinary workspace commands. An
   unattended goal stalls at the first one.
2. **The LLM reviewer was the wrong fix.** It approved about 2,100 calls,
   denied 120+ legitimate ones (`/tmp` writes, `pkill`, pushes the user had
   asked for), and timed out or abstained on others. A model guessing at
   safety is neither safe nor autonomous. Jev was removed in #374 for this
   reason.

Other harnesses converged on the same answer. Codex and Claude Code put
commands inside an OS sandbox that enforces filesystem and network limits.
Inside the sandbox, a command runs without asking. A command that needs more
is either matched by a saved rule, or refused back to the model as an
ordinary error it can work around. A person is only asked about escalations,
and only when someone is attached.

## Outcome

- A goal runs unattended for at least 24 hours on Linux, macOS, and Windows,
  with **zero blocking approval prompts**, and finishes or stops for a reason
  the plan names (budget, deadline, goal check), never because a hold had no
  answer.
- Shell and exec children cannot write outside the writable roots, cannot
  reach the network except through the policy, and cannot outlive their
  call, on every supported OS.
- When no sandbox is available, QQ **fails closed**: unattended runs refuse
  to start, and interactive runs fall back to today's approval behavior and
  say so. It never silently runs commands unsandboxed.

## Model

Three layers, checked in order for every `shell`/`exec` call:

1. **Policy rules** (configuration, saved grants). Exact command-prefix rules
   such as `git push origin <current branch>`, `gh pr create`, or
   `cargo publish --dry-run` decide `allow-unsandboxed`, `allow-sandboxed`,
   or `deny`. They are durable, reviewable, and trust-gated like today's
   grants. `forbidden` shell shapes stay forbidden.
2. **The sandbox** runs everything else, without asking:
   - **Filesystem:** the whole disk is readable except denied paths (credential
     stores, `~/.ssh`, `~/.config/qq` secrets, keychains). Writes are allowed
     only under writable roots: the workspace, a per-session temp directory
     that `TMPDIR` points at, and configured extras. `.git` and `.qq` inside
     the workspace stay read-only unless policy allows them (so a goal can
     commit but cannot rewrite hooks or trust config).
   - **Network:** off by default. When enabled, it goes only through a
     QQ-owned loopback proxy that enforces a host allowlist, backed by OS
     blocking of direct egress.
   - **Processes:** all children live in one kill-on-close group, have no new
     privileges, and are reaped when the call ends, is cancelled, or times
     out.
3. **Escalation** happens only when the model explicitly asks for it: the
   call carries `sandbox: "escalate"`, a justification, and a proposed prefix
   rule. Then:
   - **Unattended (goal, `qq run`, no client):** a policy rule decides;
     otherwise the call is refused immediately with a typed
     `sandbox_denied` result naming what was blocked. The run continues.
     Optionally the request is queued as a non-blocking *pending grant* the
     user can accept later, which turns it into a saved rule for future runs.
   - **Attended:** the human is asked, as today. The existing
     `reviewer_model` may pre-answer escalations only, never ordinary calls.

A command that fails inside the sandbox (EACCES, connection refused) returns
its normal output plus a short, typed marker saying the sandbox blocked it,
so the model can choose another route instead of retrying blindly.

Approval modes collapse to: `read-only` (sandbox, no writable roots),
`auto` (sandbox plus rules plus refuse-on-escalation, the unattended
default), `ask` (sandbox plus human for escalations), and `full` (no sandbox,
explicit and logged).

## Platform backends

All backends sit behind one trait in a new crate, `qq-sandbox`, that turns a
`SandboxPolicy` plus a command into a spawned, contained process. Platform
dependencies are optional and per-target. **The OS calls need `unsafe`**
(`prctl`/seccomp, Windows token and ACL APIs), so `qq-sandbox` (or a tiny
`qq-sandbox-helper` binary it execs) is the one documented exception to
`#![forbid(unsafe_code)]`; `cargo xtask lint` and `AGENTS.md` must name that
exception explicitly. Every other crate stays forbid-unsafe.

### Linux

- Primary: **bubblewrap** (`bwrap`). Mount `/` read-only, bind writable
  roots read-write, re-bind `.git`/`.qq` read-only, use a fresh `/tmp`, and
  set `--unshare-user --unshare-pid --unshare-net` (net shared only when
  network is enabled through the proxy), `--die-with-parent`, and
  `--new-session`.
- Inside, the helper sets `no_new_privs` and installs a **seccomp** filter
  that blocks socket families other than AF_UNIX/loopback-to-proxy,
  `AF_VSOCK`, `io_uring`, `ptrace`, and mount/namespace syscalls.
- Landlock is a fallback only for hosts without unprivileged user
  namespaces. It cannot isolate Unix sockets, so it is weaker and must be
  reported as such.
- Detection: probe `bwrap` and user-namespace availability at startup and
  report the result in `qq doctor`.

### macOS

- **Seatbelt** via `/usr/bin/sandbox-exec` with a generated `.sbpl` profile:
  `(deny default)`, allow reads, allow writes under writable roots and the
  session temp dir, deny writes to `.git`/`.qq` subpaths, and allow network
  only to the proxy's loopback port.
- `sandbox-exec` is deprecated but still shipped and is what Codex and
  Claude Code use. Track it; the fallback is the Endpoint Security / App
  Sandbox route, which needs signing and is out of scope for the first slice.

### Windows

- **Restricted token:** `CreateRestrictedToken` with `WRITE_RESTRICTED` and a
  per-session capability SID. Writable roots get an ACE for that SID, so
  writes elsewhere fail. Children run in a **Job Object** with
  `KILL_ON_JOB_CLOSE`, an active-process limit, and UI restrictions.
- Network: Windows Filtering Platform rules (or firewall rules keyed to the
  sandbox SID or user) allow only loopback to the proxy. This may need a
  one-time elevated `qq sandbox setup` that creates a dedicated low-privilege
  sandbox user plus the WFP rules. Without setup, the backend is
  filesystem-only and reports that network is unenforced.
- Shell: `cmd /C` today. Evaluate PowerShell and Git Bash for parity with the
  model's expectations, but the sandbox is independent of the shell choice.
- AppContainer is a candidate for the no-admin path; its behaviour with
  arbitrary developer toolchains is unverified (see Open questions).

### Network proxy

A QQ-owned HTTP CONNECT and SOCKS5 proxy on loopback, started per runtime.
It enforces the host allowlist (configuration plus `allow_hosts` grants),
denies private and link-local ranges as `fetch` does, logs each decision as
a session event, and is the only route out where the OS backend blocks
direct egress. `HTTP(S)_PROXY`/`ALL_PROXY` are set for children; OS blocking
makes ignoring them fail closed rather than leak.

## Slices

Each slice ships behind configuration, keeps today's behavior as the default
until S6, and lands with its acceptance tests.

| Slice | Scope | Acceptance |
| --- | --- | --- |
| S0 | Threat model and policy types. `SandboxPolicy` (writable roots, denied reads, network mode, allowlist), `sandbox` field on `shell`/`exec`, typed `sandbox_denied` result, protocol and config additions, no enforcement. | Protocol fixtures; config round-trips; ADR for the model above, including the `unsafe` exception. |
| S1 | `qq-sandbox` crate and Linux bubblewrap + seccomp backend; per-session temp dir; process-group reaping. | Escape suite (below) passes on Linux CI; no leaked process after cancel, timeout, shutdown, or crash recovery; launch overhead p50 ≤ 15 ms reported. |
| S2 | Loopback proxy with allowlist; network modes `off`/`proxy`. | Direct egress blocked; allowlisted host reachable through the proxy; private ranges refused; decisions appear as events. |
| S3 | Policy rules and escalation: prefix rules with `allow-unsandboxed`/`allow-sandboxed`/`deny`; unattended refuse-with-reason; non-blocking pending grants. | A goal pushes to its branch via a rule with no prompt; an unruled escalation is refused at once and the run continues; a pending grant accepted later is honoured by the next run. |
| S4 | macOS Seatbelt backend. | Same escape suite on macOS CI. |
| S5 | Windows restricted-token + Job Object backend; optional elevated setup for network. | Same escape suite on Windows CI, with the network rows marked as needing setup when it has not run. |
| S6 | Make `auto` sandboxed by default; fail closed for unattended runs without a backend; `qq doctor` reports backend and guarantees; guide pages. | 24-hour unattended goal soak on each OS with zero blocking holds; docs updated. |

### Escape suite (every backend)

Run as real child processes, never mocked:

- Write outside the writable roots (home, `/etc` or `C:\Windows`, a sibling
  repo) fails; writes inside succeed; `TMPDIR` writes succeed and are cleaned.
- Write to `.git/hooks` and `.qq/config.ron` fails unless a rule allows it.
- Read denied paths (`~/.ssh`, the credential store) fails.
- Symlink and hardlink escapes, `..` traversal, and bind-mount tricks fail.
- Direct TCP/UDP egress fails; loopback to the proxy works; DNS exfiltration
  through the proxy is limited to allowlisted names.
- `fork` bombs and daemonizing (`setsid`, `nohup`, double fork, Windows
  `start /b`) die when the call ends.
- Privilege gain (`sudo`, setuid binaries, `ptrace` of the parent) fails.
- Cancellation, timeout, runtime shutdown, and restart recovery leave no
  sandboxed process alive.

## Non-goals

- Containers or VMs as the default (too slow to start per call, heavy on
  Windows and macOS). A container backend can implement the same trait later.
- Sandboxing MCP servers or the QQ process itself in the first slices; they
  keep today's trust model.
- Any LLM in the default decision path. `reviewer_model` stays optional and
  answers escalations only.

## Open questions (verify before the slice that needs them)

- Claude Code sandbox-runtime details (bwrap/Seatbelt plus proxy; no native
  Windows) are reported second-hand; confirm before reusing its profiles.
- Landlock ABI support across the kernels we claim (for the fallback).
- Whether AppContainer works for typical toolchains (cargo, node, python)
  without admin, or whether the elevated sandbox-user setup is required.
- `sandbox-exec` deprecation timeline on current macOS.
- Launch overhead on Windows for restricted token + Job Object per call.

## References

- Codex: `codex-rs` linux-sandbox (bwrap, seccomp), seatbelt profiles,
  windows-sandbox (restricted token, capability SIDs, elevated setup),
  network-proxy, and the `untrusted`/`on-request`/`never` approval policies
  with `require_escalated` plus justification and prefix rule.
- QQ: H10 in [speed-first](speed-first.md); F16 in
  [harness comparison](../research/harness-comparison.md); approval policy in
  [tools design](../design/tools.md#approval-policy).
