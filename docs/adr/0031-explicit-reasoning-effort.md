# ADR-0031 — Explicit reasoning effort belongs to the compiled plan

**Status:** Accepted
**Date:** 2026-09-18

Configured effort is a provider-neutral request choice, independent of Jev.
Resolve it through trusted configuration and profiles, with explicit runtime
choices taking precedence. Preserve it through plan compilation and every model
turn; omission continues to use provider defaults. Explicit `none` is distinct
from omission. Unsupported adapter families reject it before credential lookup.

Descriptor version 8 records optional `reasoning_effort` and changes the digest
domain, so different choices cannot share plan identity. No store migration or
new provider transport is needed: the existing provider effort field owns wire
encoding and retry preservation. Automatic selection remains the separate J6
routing implementation and cannot override a pinned effort choice.
