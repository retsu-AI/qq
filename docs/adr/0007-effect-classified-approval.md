# ADR-0007 — Classify tool approval from the catalog effect class, not the name

**Status:** Accepted
**Date:** 2026-09-04
**Deciders:** speed-first plan D4 / H13
**Implements:** [`tools.md`](../design/tools.md) approval policy

## Context

`approval::classify` keyed on tool-name prefixes: `mcp__` was recognized, and
every other unknown name fell to `ToolClass::Unknown`, which executed in every
approval mode including read-only. Embedded-host `ext__` tools therefore
bypassed approval (P0). Effect was re-derived from name strings in four places.

## Decision

The immutable `ToolCatalog` records an `EffectClass` (`ReadOnly`, `Mutating`,
`Shell`, `External`) for every tool at compile time. `RuntimeToolCall` carries
it from catalog lookup, and `classify(effect, name, arguments)` matches on the
effect, consulting arguments only for the shell and `spawn_agent` refinements.
`ToolClass::Unknown` is deleted; a name absent from the catalog is a tool error
before approval. Host `read_only` hints remain advisory and never change a
decision.

## Consequences

- Positive: `ext__` tools are denied under read-only and held under ask and
  supervised; one JSON parse and two string matches removed per call.
- Negative / risks: none observed; MCP decisions unchanged by test.
- Follow-ups: HC2's `exposed_tools` narrows the catalog and relies on this
  ordering (exclusion is a catalog error, never an approval hold).

## Alternatives considered

| Alternative | Why not |
| --- | --- |
| Add an `ext__` prefix arm | Fixes the symptom; keeps four derivations |
| Let hosts declare effect | Hints must not grant authority |

## Evidence / references

- `crates/qq-core/src/catalog.rs:68-73` (`EffectClass`);
  `crates/qq-core/src/approval.rs:22-33` (`ToolClass`), `:165-172`
  (`classify`).
- Commits `d58b066` (catalog), `ea5a6af` (decision; removes `Unknown`).
- Test `classification_follows_the_catalog_effect_and_reads_refining_arguments`
  and the five-mode `ext__` matrix (`approval.rs`).
