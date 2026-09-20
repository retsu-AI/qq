# ADR-0035 — Global configuration may be a leaf symlink to a regular file

**Status:** Accepted
**Date:** 2026-09-20
**Deciders:** GitHub #83; Home Manager global configuration
**Implements:** [`architecture.md` § Repository Layout](../design/architecture.md#repository-layout)

## Context

Declarative user configuration on NixOS/Home Manager writes
`~/.config/qq/config.ron` as a symlink whose target is a regular file in the
Nix store. QQ rejected every configuration path that was a symlink, including
that user-global file, so a valid model route never loaded. Project
configuration can still be planted by a workspace checkout; that boundary is
intentional. `QQ_CONFIG_CONTENT` already injects inline RON and is not a
substitute for a managed user file.

## Decision

`SourceKind::Global` file discovery admits a leaf symlink when the resolved
target is a regular file. Ancestors of that leaf, including the global
directory itself, remain ordinary directories. Project, explicit, managed,
pack, and trust sources still reject symbolic links. Provenance records the
canonical target path.

## Consequences

- Positive: Home Manager and similar tools can manage user-global
  `config.ron`, `config.d/*.ron`, `tui.ron`, and `themes/*.ron` without copying
  files into place.
- Negative / risks: a writable global directory can still be pointed at an
  unexpected regular file. Literal secrets in a global file continue to require
  a private mode on the opened target.
- Follow-ups: none. A Home Manager module is not required for this path.

## Alternatives considered

| Alternative | Why not (now) |
| --- | --- |
| Accept any resolved regular file, including project sources | Weakens the workspace trust boundary the issue asked to keep |
| Nix-store path exception | Couples QQ to one store layout; Home Manager is not the only symlink manager |
| New environment variable or config path | Duplicates XDG global configuration for one installer |
| Document `QQ_CONFIG_CONTENT` only | Works, but is not an idiomatic user-global file |

## Evidence / references

- `crates/qq-config/src/loader.rs` (`allows_leaf_symlink`, `discover_file`)
- Tests: `accepts_global_leaf_symlink_to_a_regular_file`,
  `accepts_global_fragment_leaf_symlink_to_a_regular_file`,
  `rejects_project_leaf_symlink_sources`, `rejects_global_directory_symlink`,
  `rejects_symlink_sources`
- GitHub issue #83
