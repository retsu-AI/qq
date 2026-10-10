# ADR-0057 — Skills ship supporting files, and delegates may pull skills

**Status:** Proposed
**Date:** 2026-10-09
**Deciders:** workflow-packs WP0
**Implements:** `docs/plans/workflow-packs.md` WP1, WP2

## Context

A workflow pack (prior art: Cursor's pstack) is a short router persona plus
playbooks and principles. The model reads those on demand. QQ blocks this in
two ways:

- `load_skill` reads only `<name>/SKILL.md` (`workspace/skills.rs:146`).
  `read_file` cannot reach a global pack, which sits outside the workspace.
- Child task runs drop the skill index and `load_skill`
  (`plan.rs:569-576`, `lib.rs:1903-1913`). A delegate inherits the parent's
  persona (`sessions/subagents.rs:908-916`). It can see that a playbook is
  named but cannot open it.

Without supporting files, each playbook and principle would need its own index
entry. That would use about 40 of the 64 slots, or need a disclosure flag.

## Decision

1. `load_skill` accepts an optional `file`, a path relative to the skill's
   directory.
   - The file follows the same contract as `SKILL.md`: a regular UTF-8 file of
     at most 64 KiB, read from the same opened root.
   - Paths containing `..`, absolute paths and symlink escapes are refused.
   - A command, which has no directory, accepts no `file`.
   - The run's prompt identity records the file and its hash.
   - `/name` invocation is unchanged.
2. `RunCapabilities::allow_guidance` splits into `select_guidance` (a leading
   `/name` in the run's prompt) and `pull_skills` (the skill index plus
   `load_skill`).
   - Child task runs get `pull_skills` and not `select_guidance`.
   - Model-written task text still never selects guidance.

## Consequences

- Positive:
  - A pack can ship any number of playbooks and principles behind one index
    entry each.
  - Delegates follow the same procedures as their parent.
  - No new manifest schema, front-matter key, agent type or install command is
    needed.
- Negative / risks:
  - A child's cached prompt prefix grows by the skill index, at most
    64 × 512 bytes. WP2 measures this with the `context_assembly` bench.
  - Child transcripts gain `load_skill` calls.
- Follow-ups:
  - A typed `spawn_agent { profile }` for reviewer children on a different
    persona.
  - A disclosure flag, only if the 64-entry cap binds in practice.

## Alternatives considered

| Alternative | Why not (now) |
| --- | --- |
| A front-matter flag to hide principle skills (pstack `disable-model-invocation`) | Supporting files remove the need for it. Each hidden skill still costs an index slot |
| Named agent types (`agents/*.md`, pstack `poteto-agent`) | Profile inheritance already gives delegates the persona. A second agent definition lane conflicts with ADR-0004 |
| Absolute skill path plus the general read tool (Codex, Claude Code, OpenCode, pi) |  Needs `read_file` to leave the workspace for global packs; fx's directory-confined `skill {location, resource}` keeps confinement, so QQ follows fx |
| Let `read_file` reach pack roots | It widens workspace confinement for every tool path, not just guidance |
| Children select guidance from their task text | It breaks the provenance rule in `docs/design/tools.md` § Explicit Commands And Skills |

## Evidence / references

- `crates/qq-core/src/workspace/skills.rs:104-151` (roots and document paths)
- `crates/qq-core/src/workspace/prepare.rs:233-263` (`load_disclosed_skill`)
- `crates/qq-core/src/plan.rs:563-581` (prefix keys)
- `crates/qq-core/src/sessions/execution.rs:78-103` (summarizer key mirror)
- Survey: `docs/plans/workflow-packs.md` § Survey. fx `src/builtins/tools.zig:538-567` (resource-confined skill tool); Codex `core/src/agent/role.rs:91-117` and fx `agent_adapter.zig:292` (children keep the skill catalog)
- pstack: `cursor/plugins` `pstack/skills/poteto-mode/SKILL.md`,
  `pstack/agents/poteto-agent.md`
