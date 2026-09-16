# ADR-0020 — Shell `Forbidden` is a policy decision above every approval mode, produced by a CST classifier whose rules are self-tested data

**Status:** Accepted
**Date:** 2026-09-14
**Deciders:** tool-layer plan T6
**Implements:** [`tools.md` § Shell Classification](../design/tools.md#shell-classification) (design as built; the plan's D5 is a pointer to it),
[`tools.md` § Approval Policy](../design/tools.md#approval-policy)

## Context

Shell is the one tool path checks cannot contain: any command can touch
anything the server process can. Until T6 the only gate between a model and
`rm -rf /` was the approval mode. `full` executed everything; `auto` ran a
hand-written substring detector (`dangerous_shell_command`) that split on
`|;&` and looked at each segment's first word, which a subshell, a `$(…)`,
an `env -i`, or a `sh -c '…'` walked straight past. T5 shipped multi-file
atomic edits and batch writes; shipping more power without a matching
guardrail would have made the "safety release" a misnomer.

Two constraints shaped the answer. First, the harness containers and
Terminal-Bench reference runs use `--approval full`, so a guardrail that only
applies under `ask`/`auto` protects no benchmark and few power users. Second,
whatever judges a command runs on every shell call before policy; it has to
be fast (the plan's gate is ≤ 200 µs for 1 KiB) and its false-positive rate
has to be low enough that `auto` stays useful.

## Decision

A shell command is parsed to a concrete syntax tree with `tree-sitter-bash`
(already in the binary via `qq-tui`) and judged by two walks over a static
rule table. The verdict is a three-tier lattice, `Forbidden > Prompt >
Allow`, and the strictest verdict over every simple command anywhere in the
tree wins.

- **`Allow`** is reachable only through `word_only_sequence`: the program is
  `cmd (&& || ; |) cmd…` of literal words (no expansion, glob, substitution,
  construct, or writing redirect) and every command is in the allow table
  with a read/build shape and workspace-relative operands.
- **`Prompt`** is the default: parse errors, dynamic words, unlisted
  programs, constructs, and the explicit list (deletions, git mutations and
  remotes, mode changes, in-place edits, installs, containers, downloads,
  signals, `xargs`/`tee`, writing redirects, inline interpreters, operands
  outside the workspace).
- **`Forbidden`** is a new `PolicyDecision` refused under **every** mode,
  `full` included: `rm -rf` on root/home/outside the workspace, privilege
  escalation, raw device writes, filesystem formatting, power state, git
  force-push, recursive mode changes on root, download piped to an
  interpreter, dynamic `eval`, fork bomb, history/shred, shell-profile and
  `~/.ssh` writes, reverse shells, and loader/`PATH`-class env prefixes.
  The one escape is a grant that quotes the exact command string: the user
  typed the whole thing. Prefix grants lift `Prompt` only.

Wrappers are peeled ≤ 8 deep (`env`, `nice`, `nohup`, `time`, `timeout`,
`stdbuf`, `xargs`, `sudo`/`doas`/`su`, and `sh|bash|zsh -c STRING`, which
is reparsed as a program of its own ≤ 4 deep), so the inner command is
judged as well as the wrapper. Commands over 16 KiB skip parsing and are
`Prompt`. The rules live in `approval/rules.rs` as a table of program sets
and argv predicates; each tier's `match`/`not_match` examples (~250) run as
one unit test, so a rule's intent and its reach are checked together. The
model's refusal names the rule and an alternative; the approval preview
carries `verdict` and `reasons` so a client can say why it is asking.

## Consequences

- Positive: `full` now means "unrestricted authority over the workspace",
  not over the machine; `auto` executes exactly what the table allows and
  asks for the rest, replacing a heuristic that could be routed around with
  a subshell. Every refusal is explainable by rule id. The classifier is
  3–10 µs on ordinary commands and 165 µs on a 1 KiB one-liner (gate ≤ 200
  µs); `tool_dispatch` is unchanged.
- Negative / risks: a rule table is a curated list — a new dangerous shape
  is `Prompt` until it is added, and an over-broad `Forbidden` rule blocks a
  legitimate command until a user quotes it exactly. `Allow` is narrower
  than before for `auto` users: a command with `$VAR`, a glob, or a
  redirect now asks. The parse costs quadratically on pathological input;
  the 16 KiB ceiling bounds it.
- Follow-ups: T7 adds `exec` (argv, no shell) so the classifier sees exact
  words; T13 measures the `auto` prompt rate on real runs and tunes the
  allow table from evidence. A `deny_rules`/`allow_rules` config surface
  is deliberately deferred until a real user needs it.

## Alternatives considered

| Alternative | Why not (now) |
| --- | --- |
| Keep the hand tokenizer, extend the list | It cannot see through `(…)`, `$(…)`, `sh -c`, or `env -i`; every fix is a new special case. |
| A regex per rule over the raw string | Same blindness to structure, plus quoting bugs; `"rm -rf /"` inside `echo` would match. |
| A full bash interpreter / sandbox | Out of proportion; the goal is a fast, explainable pre-check, not emulation. |
| `Forbidden` only under `ask`/`auto` | Leaves `full` — the harness and power-user default — unprotected, which is where the accidents happen. |
| Make rules configurable now | No user has asked; a table in code with tests is easier to review than a DSL. |

## Evidence / references

- `crates/qq-core/src/approval/classify.rs`: `classify_command`,
  `word_only_sequence`, `Collector::peel`, `MAX_CLASSIFIED_BYTES`.
- `crates/qq-core/src/approval/rules.rs`: `RuleId`, `judge`, `forbidden`,
  `prompt`, `allow`, `path_escapes`; test `rule_examples_hold`.
- `crates/qq-core/src/approval.rs`: `PolicyDecision::Forbidden`,
  `SessionGrants::quotes_exactly`, `forbidden_result`; test
  `forbidden_commands_are_refused_under_every_mode_unless_quoted_exactly`.
- `crates/qq-protocol/src/sessions.rs`: `ShellCommandPreview { verdict, reasons }`, `ShellVerdict`.
- Bench `classify_command`: `target/qq-perf/t6-2026-09-14/classify_command.txt`.
