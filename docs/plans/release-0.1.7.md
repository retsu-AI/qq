# v0.1.7 release preparation

## Status and scope

Preparation only; tracked by [ENG-1054](https://linear.app/retsu-ai/issue/ENG-1054).
Reviewed main: `0b4f29e3` on 2026-10-15 (rebased after #315/#317 merged).
This is not a published prerelease, a version bump, or permission to tag.
The manifest, lockfile, generated changelog and guide install pins remain at
v0.1.6 until their respective release steps.

**Not ready to tag yet.** Main's hosted correctness, smoke, Windows teardown,
WASM and website checks pass, but release performance/size qualification is
not complete. The CI job called `release build size` builds and reports bytes;
it does not enforce the 48,000,000-byte default or 41,000,000-byte minimal
budgets in `benchmarks/perf/budgets-v1.json`.

## Merged release scope

| Area | On main | Qualification boundary |
| --- | --- | --- |
| Side questions | #266/#270: isolated durable `/btw` execution and TUI | Not goals or continuation; see `design/side-questions.md` |
| Runtime reliability | #278 reset bounds; #294 tolerant read arguments; #296 recover transient summarizer failures | AC1–AC3 performance, AP5 and integrated seven-day reliability remain open |
| Tool safety/usability | #281/#283/#285: exact-match freshness, shell classification and built-in hints | T13 paid ablations and post-landing observation remain open |
| Action bias | #301–#305: verified process context, prompt/read window, authority-aware tools, root stall notice | Named performance and AB6 outcome targets remain unqualified |
| Qualification tooling | #297 recorder; #306 execution-context/export/recount tooling (`9868038c`) | Instrumentation is not proof of a successful historical or seven-day cohort |
| Developer feedback | #282 affected/full checks, lint, nextest, smoke and skills; #311 cancellation test audit; #317 feedback-gate integrity | Remaining AF6 modules and operator merge-queue activation are not complete |
| Client status | #289 herdr reporting | Not a desktop or remote-client release |

## Pre-bump gates

- [x] Merge #315's release-profile and performance-gate repair (ENG-834),
  merged as `21332992`. Final source `3dc36557` records default 44,849,176
  bytes and minimal 40,778,584 bytes, within unchanged ceilings. Correctness
  passes, but audited H0 reports have 18 A/B and 20 A/A failures with host
  pressure above the documented threshold. Merge does not waive qualification;
  unchanged H0 on an exclusive quiet host remains required.
- [ ] Attach accepted final-head performance/size receipts, including A/B and
  same-binary A/A controls and host-pressure checks, to the owning ledgers.
  Recheck after any runtime or release-profile merge; inherited receipts do
  not qualify a different candidate.
- [ ] Merge this preparation PR after its documentation checks and review.
- [ ] Freeze the candidate and run `cargo xtask check --full` and
  `cargo xtask smoke`; record exact commit, counts, skips and failures.
  Main's [hosted check](https://github.com/retsu-AI/qq/actions/runs/38688545712)
  is useful baseline evidence, not the later bump PR's verification.
- [ ] Recheck main, open PRs and contract constants immediately before the
  bump; if scope changes, update this checklist and the Upgrading block.

AP5's 2026-10-06–13 productivity window has elapsed, but no completed result
is established here. AB6 needs seven full UTC days on a deployed pinned
runtime with writer provenance; no deployment/window start is inferred from
merge time. CX5 compaction latency/cache-share, T13 tool efficiency, D6b
worker/default selection and integrated reliability are still unqualified.
They constrain release claims and default changes; this preparation does not
waive them or claim that every roadmap gate blocks an ordinary patch release.
Paid runs still need ENG-809 authorization, credentials and a spend cap.

## Open PR disposition at review

| PR | Disposition for this preparation |
| --- | --- |
| #315 | Merged as `21332992`; size/correctness pass, but accepted H0 performance evidence remains a release gate |
| #308 → #309 | G0/AC4 and G2 checkpoints in review, not merged; full goal acceptance remains incomplete. Excluded from the reviewed scope, not silently treated as shipped |
| #313 → #314 | Supervised server/login foundation and desktop client are unmerged; optional separate scope, not a prerequisite for this CLI release |
| #293 | Preparation-deadline candidate is a draft on a compatibility branch. Current-main reproduction and focused port/review required before inclusion |
| #287/#299 | Retained-installer candidates; consolidate and qualify separately. They do not solve macOS Keychain re-prompts; trusted signing remains ENG-1046 |
| #292/#298 | Search-array preservation fixtures, not accepted fixes; failing checks are not inherited release evidence. Establish a residual reproduction after merged AC17 before proposing a fix |
| #247/#248/#249/#277 | Skill/planning work, not binary release prerequisites; reconcile before any separate merge |
| #166/#170 and #167/#173 | Held Jev/client stacks; excluded, not implied complete by this release |

## Upgrading block to add in the bump PR

These values are checked against v0.1.6 and reviewed main, not against
unmerged goal branches:

| Contract | v0.1.6 | Reviewed main |
| --- | ---: | ---: |
| `PROTOCOL_VERSION` | 31 | 32 |
| `CAPABILITIES_VERSION` | 1 | 1 |
| `DESCRIPTOR_VERSION` | 13 | 15 |
| `STORE_SCHEMA_VERSION` | 43 | 50 |

The hand-written `### Upgrading` block under the generated release heading
must include these deltas, coordinated client/server upgrade and restart,
and forward-only store migration. Stop every QQ process and back up the
actual data-directory `sessions.sqlite3` together with adjacent WAL/SHM files,
or use a consistent SQLite backup. Locate the directory using
`qq config paths`; do not create a relative-path database by mistake.
Rollback needs the old binary and pre-upgrade backup, not the migrated store.
The v0.1.7 Upgrading block must restate the complete backup and safe rollback
procedure, including absolute source/backup paths for SQLite's consistent
backup alternative. Do not rely on the previous changelog section: the release
workflow publishes only the new release section to GitHub users.

Describe authority-filtered tool declarations, process read-approval behavior
and exact replay/root notices without promising measured productivity gains.
A default-empty approved-read-image manifest means command spelling alone
never proves that a process was a read. Do not claim goal support, a trusted
macOS signing fix, remote enrollment, or a desktop app from unmerged PRs.

## Handoff after this PR merges

1. Fetch current main and reassess all unchecked gates above. Complete them
   before treating the candidate as release-ready.
2. From clean, up-to-date main, create `chore/0.1.7` and run
   `cargo xtask release 0.1.7` following
   [`../runbooks/release.md`](../runbooks/release.md). Review generated notes
   and add the Upgrading block; do not hand-generate the changelog here.
3. Open the separate `chore(release): v0.1.7` bump PR, verify its final head,
   and merge only after review and green checks.
4. Only on the merged, clean main, and with release authorization, run
   `cargo xtask release --tag` and push `v0.1.7`. Never tag this preparation
   branch or the unmerged bump branch.
5. Verify all platform assets, checksums, install script and Homebrew route.
   Only after assets exist, open the guide-pin follow-up with
   `cargo xtask release --docs` and refresh example commit/date outputs.

The exact release procedure, failure recovery and contract policy remain in
the release runbook. After publication and the guide handoff, remove this
plan and its ledger, their plan/progress index entries, and the temporary
preparation link in `docs/README.md`; preserve durable instructions in the
runbook. This avoids leaving links to deleted release-preparation files.
