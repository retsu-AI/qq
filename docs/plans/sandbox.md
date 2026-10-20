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
- When no sandbox is available, or the available backend cannot enforce
  every guarantee above (filesystem, denied reads, egress, resource
  ceilings), QQ **fails closed**: unattended and `auto` runs refuse
  to start, and interactive runs fall back to today's approval behavior and
  say so. It never silently runs commands unsandboxed.

## Model

Three layers, checked in order for every `shell`/`exec` call:

1. **Policy rules** (configuration, saved grants). Exact command-prefix rules
   such as `git push origin <current branch>`, `gh pr create`, or
   `cargo publish --dry-run` decide `allow` or `deny`. An `allow` rule never
   leaves the sandbox: it runs the command sandboxed with **explicit extra
   resources** named in the rule (hosts for the proxy such as `github.com`,
   a credential such as the git/`gh` token exposed to that call only, extra
   writable paths). This matters because commands like `git push` or
   `cargo publish` run repository-controlled hooks, helpers and build
   scripts; an unsandboxed durable rule would hand model-written code the
   user's full authority. Rules are durable, reviewable, and trust-gated like
   today's grants. `forbidden` shell shapes stay forbidden.
2. **The sandbox** runs everything else, without asking:
   - **Filesystem:** the whole disk is readable except denied paths (credential
     stores, `~/.ssh`, `~/.config/qq` secrets, keychains). Writes are allowed
     only under writable roots: the workspace, a per-session temp directory
     that `TMPDIR` points at, and configured extras. `.git` and `.qq` inside
     the workspace stay read-only unless policy allows them (so a goal can
     commit but cannot rewrite hooks or trust config).
     Denied reads are enforced, not advisory, on every backend.
   - **Network:** off by default. When enabled, it goes only through a
     QQ-owned loopback proxy that enforces a host allowlist, backed by OS
     blocking of direct egress.
   - **Processes:** all children live in one kill-on-close group, have no new
     privileges, and are reaped when the call ends, is cancelled, or times
     out. Each call has hard ceilings that hold *while it runs*: process count,
     memory, and CPU time (cgroup v2 `pids.max`/`memory.max` via a delegated
     systemd scope or bwrap's cgroup on Linux, `RLIMIT_NPROC`/`RLIMIT_AS`
     plus a supervising watchdog on macOS, Job Object process, memory and
     CPU limits on Windows).
3. **Escalation** happens only when the model explicitly asks for it: the
   call carries `sandbox: "escalate"`, a justification, and a proposed prefix
   rule. Then:
   - **Unattended (goal, `qq run`, no client):** a matching `allow` rule
     runs it sandboxed with the rule's extra resources; otherwise the call is
     refused immediately with a typed `sandbox_denied` result naming what was
     blocked. The run continues. Optionally the request is queued as a
     non-blocking *pending grant* the user can accept later, which turns it
     into a saved (still sandboxed) rule for future runs.
   - **Attended:** the human is asked, as today. Only an attended human may
     approve **unsandboxed** execution, and only as an exact, one-shot
     approval of that command; it is never saved as a rule. The existing
     `reviewer_model` may pre-answer sandboxed escalations only, never
     ordinary calls and never unsandboxed ones.

When the backend reports an authoritative denial for the call, the result
carries a short, typed marker naming what was blocked, so the model can
choose another route instead of retrying blindly. Authoritative signals are
the proxy's own refusal log, seccomp `SECCOMP_RET_USER_NOTIF`/audit records,
Seatbelt violation reports (`sandboxd` log, filtered to the call's process
tree), and Windows denied-access ETW events for the sandbox SID. QQ never
infers a sandbox denial from exit codes, `EACCES`, or a refused connection
in ordinary output: without a backend signal there is no marker. Backends
that cannot supply a signal for a class of denial report that in
`qq doctor`, and the marker is omitted, not guessed.

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
  set `--unshare-user --unshare-pid --unshare-net` **always**,
  `--die-with-parent`, and `--new-session`. The host network namespace is
  never shared, because seccomp cannot see a `connect` destination (it is
  indirect memory) and so cannot tell the proxy from any other address.
- Network, when enabled: the sandbox's own namespace has only loopback. The
  session's proxy endpoint is bridged in as a Unix socket bind-mounted into
  the sandbox, with a tiny in-namespace forwarder listening on
  `127.0.0.1:<port>` that relays to that socket. Nothing else inside the
  namespace has a route out, so direct egress fails by construction.
- Inside, the helper sets `no_new_privs` and installs a **seccomp** filter
  that blocks `AF_VSOCK`, `AF_NETLINK` beyond what the forwarder needs,
  `io_uring`, `ptrace`, and mount/namespace syscalls.
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

- **Identity that actually removes access.** `WRITE_RESTRICTED` alone only
  fences writes; the child would keep the user's read access to `~/.ssh` and
  credential stores, so it is not enough. The backend runs children either
  as a **dedicated low-privilege sandbox user** (created by a one-time
  elevated `qq sandbox setup`) or in an **AppContainer**, so that reads are
  granted only where ACLs allow them. Readable roots (toolchains, the
  workspace) get read/execute ACEs for that identity, writable roots get
  modify ACEs, and denied paths get none. A restricted token with full (not
  write-only) restricting-SID semantics is a further option to evaluate.
- Children run in a **Job Object** with `KILL_ON_JOB_CLOSE`, active-process,
  memory and CPU limits, and UI restrictions.
- Network: Windows Filtering Platform rules (or firewall rules keyed to the
  sandbox user or AppContainer SID) allow only loopback to that session's
  proxy port.
- **Fail closed:** if setup has not run, or any guarantee (denied reads,
  egress blocking) cannot be enforced, the backend counts as *unavailable*
  for `auto` and unattended runs, which then refuse to start. It never runs
  as a filesystem-only backend with network unenforced.
- Shell: `cmd /C` today. Evaluate PowerShell and Git Bash for parity with the
  model's expectations, but the sandbox is independent of the shell choice.
- AppContainer is a candidate for the no-admin path; its behaviour with
  arbitrary developer toolchains is unverified (see Open questions).

### Network proxy

A QQ-owned HTTP CONNECT and SOCKS5 proxy. Each sandboxed call gets its
**own endpoint** bound to its session and run: a per-call Unix socket on
Linux and macOS (bridged as above), and on Windows a per-call loopback port
that requires an unguessable per-call credential in the proxy handshake.
The proxy evaluates only that session's resolved host policy (configuration,
`allow_hosts` grants, and the matched rule's extra hosts), so one session's
grants never serve another, and every decision event is attributed to the
right session and call. It denies private and link-local ranges as `fetch`
does and is the only route out. `HTTP(S)_PROXY`/`ALL_PROXY` are set for
children; OS blocking makes ignoring them fail closed rather than leak.

## Slices

Each slice ships behind configuration, keeps today's behavior as the default
until S6, and lands with its acceptance tests.

| Slice | Scope | Acceptance |
| --- | --- | --- |
| S0 | Threat model and policy types. `SandboxPolicy` (writable roots, denied reads, network mode, allowlist), `sandbox` field on `shell`/`exec`, typed `sandbox_denied` result, protocol and config additions, no enforcement. | Protocol fixtures; config round-trips; ADR for the model above, including the `unsafe` exception. |
| S1 | `qq-sandbox` crate and Linux bubblewrap + seccomp backend (always its own network namespace); per-session temp dir; process-group reaping; per-call PID/memory/CPU ceilings. | Escape suite (below) passes on Linux CI; no leaked process after cancel, timeout, shutdown, or crash recovery; launch overhead p50 ≤ 15 ms reported. |
| S2 | Proxy with a per-call, session-bound endpoint (Unix socket bridged into the namespace; authenticated loopback port on Windows); network modes `off`/`proxy`; authoritative denial signals. | Direct egress blocked with network on; allowlisted host reachable only through the call's own endpoint; another session's endpoint and grants unusable; private ranges refused; decisions attributed to the right session; no marker on ordinary `EACCES`. |
| S3 | Policy rules and escalation: prefix rules with `allow` (sandboxed, with explicit extra hosts/credentials/paths) or `deny`; unattended refuse-with-reason; non-blocking pending grants; attended exact one-shot unsandboxed approval. | A goal pushes to its branch via a rule with no prompt, and the repository's push hook stays confined; an unruled escalation is refused at once and the run continues; a pending grant accepted later is honoured by the next run; no rule can grant unsandboxed execution. |
| S4 | macOS Seatbelt backend. | Same escape suite on macOS CI. |
| S5 | Windows backend: dedicated sandbox user or AppContainer with read/write ACLs, Job Object limits, WFP egress rules; elevated `qq sandbox setup`. | Same escape suite on Windows CI, including denied reads; without setup the backend reports unavailable and `auto`/unattended runs refuse to start. |
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
- A `fork` bomb and a memory hog stay within the call's process and memory
  ceilings *while running* (the host stays responsive and its PID count is
  bounded), and everything dies when the call ends.
- Daemonizing (`setsid`, `nohup`, double fork, Windows `start /b`) dies when
  the call ends.
- One session's sandbox cannot use another session's proxy endpoint or
  grants.
- An `allow` rule's command (`git push` running a repository hook) stays
  sandboxed: the hook cannot write outside writable roots or reach hosts the
  rule does not name.
- A program that fails with an ordinary `EACCES` or refused connection gets
  no sandbox marker; a real backend denial does.
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
