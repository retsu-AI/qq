# Develop QQ

Use one isolated branch for a bounded QQ change. Keep the real workflow,
independent review, adoption, and rollback separate.

## Start from the source

Read `AGENTS.md`, `docs/plans/workflow.md`, the relevant design document, and
the plan slice before changing a boundary. The architecture document describes
the current crate ownership. `docs/guide/` documents user behavior. Design
documents describe the system as built, while plans describe future work.

Create a branch from the current base and keep one writer for each mutable
artifact. Do not copy session stores, credentials, or private diagnostic
transcripts into a public change.

## Follow the development loop

1. Name the user journey and the exact source entrypoint.
2. Reproduce the behavior on the affected surface.
3. Make the smallest source change that addresses the cause.
4. Run the narrowest meaningful check, then the repository checks required by
   the change.
5. Have a non-author review the exact final revision.
6. Run the real workflow that the change claims to support.
7. Adopt the candidate explicitly or roll it back with the previous revision.

The repository's normal checks include formatting, workspace tests, Clippy,
and a workspace build. Documentation work does not require running those Rust
suites. A documentation build checks the website artifact instead.

## Keep evidence separate

A passing test proves the assertion that it exercises. It does not prove a
provider call, native UI, hosted deployment, payment, or customer acceptance.
Record the revision, command, observed result, and limit for each claim.

For a release or PR, follow the repository's existing [release runbook](../runbooks/release.md)
and [website runbook](../runbooks/website.md). Merging and deployment require
their separate authority and readback.
