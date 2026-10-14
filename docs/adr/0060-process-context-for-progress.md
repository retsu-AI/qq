# ADR-0060: Bind process context before classifying progress

- Date: 2026-10-14
- Status: Proposed (action-bias AB1, ENG-1038)
- Supersedes: ADR-0054 decision 1's lexical shell-read accounting

## Context

A command named `grep` can execute an inherited-PATH replacement that writes
files. Saved command text cannot identify the executable, cwd capability or
child environment that actually ran. Hashing a pathname also leaves a race
between inspection and execution. Historical tool rows lack this evidence.

## Decision

1. Progress and heuristic audits consume executor-produced receipts. An
   executed process is Work unless its read behavior is explicitly approved
   and its launch binding is verified. Failed, cancelled and timed-out Work
   still counts as work and triggers the non-read-shell audit flag. A rejected
   or never-started call has no receipt; output text cannot manufacture one.
   The bounded lexical recognizer remains a qualification diagnostic and grants
   no process authority.
2. One prepared command owns captured environment values, the opened cwd and
   the selected image through reap. Preparation uses the bounded blocking-tool
   permits. Child environment is cleared and populated from this one snapshot;
   neither classification nor execution rereads ambient values. Captured values
   remain private. The existing timeout also bounds preparation between reads.
3. The trusted host supplies at most ten immutable approvals through
   `--approve-self-contained-read-image FAMILY:SHA256`, or the core embedding
   builder. Each approval attests self-contained read behavior for admitted
   argv/stdin in C/POSIX or the default unset locale, independent of HOME/TERM
   and without executable helpers, plugins or configuration-driven code.
   This is a behavioral trust decision; SHA-256 and static ELF format alone do
   not prove it. Workspace, session, HTTP and configuration inputs cannot
   add approvals. Existing-server attachment rejects nonempty flags, requiring
   a new server process rather than silently ignoring the host's request.
4. Verification initially supports literal Linux exec calls to cat, head,
   tail, wc, grep, nl, cut, pwd, echo and uniq. It uses the closed read argv
   grammar, no requested environment names, and a supported locale. Shell
   commands, other platforms and unsupported contexts execute under existing
   approval policy and count as Work. This changes accounting, not permission.
5. An absolute program path or wholly absolute captured PATH selects the
   first executable candidate. An unreadable candidate disqualifies binding
   rather than being skipped. A regular executable source must be at most
   16 MiB, with no set-id bits, file capabilities or noexec mount. The source
   is copied per call into a Linux memfd; write/grow/shrink/seal seals are
   confirmed before bounded readback and hashing. The approved native static
   ELF bytes execute through the held descriptor, preserving argv0. Source
   permission admission is a capture-time snapshot. There is no pathname cache
   and a changed source cannot execute old cached approved bytes.
6. Linux cwd uses a held workspace-contained directory descriptor rather than
   a replaceable pathname. The command consumes that same capability.
   Non-Linux cwd remains unbound and cannot qualify a verified read. Receipts
   contain an opaque context ID, canonical argument digest, public approval
   manifest digest, limited environment facts, cwd device/inode, verdict and
   actual child PID. They contain no environment values or fingerprints.
7. Store schema 47 follows main's AC3 schema 46 and adds nullable, bounded
   `tool_calls.execution_context_json`. Receipt, result, file state, spill and
   finished event commit together; failure rolls them all back. Historical
   rows remain NULL and cannot qualify execution effects. The wire envelope
   stays unchanged. Descriptor 14 identifies the public host manifest; prompt
   changes are independent. AB5's subsequent notice migration must be 48.

## Consequences

Default process calls conservatively count as Work. Built-in read tools keep
their existing read behavior. Hosts that need verified process reads must
approve a suitable self-contained image explicitly; dynamically linked images
and shell compounds cannot qualify through this initial implementation.
Preparation adds bounded filesystem work only on process calls, requiring
matched process/turn and persistence measurements before acceptance.

A receipt proves the prepared launch binding under a trusted behavioral
approval. It is not a sandbox or an independent proof that arbitrary native
code cannot mutate state. Missing or unsupported receipts remain unqualified
in historical and deployed cohort reports.
