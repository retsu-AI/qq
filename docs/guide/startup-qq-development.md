# Develop QQ

Use one isolated branch for one bounded journey. Keep implementation, review,
runtime evidence, adoption, and rollback as separate states.

## Read the source boundary

Before changing a behavior, read [AGENTS.md](https://github.com/retsu-AI/qq/blob/9bae6937b50a013f652db8963ac3590103eb4b84/AGENTS.md),
[docs/plans/workflow.md](https://github.com/retsu-AI/qq/blob/9bae6937b50a013f652db8963ac3590103eb4b84/docs/plans/workflow.md),
the relevant architecture/design page, and the plan slice. The public source
snapshot is QQ origin/main `9bae6937b50a013f652db8963ac3590103eb4b84`.
Source paths in a private candidate require repository access; do not publish
session stores, credentials, account identifiers, or diagnostic transcripts.

The CLI ownership is visible in
[src/cli.rs](https://github.com/retsu-AI/qq/blob/9bae6937b50a013f652db8963ac3590103eb4b84/src/cli.rs),
the agent and run state in
[crates/qq-core](https://github.com/retsu-AI/qq/tree/9bae6937b50a013f652db8963ac3590103eb4b84/crates/qq-core),
and the local server boundary in
[crates/qq-server](https://github.com/retsu-AI/qq/tree/9bae6937b50a013f652db8963ac3590103eb4b84/crates/qq-server).

## Development loop

1. Name the user journey, source entrypoint, expected effect, and rollback point.
2. Reproduce the behavior on the affected surface before editing.
3. Make the smallest change and preserve unrelated work.
4. Run the narrowest meaningful check for the touched crate or guide.
5. Have an independent reviewer inspect the exact final revision.
6. Exercise the real supported journey, including timeout, cancellation, and
   cleanup when those paths are part of the claim.
7. Adopt the candidate explicitly, or retain the prior revision and record why.

For a QQ source change, the normal Rust checks remain formatting, workspace
tests, Clippy, and the workspace build. Separately, the website runbook
uses `nub run build` for the docs-truth and static-site path; its checks parse
command examples and compatibility labels across the guides. This
documentation-only delivery is scoped to guide sync and site checks, so no
Cargo suite was run. A green check still does not establish provider,
deployment, payment, or customer acceptance.

## Evidence and delivery

Record revision, source paths, command, expected result, observed result, and
limitations in the existing plan or ledger. Link to the existing detailed
[release runbook](../runbooks/release.md) and [website runbook](../runbooks/website.md).
A merged branch, deployed binary, or opened PR is a separate delivery state
from a reviewed candidate.
