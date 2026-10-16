# Ledger — Goals

Plan: [`../goals.md`](../goals.md). One writer per file.

| Slice | Goal | Status | Linear | Branch / PR | Notes |
| --- | --- | --- | --- | --- | --- |
| G0 | The goal PR: protocol, store, goal in runs, `update_goal`, and autonomous-core AC4 | In review | [ENG-1045](https://linear.app/retsu-ai/issue/ENG-1045) | [#308](https://github.com/retsu-AI/qq/pull/308) | Foundation checkpoint; not on main, full goal acceptance incomplete |
| G1 | — | Dropped (combined into G0) | | | No separate slice |
| G2 | The driver | In review | [ENG-1053](https://linear.app/retsu-ai/issue/ENG-1053) | [#309](https://github.com/retsu-AI/qq/pull/309), stacked on #308 | Implemented checkpoint only; full G2 acceptance remains open; not shipped |
| G3 | Completion check | Planned | | | Depends on G2; owns full checked Goal 1 |
| G4 | Surfaces: `/goal`, `qq run --goal`, `qq goal` | Planned | | | Read-only panel after G0; execution after G2/G3 |
| G5 | Evidence | Planned | | | ENG-809 |

## Entries

### 2026-09-28 — plan opened

This is a design PR only. The reference survey is in
`../../research/goal-pursuit.md`. Of the reference harnesses,
only Codex has a real goal. OpenCode, Pi and fx have pieces: a todo list
that is never re-injected, a finish-turn hook, and a stop hook that fails
open.

ADR-0049 was still Proposed, so it was revised in place instead of
superseded. The first draft's run-chain goal became a session goal that
only a client can create. The revision adds:
- a runtime-owned goal driver;
- goal budgets enforced through `RunLimits`;
- an optional check command that the runtime runs;
- durable waits and backoff;
- pause-on-cancel as a runtime rule.

The loop guard in § 8 is unchanged. Autonomous-core AC7–AC9 move to this
plan as G0–G3.

### 2026-09-28 — self-review before opening the PR

Before opening, an independent read-only review checked the design against
source and found 12 problems. All were fixed:
- **The check is a `GoalCheck` run.** It holds the active slot, can be
  cancelled, uses a dedicated runner (the shell tool caps timeouts at
  600 s, `tools/shell.rs:23`), and makes no sandbox claim.
- **Unanswered approvals pause the goal** after `approval_wait` instead of
  using up the deadline.
- **Goal accounting lives inside `settle_run`,** so every settlement path
  charges it, including recovery (`settlement.rs:1061–1107`).
- **`AutoContinue` skips goal runs,** and goal successors take the goal
  clamp and the goal block as their first message.
- **A `NoProgress` pause gets a fresh run,** never a continuation.
- **Limits are clamped at start,** because they are fixed at insert today
  (`commands.rs:598–600`). User prompts are clamped too.
- **Audit rules:** only goal-origin runs can claim a status, and an
  exhausted audit allowance blocks the goal.
- **Stale claims** are rejected or merged, and an interrupting prompt does
  not pause the goal.
- **Descriptor and prefix claims** were restated honestly.
- **The size bound** now includes the check tail (16 KiB).

### 2026-09-28 — Codex review on #226 (17 comments on `ccc8b47`)

All 17 were checked, and none were file-list comments. All 17 are fixed.
- The design faults:
  - the check takes a workspace-wide exclusion, since other sessions can
    run in the same workspace (`tools.md:1440–1442`);
  - the check is authorized by an exact-command held approval (a server
    command has no `--allow-shell` caller, `src/cli.rs:207`);
  - pause, replace and clear cancel queued work and ask running work to
    cancel;
  - `update_goal` is dispatched serially (`lib.rs:3372–3396`);
  - the approval wait is clamped to the deadline, and its pause takes
    precedence;
  - stale check results are ignored;
  - a check interrupted by a crash pauses the goal instead of re-running;
  - `session_goals` keeps history rows;
  - the last run under the cap gets a wrap-up;
  - runs with no committed turn don't count toward the cap, so an outage
    only uses up the deadline;
  - a user prompt cancels an unclaimed goal run;
  - `qq run --goal` returns on any non-`active` state;
  - managed ceilings can only be lowered;
  - G0 and G1 merged into one goal PR that includes AC4;
  - the stale AC7–AC9 ledger rows and the § 4 → § 8 references are fixed.
- None were declined.

### 2026-09-30 — #226 latest review closure (12 comments on `fb1e510`)

Worktree `.worktrees/pr-226-review`, branch `fix/eng-982-goal-review`, based
on PR head `5ebde18`. Design only; no Rust, wire, schema or runtime changed.
All 12 findings are valid at that head; the proposed ADR and plan now specify:
- explicit once/for-goal check consent, never a widened ordinary approval;
- one deadline for check exclusion/process; stale status/wait and results fenced;
- exact identity/revision CAS, immutable achieved goals, explicit stopped-goal resume;
- cancellation retains ownership; early resume rejects; archived spend remains charged;
- fresh goal recovery, not prompt-chain continuation; queued checks yield to prompts;
- final-slot handoff for prompt runs too; checked Goal 1 gate moves from G2 to G3;
- workflow-valid dropped statuses and synchronized dependencies/protocol shapes.
Two read-only source reviews confirmed approval, continuation, cancellation
and accounting contracts; final consistency review and verification follow.

### 2026-09-30 — final review and verification receipt for #226

- Verdict: **Approve for the proposed docs-only design** after independent recheck.
- Every spec edit invalidates its audit; neutral controls retag only valid audits.
- `cargo test -p qq docs_truth`: 5 passed in this worktree.
- `cargo fmt --all -- --check`: passed.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: passed.
- Initial unrestricted workspace tests: 5 deadline failures; no Rust changed.
- `cargo test -p qq-core --lib sessions::tests::deadlines -- --test-threads=1`: 12 passed.
- `cargo test --workspace -- --test-threads=4`: passed; original timing failures disclosed.
- `cargo build --workspace`: passed. Cargo gates share `../../target`, not source changes.
- Full-PR relative links, fences, ledger status checks and `git diff --check`: passed.
- No runtime/hot-path changes; provider minimal profile and performance benches not applicable.
- ADR-0049 remains Proposed; lead decisions are in `decisions-needed.md` row 11.

### 2026-10-06 — delivery stack planned

- ENG-1011 tracks SQ0/SQ1/SQ2 before G0/G2/G3/G4/G5; no goal implementation claimed.
- Immediate-parent PR bases, retarget after merge; G0 retains AC4 and one bump.
- Side questions remain isolated from goal accounting and workspace check authority.

### 2026-10-13 — G0/AC4 implementation begins

- Parent ENG-1011 remains delivery tracking; ENG-982 remains completed design. User's full autonomous-core stack owns isolated `.worktrees/autonomous-goals` at recorder #297 (`a775cf26`); predecessor PRs are open, not represented as merged.
- Read workflow, architecture and G0/ADR0049 requirements. Root reserves protocol33/schema46/descriptor14 and owned paths. G0 includes durable client controls, claim/tool/notices/replay and loop guard in one PR; driver/check execution follows G2/G3.
- AC3-source raw diagnostics and saved binaries precede runtime changes; isolated baseline recording38488091601 is live, no performance qualification yet. First contract test covers goal creation, idempotency, snapshot/reopen and stale identity/revision fencing. T13 ablation and integrated/live goals remain separate required gates.

- First G0 public-command regression runs and fails at `set_goal` decoding (unknown variant), proving the command is absent on the current baseline. It covers create/replayed CommandId, no driver execution, snapshot/reopen identity, stale id and revision despite replace:true, and paused edit preserving stopped state. Raw `g0-red-client-goal.log` under root AC3 evidence directory. Test compiles with existing production APIs; implementation follows this red commit.
- Recorder predecessor #297 has verified Codex opening +1 (reaction566248527, 2026-10-14T00:07:41Z), no inline findings. Its isolated run38488091601 remains live in separate optimized builds; Windows teardown and client wasm jobs already pass. No AC3 performance result inferred.

- Independent AC4 source audit proves checkpoint slices have at most256 executable admissions and may close at241. Literal slice-local seen reset cannot pause alternating A/B; two checkpoints do not imply512calls. Decision13 records a proposed exact bounded current/previous observation-window contract, with512 quiet attempts after novel warm-up, report/over-cap excluded, guard rejections counted. Proposed ADR0049 clarification is explicit; repeat thresholds, mutation reset, distinct reads and T13 acceptance remain. G0 control implementation proceeds while this clarification is reviewed.

- Independent re-review approves the bounded-window model; synchronized AC4 acceptance and reset-scope table, with explicit eligible admission/result boundaries and exclusions. Denied/malformed ordinary calls consume quiet attempts without inventing executed triples; interrupted attempts stop via their existing terminal path. Decision13 remains open for lead acceptance; no default shipment or accepted ADR claimed.

- G0 first client contract is green: protocol33 adds goal identity/states/commands/events, schema46 retains one live row plus history with validated table/index shape, and store-transaction controls preserve CommandId replay and exact generation fencing. Snapshot/reopen/paused-edit test passes. Typed domain validation errors and client snapshot/event state are wired. Separate `target/qq-goals-check` uses command-scoped debug0/incremental0 to avoid shared build-cache aliasing and disk pressure; no global Cargo setting changed.
- This is local partial G0 implementation, not a ready PR: claim/model tool/notices/loop guard, accounting/cancellation integration, full boundary fixtures/consumer qualification and review remain. AC3 validation independently hit ENOSPC after both corrected fixture profiles passed; restored the failed owned-file write and cleared three inactive generated core incremental-cache entries. AC3 minimal suite then passed with incremental0; full workspace requalification is live. Recorded source/fixture evidence stays untracked under root AC3 directory.

### 2026-10-13 — G0 exact check policy admission

- Test-first5a798e0 adds the configuration-facing exact-check policy seam and reproduces managed-denied authority being granted. Core prefetches policy only for explicit ApproveCheck, off the SQLite worker, carrying success/failure into the existing command admission. Receipt replay precedes policy errors; fresh commands enforce goal CAS/exact command/static Forbidden before policy, activation and authorization mutation. Existing grant authorities fail closed unless they implement check policy; no-authority embedding explicitly has no managed layer.
- Five focused goal tests pass, including denial/unavailable policy without state/event mutation, committed replay after both policy changes and missing session, and IdempotencyConflict despite policy failure. Independent core-seam review Approve. Raw `g0-{red-check-policy-fresh,green-check-policy-missing-session}.log` under root evidence directory. Cached AC3 protocol artifacts caused an invalid initial compile; owned protocol/core source timestamps refreshed without content edits and the actual behavioral red/green recorded separately.
- This remains local partial G0: RuntimeFactory managed-deny mapping and bounded shell matching, claim/RunOrigin, serial model tool, verbatim notices, loop guard and full consumer/workspace/performance qualification remain before one ready G0/AC4 PR. No check executes and no whole-plan/default-shipping acceptance is claimed.

- G0 policy core implementation31d0d37 committed locally; merged qualified parent review repairs in4d3a03c without source conflicts. Focused post-integration goals suite5 passed and inherited four-state recovery accounting test1 passed. Logs `g0-parent-{goals,accounting}-check.log`. Root mapping/matching and full G0 remain open; no G0 PR/push or full-workspace qualification claimed.

### 2026-10-13 — G0 managed-deny mapping and bounded parsing

- Test-first5de46e6 adds wrapper/member/unresolved matcher and current-config authority contracts. Matcher and root mapping now pass30 approval tests and2 root policy tests, including config edits, malformed policy, oversized command and the lazy four-permit blocking limit. Root server command/error handling and advisory snapshot fixture were updated for the existing G0 wire shapes. Logs `g0-{green-managed-deny-bounded,green-root-check-policy-bounds}.log`; earlier missing-model fixture/configuration and consumer compile failures retained separately.
- Independent review confirms bounds/current-config mapping but finds concrete managed-deny gaps: bash --rcfile payload selection, Python inline code, fish and BusyBox dispatch. Those are open repairs, not qualified policy authority. No G0 ready PR/default shipment claimed; claim/tool/notices/guard/full qualification still remain.

### 2026-10-13 — G0 managed-policy review corrections

- Long shell options no longer select `-c` payloads in the goal parser; versioned interpreter/Windows executable dispatch is refused or matched conservatively. Independent review additionally reproduced `env -C ./ cargo test` and `xargs -a /dev/null cargo test` bypassing managed Cargo denies; unsupported env options and opaque xargs input now refuse. All30 approval tests pass; ordinary classifier behavior is unchanged.
- Corrected an actual composition-root restriction previously treated as a fixture requirement: managed policy validation uses `load_for_client`, so a workspace with a selected session model need not configure a global default model. The no-global-model public authority regression fails before and passes after; both root authority tests pass. Raw `g0-{red,green}-policy-without-default-model.log`, `g0-{red,green}-wrapper-option-review.log` under root AC3 evidence directory; previous failure logs retained.
- Still local partial G0/AC4, no PR/push/default shipment or full-workspace qualification. Further static Forbidden-floor inputs and wrapper review, claim/tool/notices/guard/accounting and G0 boundary fixtures remain. Latest parent client/settlement review repairs are being qualified separately before integration.

### 2026-10-13 — G0 static Forbidden floor repair

- Concrete static classifier regression: `bash --rcfile /dev/null -c 'sudo true'` receives Prompt, allowing the goal-check Forbidden floor to miss sudo when managed prefixes are empty. Test-firstc4ca10e covers rcfile/norc/noprofile; ordinary and goal collectors now select only short `-c` flags. This intentionally corrects ordinary goal-less classification too; no allocation or additional parsing pass. All31 approval tests pass; independent review Approve.
- Pre-change production source3eacc81 and post-change classifier binaries are preserved with SHA256/compiler/profile metadata. Loaded-host unoptimized dev diagnostics (10,000 iterations) record1KiB709,570→686,641ns and unchanged existing verdicts; this is not optimized200µs/H0 qualification or an attributed speedup. Raw `g0-static-floor-*dev.*` and `g0-{red,green}-static-forbidden-shell-options.log` under root evidence directory.
- Still local partial G0/AC4 with no PR/push/default-shipping claim. Claim/tool/notices/guard/accounting, full boundaries/workspace and optimized/performance qualification remain; latest qualified AC3/recorder parent integration follows separately.

- Integrated qualified parent6383c746 into local G0 atb6aad76. Three inherited client SessionSnapshot fixtures needed explicit goalNone; after this consumer correction all34 client tests pass. This is post-integration qualification, not full G0 completion; core goals/static policy checks follow before further source work.

### 2026-10-13 — G0 immutable prompt claim snapshot

- Test-first queued-prompt matrix covers active, paused, cleared and absent goals at reservation, plus replacement/new goal before authoritative start. New contract is red because ClaimedRun has no goal snapshot; initial unrelated test cleanup typo is corrected and preserved separately.
- Capture the active goal in the existing reservation query, without an extra SQLite hop; persist its id/revision/full snapshot only in the FULL start transaction. Preparation never reloads a replacement. Internal compaction and historical synthetic cleanup claims do not gain goal mutation authority. This remains local partial G0; no tool, notices, driver or ready PR is claimed.

- Immutable claim matrix now covers active/paused/cleared/absent plus pause after reservation, replacement/new-goal before start, and reopened historical bindings. Schema45→46 preserves goal-less runs with all three nullable claim fields, including repeated reopen. All7 goal tests pass (`g0-green-goal-claim-and-migration.log`), independent claim review Approve; synthetic abandoned-summary cleanup also clears capability. Source commits remain local; origin/model tool/notices/loop/render bounds/full workspace and optimized qualification still open.

### 2026-10-13 — G0 immutable provenance and wire fixtures

- Protocol test-first4ca36d9a distinguishes creation origin from optional goal binding and preserves unknown historical provenance. Empty struct variants make serde reject extra fields for user/manual origins. RunSnapshot exposes copyable origin and a small immutable GoalIdentity separately; full claim snapshot remains stored internally. Protocol51 unit+3 headless+5 wire tests pass with fixture rewriting disabled, v33 goldens added, v32 historical files untouched (`g0-origin-wire-fixtures-readonly.log`). Initial fixture-generation race is retained; final read-only verification passes.
- Test-firstd7c42356 reproduces missing UserPrompt provenance before claim. All five existing creators now persist exact origin JSON at enqueue/start. Nine goal tests pass, including delegated task/audit, user followup in child, manual/between/in-run compaction ownership/foreground semantics and reopen (`g0-green-all-origin-creators.log`); independent provenance review Approve. Creation data never infers history from command absence or mutable parent relationships.
- Claim-origin forwarding, start/terminal event metadata, client consumers, model tool/notices/guard and full/optimized G0 qualification remain open. Goal/GoalCheck variants are wire shapes without a queued driver/check executor. No ready G0 PR or default shipment claimed.

### 2026-10-13 — G0 scoped model writes and rendered size boundary

- Test-firstbdd9f640 fences model writes by immutable claim, durable running/cancellation state and exact origin. Checklist/notes merge after client edits; stale status/wait cannot control the new revision. Only goal-origin runs propose controls; achievement retains run/revision/evidence as an audit proposal, never verified success. Client edits clear proposals. Stopped/replaced/terminal/forged claims refuse all data.
- Independent review caught draft64×256 checklist bounds conflicting with ADR0049. Concrete121-byte acceptance red is preserved; source now enforces24×120. Waits cap86400s and clamp to the goal deadline; serial store edits retain prior data; conflicting controls reject. Injected event-insert failure rolls record/cursor back; successful model state and exact event survive reopen.
- Shared notice renderer preserves verbatim objective/check/notes/tail and checks each save against16KiB with reserved4KiB check tail. Maximum notice contract first fails on missing renderer, then passes. All21 focused goal tests pass (`g0-green-goal-render-final-backend.log`), including accepted blocking and refusal of further writes; independent backend/rendering re-review has no source blockers; raw reds and narrower greens remain under root `target/qq-perf/ac3-stack-2026-10-13/`.
- This is local backend/rendering progress, not a complete G0/AC4 or PR. Runtime tool exposure/serial dispatch, notice insertion/revision/compaction seams, loop guard, request goldens, full workspace and optimized/live qualification remain. Unwired private backend produces dead-code warnings; none are suppressed. Latest qualified stack integration follows after these owned source commits.

- Backend source de0831df plus test-firstb15a863c: all21 goal tests pass and all-target workspace compilation passes (`g0-check-goal-render-backend-workspace.log`). Unwired backend/notice functions produce17 dead-code warnings; runtime wiring is required before full Clippy qualification. Worktree clean at checkpoint, no push.

### 2026-10-13 — G0 initial runtime goal capability and serial tool dispatch

- Test-first799e656d reproduces missing claimed-goal tool exposure. Concrete GoalRunInput carries only the immutable snapshot, origin, identity and scoped writer. Catalog include flag exposes the tool to claimed prompts; goal-less common specs and schema measurement stay cached. Descriptor13→reserved14; protocol remains the single reserved33 bump.
- Initial framed notice and accepted prompt are an irreducible prefix. Dedicated goal dispatch needs no workspace approval, retains durable claim/origin/revision guards and fails the run on persistence errors. All23 goal tests pass after disk space recovery; hallucinated goal-less edits refuse, injected goal-event failure leaves no accepted tool write. First all-goal compilation failed with ENOSPC; logs retained.
- Mixed read/goal batch regression reproduces edits jumping ahead of an earlier read. Goal edits now remain in the ordered suffix, never the parallel read batch. The tree-before-edit cursor regression passes together with dependent add/set-state calls and runtime failure tests (`g0-green-goal-runtime-mixed-order.log`). Independent initial source review finds no authority/source blocker; mixed-order repair re-review and production-encoder exact-byte comparison follow.
- Still local incompleteG0/AC4: post-summary notice placement, revision refresh, ≥3-compaction acceptance, loop guard/default ablation, full workspace/performance and latest stack integration remain. No PR/push or default-shipping claim.

- Independent re-review finds goal dispatch skipped the ordinary interrupt boundary. Test-first53a451d0 reproduces obsolete suffix notes overwriting an accepted goal prefix after an interrupting steer. Dispatch now checks the generation before admitting each goal write, preserving admitted atomic writes and stopping the remaining suffix. Both read-first and goal-first boundary cases pass; all24 goal tests pass/0fail/1manual capture ignored (`g0-green-goal-runtime-interrupt-suffix.log`). Final repair review and byte-parity capture follow. The capture's initial1-call limit gave an immediate final turn; corrected16-call reservation permits the actual tool turn plus tools-disabled final turn, without changing production budgets.

### 2026-10-14 — G0 exact request bytes and interruption-aware admission

- Baseline26b14b81 and candidate00788838 use identical capture source/workspace and production Responses/Chat/Anthropic/Google encoders. All8 bodies are byte-identical (initial request plus real tool-result/finalToolChoiceNone request), lengths15710–17095B. Raw bodies, SHA256 receipt and common fixture under root `target/qq-perf/ac3-stack-2026-10-13/goal-less-request-*`; core cache explicitly cleared before baseline capture to prevent stale candidate reuse. This qualifies initial wiring's request parity, not later notice/guard/integration source.
- Independent review identifies a second steering gap while AwaitControl waits for its permit. Test-first6038a001 saturates all256 permits, polls updatePending, steers then releases capacity: old source commits obsolete notes. GoalRunInput now returns the existing store's owned admission receipt; biased steering applies only during admission wait, then durability is awaited. Remaining suffix stops through existing cleanup. Positive dropped-receipt fixture preserves the already-admitted write.
- All26 focused goal tests pass/0fail/1manualcaptureignored (`g0-green-goal-admission-owned-and-interrupted.log`); independent repair review has no blockers. Clippy core/all-target/all-feature is running; whole-workspace qualification remains later.
- Fresh stack reconciliation confirms AC17 is on main7de994b1 (merge parentf3b76376), but main's schema45 adds approval_delegated and collides with AC3's schema45 owner index. AC3 must preserve main45 then add46; local unshipped G0 reservation must move46→47 on integration. No stale source is pushed; main/client/TUI/migration reconciliation and final requalification remain. Post-summary/revision notices, ≥3-compaction acceptance and AC4 remain incomplete.

- Core all-target/all-feature Clippy `-D warnings` passes (`g0-clippy-goal-runtime-admission-clean.log`). Initial lint catches the test's request MutexGuard scope; lexical block now ends before async snapshot/shutdown, with no lint suppression. Production admission source is unchanged from the26-test green; formatting/diff checks pass. Final source commit and tracker update follow; no push or complete-G0 claim.

### 2026-10-14 — G0 integration with main45 and AC3 schema46

- Merge AC3d1d7796c, preserving main7de994b1 approval holds and Herdr. Goal migration moves to47; protocol33/descriptor14 stay the single reserved bumps. Incoming client/TUI snapshot and run-event fixtures gain their optional goal/origin fields. All-target workspace compilation passes (`g0-main47-integration-consumers-fixed.log`).
- Migration suite55 passed/0failed/1ignored and focused goals26 passed/0failed/1manual capture ignored (`g0-main47-{migrations,goals}.log`). Independent focused migration review Approve: main45 delegated values survive, index46 stays separate, goals47 rolls back to46 on failure, current-schema reopen validates shape.
- Doctor sample uses build placeholders and current33/14/47 compatibility. Full workspace, final byte parity and performance remain before publication; post-summary/revision notices, three-compaction acceptance and AC4 remain incomplete. No G0 PR or shipping claim.

### 2026-10-14 — G0 post-summary goal placement regression

- New maximum-field fixture runs72 real shell turns and at least three durable in-run compactions. It reproduces the missing immediately-following goal notice (`g0-red-post-summary-goal.log`), after the run completes successfully. Objective/check/checklist/notes/check tail are at their respective bounds.
- Repair must put the bounded claimed notice immediately after each summary and charge it in the existing window byte measurement. Revision refresh and full G0/AC4 remain separate unfinished acceptance.

- The captured notice now follows each summary in the same splice; goal-less splice stays unchanged. The maximum-field72-turn/≥3-compaction regression passes (`g0-green-post-summary-goal.log`), focused goals26 pass/1manualcaptureignored, existing goal-less multi-window regression passes, core all-target/all-feature Clippy passes (`g0-post-summary-*`). Formatting and diff checks pass. Revision refresh still remains before full G0 acceptance.

- Independent review finds the new notice omitted from durable `context_base_bytes`, despite live remeasurement. Extend the real-compaction fixture to compare the persisted latest rebase with the exact first following provider request:38921 persisted vs50792 actual (`g0-red-post-summary-durable-weight-compiled.log`). Initial fixture compilation omitted Option unwrap; failed log retained. Charge and reuse one constructed notice before admitting compaction. Live/replay notice parity also remains unfinished, separately from revision refresh.

- Construct and charge one bounded post-summary notice before compaction admission, then reuse that exact message in the successful splice. Exact persisted/provider-weight equality now passes through the72-turn/≥3-compaction fixture (`g0-green-post-summary-durable-weight.log`); independent repair re-review Approve and core all-target/all-feature Clippy pass. Goal-less path adds zero notice weight. Formatting/diff checks pass; replay and revision refresh remain open.
- Published AC3 parente8c52db1 has fresh Codex no-major-issues review and opening+1 reaction566977703 at2026-10-14T07:51:14Z; all13 threads resolved. G0 has no PR/push and is not complete.

### 2026-10-14 — G0 current-revision notice regression

- Client edit between ordered turns advances the live revision. New public-request regression proves no edited goal is shown in the subsequent window (`g0-red-goal-revision-notice.log`,0 notices expected1). It also verifies an old claimed goal-run wait remains rejected as stale; display refresh must never widen mutation authority.
- Use a bounded goal-only committed-row read at turn boundaries, with cancellation/deadline and same active goal identity fencing. No goal-less read/subscription. Record and measure the added goal-only read cost; exact durable initial/turn/marker atoms and replay remain before publication.

- Mutable display snapshot is separate from captured claim/origin. Goal-only AfterGroup read waits within bounded control admission and cancellation/deadline; same active ID and rendered bounds required. Append only on revision change; summary uses latest model notes even without revision change. All27 goal tests pass/0fail/1manual capture ignored, maximum72-turn/≥3-compaction placement/exact durable weight passes, core all-target/all-feature Clippy passes (`g0-green-goal-revision-notice-compiled.log`, `g0-revision-notice-*`). Initial compile's missing Priority qualification log retained.
- Saturated-write fixture takes all256 permits after both ToolCallStarted events, before polling the edit; this preserves the writer-wait interruption premise while allowing the prerequisite context read. Independent focused review Approve. Display reads are an explicit goal-only cost, not yet performance-qualified. Durable notice atoms/replay and full G0/AC4 remain unfinished.

### 2026-10-14 — G0 exact request parity and reopened-window regression

- Initial/tool-result/finalToolChoiceNone bodies from production Responses/Chat/Anthropic/Google encoders at02c3de7b match baseline26b14b81 byte-for-byte: all8 bodies,15710–17095B. Capture function body matches the common immutable fixture (outer blank lines excluded); raw receipt `goal-less-request-revision-byte-parity.json` and bodies under root evidence directory. This covers revision-display source, not future durable atoms or full G0.
- Extend maximum-goal72-turn/≥3-compaction fixture to reopen its store and compare full model messages against the actual last provider window plus completed answer. It reproduces missing initial goal notice (`g0-red-reopened-goal-window-compiled.log`); first fixture attempt incorrectly assumes Message is Serialize, then switches to its actual PartialEq. Both logs retained. This failing acceptance stays local; no G0 push.
- Root reserves exact bounded initial/turn/marker text atoms within unshipped47. Existing transactions must preserve and replay those bytes, including compaction cuts and cumulative read bounds; no regeneration from current goal or wallclock. AC4 and broader G0/plan gates still open.
- Recorder parent PR297b590823f now has fresh Codex no-major-issues review/opening+1 at2026-10-14T08:04:58Z; all3 threads resolved. Controlled recording38532450201 confirmed in_progress, job115747284688, baseline main7de994b1/candidate+fixtureb590823f. No final-head performance conclusion until raw artifact verification.

### 2026-10-14 — G0 initial/marker atoms and revision replay regression

- Initial claim text is captured once and shared through the first FULL routing/auto/main admission and runtime. Marker notice is stored with its summary and replayed after it; exact72-turn/≥3-compaction full-message reopened-window comparison now passes locally (`g0-initial-marker-replay-compiled.log`). Earlier constructor-fixture compilation failure retained. Storage/session review and broader qualification remain before commit/push.
- New live-session regression edits the goal while the first provider response is held, then finishes three requests and reopens. Actual requests contain the revision notice but replay does not (`g0-red-turn-goal-notice-replay.log`). The turn commit must carry exact pending notices alongside existing static notices, preserving retries/interruption order and bounded read/allocation. No new transaction or protocol bump.

### 2026-10-14 — publish the foundation for review

- Parent PRs296/297 are merged. Integrate main007adcda in the isolated G0 checkout, preserving #301 process-context preparation/receipts and moving unshipped goal storage48/descriptor15; protocol33 unchanged. Driver checkout and its unfinished admission work remain separate.
- Prepare an explicitly draft foundation PR against main. Quiet-host performance, T13 and driver/check/surface/full-plan acceptance are incomplete; existing loaded-host observations are diagnostic, with no waiver or ready-to-merge claim. Fresh workspace checks and focused main-integration regressions are required before push.

- Integration review reproduces lexical shell classification conflicting with main's executor receipts: six successful `cat note.txt` shell calls have Work receipts, but AC4 starts only four. Public regression `ac4_process_work_receipts_reset_repeated_successes` fails0/1 in `g0-main301-process-work-red.log`; retain the red before the receipt-derived guard repair. Descriptor16-test subset passes; integration fmt/Clippy/build pass, whole-workspace run still active for the pre-repair source.

- Receipt-derived process-work bit replaces lexical shell mutation detection, with no receipt clone, query or wire field. New regression is green; all13 loop-guard tests pass, including actual static approved-image VerifiedRead receipts and fifth-call refusal (`g0-main301-process-work-verified-read-green.log`). Independent focused source review Approve.
- First integration workspace run stops at core1067pass/1fail/4skips: main's AP verified-read report fixture issued64 identical calls, so the new guard correctly admits4. The fixture now uses64 distinct admitted argv to test its actual reporting premise while the dedicated guard fixture proves repeated-read refusal. Compile-only lifetime fixture failure retained; no production lifetime change. Final workspace qualification follows.

#### G0 publication checkpoint — 2026-10-14
Commit: b4f3645b (main301 integration8fb55d85; final receipt-derived guard repair).
Tests: cargo test --workspace2431top-level passed/0failed/15explicit skips; raw2436includes5nested isolated child results.
Checks: cargo fmt --all -- --check1.32s; workspace all-target/all-feature Clippy-Dwarnings15.50s; cargo build --workspace12.42s.
Focused: descriptor16/0; loop guard13/0; main verified-read reporting1/0 plus isolated child; independent integration/guard/fixture reviews Approve.
Evidence: target/qq-perf/ac3-stack-2026-10-13/g0-main301-final-{fmt,clippy,build,tests,report}.log; reds retained.
Compatibility: protocol33/descriptor15/schema48; preserve main47 receipts and descriptor13/14 domains.
Open: draft foundation review, current-head request parity, quiet-host/producer identity performance and T13; G2/G3/G4/G5 and whole autonomous-core incomplete.

- Published draft [PR308](https://github.com/retsu-AI/qq/pull/308) against main007adcda at f7806bf4. Codex review requested in comment6176940616; Linear ENG1045 remains In Progress with the PR linked. No hosted CI pass, Codex approval, ready-to-merge or full-plan completion claimed.

### 2026-10-14 — hosted deadline fixture phase correction

- G0 current9b0130cb controlled recording38626845913 succeeds and uploads artifact11925700849; Windows teardown and wasm pass. Linux tests fail1068/1/4 at duration_expires_during_shell_execution_and_drains_before_session_reuse: its300ms expires before any ToolCallStarted under CI load, so the test never reaches its claimed native-execution premise. This is not a production timeout failure; logs retain the exact assertion.
- The test now pauses Tokio time, waits for the executor's actual owned process-spawn hook, then advances300ms. A bounded5s real-time yield task prevents auto-advance during native/store I/O; existing exact Duration outcome, one provider request, no late write and subsequent session reuse assertions remain. Focused regression and broad qualification follow; no hosted pass or performance gate conclusion yet.

- Focused native deadline regression passes1/0 in0.31s (`g0-hosted-native-deadline-clock-fixed-api.log`); independent fixture review Approve. Regular PR CI38626787410 passes Linux, Windows and wasm. The separate controlled recording run's Linux phase premise failure remains recorded; final repair workspace checks follow.

### 2026-10-14 — current-head controlled performance evidence

- Run38626845913 perfjob116068065076 succeeds; artifact11925700849 (`turn-overhead-38626845913-1`, sha2560d72698916d76069e531829f5987a3110bc53bff38d8a345ae68e425f674995f) downloaded untracked to target/qq-perf/g0-ci-main301-2026-10-14/. Independently audited120logs/360rows,30A/B and30A/A pairs with baseline007adcda and candidate/common fixture9b0130cb.
- Turn10/100/1000 A/B median−1.927/+2.782/−0.211%; p95−8.231/+32.704/−1.645%. A/A median−0.007/+3.463/−3.561%; p95−16.104/−11.891/+1.198%. Turn100 p95 increases2.005593→2.661499ms while A/A declines2.230246→1.965042ms. Pressure0–13.38%; this tail increase is not demonstrated noise and is not waived.
- h0_registered:false means the metric is unregistered. This artifact does not qualify H0, active-goal overhead, concurrency/fsync, turn2000, producer identity costs or paid T13. These and decision13 remain open; PR308 stays draft despite genuine Codex opening+1 and regular CI success.

#### G0 native fixture repair receipt — 2026-10-14

Source: native deadline fixture only; production remains b4f3645b/9b0130cb.
Checks: isolated-target fmt1.32s, workspace all-target/all-feature Clippy16.71s, build13.24s pass.
Tests: workspace2431top-level/0failed/15explicit skips in376.08s; raw2436 includes5nested child results. Native deadline regression passes in this full run.
Evidence: target/qq-perf/ac3-stack-2026-10-13/g0-isolated-final-{fmt,clippy,build,tests}.log.
Correction: prior shared-target run reused a driver-schema50 artifact in the foundation-schema48 build and failed docs-truth271/1/2. Cleaned affected packages and permanently separated G0/G2 target directories; the failed run is retained, not qualification.
Review: independent native fixture review Approve. No production perf change; controlled9b0130cb artifact remains relevant with its unexplained100-turn p95 increase.
Open: performance/T13/decision13/current request parity; complete G2 and original autonomous-core gates remain open.

### 2026-10-14 — G0 current-main integration

- Integrating main62c65930 into foundation804948b0 on isolated fix/eng-1045-goal-main-integration. All17text conflicts resolved; deployed main47–50 migrations retained, foundation moved to51, protocol33/descriptor15 with exact projection identity. Added real goal-less claim upgrade matrix47/48/49/50 with exact notice/projection bytes, empty goal history and reopen identity; builds/tests not yet run.
- Ordered G0 call admission remains in place while main full-page reservation, typed rejection and authority filtering are retained. Driverreview qualification76330 stays isolated/live; no merge readiness or whole-plan completion claim.

- Independent focused integration review Approve: ordered admission/guard/start tracking, full-page reservation and typed refusals, authority filtering/selector replay, exact projection barrier, transcript columns5–9 plusgoal10 and deployed schema47–50 retained. Source review only; compiler and workspace qualification remain pending.

- Preserve the independently approved hanging-provider phase fixture in the foundation too: actual dispatch before paused-clock expiry, bounded real entry wait and shutdown before assertions. This is the same test-only repair68f0b0a7 being qualified in the driver; no production timeout change. Foundation qualification follows.

- First integration compile94327 stops on8missing test-fixture fields introduced independently by goals and upstream projection/authority/producer identity. No reported production-library compiler error. Fixture-edit script stopped after5initializers; second compile94813 retained3missing fields. Completed explicit empty goal atoms/counts, strict projection identity, absent goal authority and search identity on all8fixtures. No runtime semantic change; upgrade test repeats.

- Integration upgrade matrix47/48/49/50 passes1/0 in1.19s after explicit fixture fields; compiler succeeds. Exact notice/projection bytes, empty goal history and store identity survive both opens. Broader goals/migrations/Clippy checks follow before workspace qualification.

- First integration goals suite34/1/1skip: malformed-goal-notice test still treated47/48 as the goal migration boundary. Deployed main steps legitimately reach50 before the new51 goal transaction refuses malformed goal fields. Fixture now tests50→current and current-schema validation, retaining unchanged-version assertions for the goal step;47–50 successful preservation is separately covered by the upgrade matrix. No production migration relaxation.

- Integration goal suite35/0/1skip10.32s passes; independent goal-boundary rollback review Approve. Migration suite58/1/1skip8.90s fails only old receipt fixture current-version assertion51vs50 after correctly rejecting the missing receipt column. Extend it to deployed50 and current schema, each asserting unchanged metadata. No production change; migrations/Clippy repeat, goals need no repeat.

- Integration migrations59/0/1skip11.20s and core all-target/all-feature Clippy19.27s pass. Goal suite35/0/1skip10.32s and independent integration/boundary review Approve. Commit updated boundary fixtures before full workspace qualification; no publication or performance qualification yet.

- Foundation workspace75459 terminates101 in457.91s: core1138/5/4skips371.96s. Four behavior fixtures interact with newly merged policy: long-read SequenceCalls does not recognize RootStallReport and consumes4×60workcalls; AB2 page fixtures repeat identical reads that G0 intentionally guards and count only Finished instead of unexecuted settlement. Fifth is experimental48 receipt fixture hardcodedcurrent50. All failures retained. Independent diagnosis approves fixture isolation: recognize/count real root reports without consuming work; distinct admitted page queries, exact deferred query retry with page/cursor compared by call ID, all settled outcomes and actual success counts; current-version assertion. Production guard/admission unchanged. Focused validation follows.

- Final focused fixture qualification: AB2suite42/0 in7.41s; two1500-work contracts2/0 in0.15s; experimental48upgrade1/0 in0.43s. Actual-source independent fixture review Approve. Intermediate edits retained: initial missingturns local, then missingregex flag and per-provider-call page comparison; final matching calls keep allwhole-page/cursor/context/bound assertions. No production guard change. Commit before final workspace repeat.

### 2026-10-14 — G0 current-head Codex review corrections

- Parent #308 current-main review returned seven findings. Check-removal and denial state regressions reproduce0/1 (1.47s); find-executor/launcher-operand policy regression reproduces0/1 (0.00s). State transitions now retain unrelated stops; removing a nonexistent approval cause becomes PausedUser. Managed denies refuse find/fd executor actions and opaque wrapper boundaries; nice/stdbuf long operands are parsed. Independent review finds no blocker.
- Active side-context and saturated goal-write admission regressions reproduce two failures (goal filter28passes/2failures/1skip,10.15s). Frozen notice+prompt now share one SQL byte preflight. Cancellation/deadline abort unadmitted goal writes; admitted receipts still await durability. Runtime retains typed report provenance alongside ordered notices; replay avoids duplicate rendering. Independent review finds no blocker.
- Focused updated goal suite and actual truncated-report metadata/extraction/replay regression are running. Thread reply/resolution tools reject writes with approval-required/policy-never; no thread is falsely marked resolved. Hosted CI, refreshed stack integration and genuine new-head Codex reviews follow publication. No merge performed.

- Focused goal qualification passes30unit/0failures/1skip in10.32s plus enabled-goal compaction soak1/0 in3.18s. Actual truncated-report fixture passes1/0 in5.08s for goal absent/present, checking typed report provenance, positioned atoms, joined progress extraction, cancelled-child answer and exactly-once reopened replay. Initial ordered-JSON assertion needed decoding rather than raw escaped-text matching; production behavior unchanged by that test correction.
- Workspace formatting/test/Clippy/build qualification is running. First fmt check catches only new report-test formatting; rustfmt applied. No push or new-head review claim yet. Separate caller-intersection implementation is checkpointed6e86028b in paid-current and is not mixed into these review fixes.

- Parentbc7951d9 full workspace qualification finishes2,377rawpasses/163failures/15skips in432.55s. All failures are160socketpermissiondenials+3OAuthCallbackUnavailable. Build passes12.61s; corrected fmt passes. Clippy catches test-only drop_non_drop for a pinned Next reference; remove the unnecessary drop and rerun Clippy. No production change or green full-workspace claim.

### 2026-10-14 — G0 text interpreter policy follow-up

- Current driver170eed8e receives a genuine Codex thumbs-up. Parent1c988e99 has a new concrete AWKsystem managed-deny bypass. Added exact awk/gawk/mawk/nawk/path-qualified AWK and sed execution regressions; first reproduces0/1 in0.00s. Treat these command-capable text interpreters as opaque for managed goal-check denies. Ordinary classification and empty-deny behavior remain unchanged.
- Prior parent corrected Clippy16.29s and build12.61s pass. Combined driver full workspace2,433rawpasses/164failures/15skips:160socketdenials+3OAuthCallbackUnavailable+one shell-deadline fixture failure; isolated unchanged fixture passes1/0 in2.24s. Corrected driver Clippy16.71s and build12.80s pass. No full-workspace green claim or waived timing failure.

- AWK/sed follow-up focused deny tests3/0; complete command-classifier suite and formatting pass. Workspace Clippy17.73s and build13.98s pass. Independent focused review Approve. Current-head hosted checks/review will rerun after publishing this narrow opaque-family correction; no full-workspace or merge-ready claim.

### 2026-10-14 — G0 Git alias managed-policy regression

- Codex current-head review names `git -c alias.qqprobe='!cargo test' qqprobe`, accepted despite managed deny `cargo test`. Exact regression reproduces0/1 in0.00s; test-first commit precedes implementation. Variants cover path-qualified Git, environment/config-env aliases, configured external subcommands and launcher wrapping. Empty managed-deny policy remains unchanged.
- Treat Git as an opaque launcher under nonempty managed check denies; parsing only inline aliases would miss repository/global config, executable subcommands and configured helpers. Focused classifier, formatting, Clippy/build and independent review follow. No merge performed.

- Under the user's explicit `gh` instruction, installed Nix GitHub CLI successfully resolves all15 previously fixed open threads (8parent,7driver). Follow-up GraphQL confirms parent has only the new Git-alias thread open; driver has0open threads. `nix-shell -p gh` itself cannot access the daemon socket, so the already-installed Nix binary is used directly.
- Git correction classifier suite11/0, formatting/diff checks, workspace Clippy16.76s and build pass; independent focused review Approve. Full local workspace qualification remains live before publication. Published previous heads both have green hosted CI. The Git-alias thread remains open until its correction is published.

### 2026-10-14 — G0 bounded execution-surface review

- One independent pass over shell/wrapper/interpreter/configured-helper/explicit-hook/implicit-alias paths identifies tar checkpoint hooks, rg preprocessors, sort compressor commands and fd list-details execution. Tar and rg harmless probes execute their named programs; sort/fd local help documents their exact command behavior. New12-command parameterized regression first fails on tar0/1 in0.00s.
- Treat tar/rg as opaque configured launchers; refuse sort compressor spellings including GNU long-option abbreviations, and fd list-details/-l combined short flags. Controls retain ordinary sort/fd/find behavior and all empty managed-policy behavior. Focused classifier and independent correction review follow; no arbitrary-executable sandbox claim.

- Bounded policy correction passes all12classifier tests, workspace Clippy and build13.02s; formatting/diff checks pass. Independent review caught the missing long fd spelling before publication; exact --list-details added and full classifier rerun passes. No empty-policy or ordinary approval behavior changes. Earlier Git-only full-workspace run is finishing; complete current stack qualification follows integration.

### 2026-10-14 — G0 recipe and package execution family

- Current Codex review names `make --eval='all:;cargo test' all`. Harmless make recipe probe prints its execution marker; regression reproduces0/1 in0.00s (`g0-review-recipes-red.log`). Recipe/package/environment drivers now use the existing opaque-program rule under nonempty managed denies, including make variants, Cargo aliases/build scripts, package exec and environment hooks. Empty policy and ordinary approvals remain unchanged.
- Safe-control commands now use printf rather than a Cargo plugin name, because Cargo is no longer treated as transparent under managed checks. A separate literal-word/launcher regression retains direct, shell, nice and stdbuf boundary coverage. This intentionally tightens only managed completion-check authority; design amended. Focused classifier and independent review follow.
- Published driverd5deaffc final full local workspace finishes2,440rawpasses/163failures/15skips:160socketpermission failures+3OAuth callback listeners. No code failure remains in that run; this is still not a green local workspace result. Parent turn recorder38687525601 completed successfully and retained its raw artifact; metric analysis is pending, not a performance-gate pass claim.

- Recipe/package/environment correction passes all14classifier tests, formatting, workspace Clippy16.32s and build. Independent focused review Approve; no empty-policy or ordinary-approval behavior change. Parent/G2 stack publication will rerun current-head hosted checks and genuine Codex reviews; no merge claimed.

- Controlled parent run38687525601 records all30A/B+30A/A pairs and uploads artifact11950189815 (sha25672e2ca529be615986aef14d70d1ebc400d434b3badbd56f6a46f8fdcb56ee6c9). Completion is not a performance pass: artifact download into this sandbox is DNS-blocked, so measured p95/control interpretation remains pending. Recorder now emits its existing six summary rows to job logs after preserving raw receipts; this adds visibility without changing measurements or gating.

### 2026-10-14 — closed managed-check execution policy

- SQLite `-cmd '.shell cargo test'` reproduces the next configured-launcher bypass0/1 (`/tmp/g0-closed-policy-red.log`). Test-firstfc75959e expands the existing table to SQL clients, an unknown custom driver, path-qualified replacement programs, environment/startup assignments and shell login forms.
- Replaces the expanding opaque-program list with a closed match of inspected command forms. Unknown source executors refuse authority; deny-prefix parsing remains unrestricted. Existing shell/launcher/find/fd/sort constraints and all empty-policy behavior remain. Architecture and the AGENTS enforcer row record the execution model, including its trusted-executable/operator-environment boundary.
- All14classifier tests pass (`/tmp/g0-closed-policy-fixed.log`); independent focused review Approve. Earlier parentdd02 local check:fmt/lint/Clippy pass,2237/2307tests pass with65socket denials,3OAuth callback failures and2unclassified timeout failures; this is not green qualification. Final-head affected quick check and hosted checks follow; no performance waiver or merge.


### 2026-10-15 — PR308 current-main merge-readiness checkpoint

- User prioritizes PR308 then PR309 before new autonomous-core PRs. Current main0b4f29e3 integrated in c2b56814; the sole conflict was root.md and both histories are retained. Legacy-site repair4f4101b1 preserves76 exact sleep identities, removes one exception already absent on main, and retains ceiling76. Independent integration review Approve.
- Exact clean4f4101b1 affected check against current main: fmt1.50s/lint1537files76/76sleeps1.99s/Clippy24.93s pass. Tests2246/2314pass,68fail,14skips; all failures classify as65socket EPERM and3unavailable callback listeners. No non-environment failure and no green gate claim. Overall210.70s; target/qq-check/last.json and immutable/tmp/pr308-current-main-qualification/.
- Hosted e0453af4 CI run38691879018 passes affected tests/smoke; genuine description bot+1 reaction569905016 and11/11review threads resolved. Those facts apply to the published head, not unpublished local integration. Current remote main verification0b4f29e3 explains the conflict despite stale base_sha in the PR response.
- Controlled record38687525601 artifact11950189815 remains unexpired; connector returns the ZIP file reference, but local retrieval still fails DNS. Metrics interpretation, quiet-host/performance/full/smoke qualification and GitHub publication remain open. Earlier automatic approval review rejected equivalent GitHub writes; no bypass or hosted merge-ready claim.


### 2026-10-16 — rebase over Jev provenance and retained installer

- Current main9e010507 adds Jev provenance60dd03db and installer287 after the previously published stack. Resolve PR308 deadline overlap by retaining the exact main fixture, avoiding duplicate spawn observers/clocks; remap the same76sleep identities without raising the ceiling. Independent static review confirms production Jev callers and goal branches remain intact.
- Existing v33Jev golden fails1/1 because incoming current constructor now supplies delegate_model/delegate_request_id. Add those two existing optional provenance fields to the current-version fixture; historical fixtures remain unchanged. Red evidence/tmp/pr308-jev-golden-red.log. Final checks follow.
- Published PR30982b2d7b7 has successful affected/Clippy/tests/smoke run38829861435; duplicate38829861446 is cancelled. Do not invent a current-head test failure or treat the cancelled duplicate as a green full gate. Rebased stack needs fresh CI and remains unmerged.


### 2026-10-16 — exclude accidentally merged PR319 from the goal stack

- User removed PR319 (Jev protocol/response/transport provenance) from main. Clean main73ff0647 excludes60dd03db; published30881eee50 still carried replaybf786880 and30919c255aa inherited it. Rebuild from reviewed pre-Jev foundationsb39fd753/82b2d7b7 onto clean main, preserving current installer/docs consolidation and retired-plan deletion.
- Independent exclusion audit Approve: both candidates descend from73ff0647; neither removed/replayed commit is reachable. Runtime source and manifests exactly match each corresponding pre-Jev baseline. All4PR319-added files and added approval/reviewer provenance fields are absent. Original G0/G2 source behavior is preserved; earlier Jev-integration receipts above are historical, not current source claims.
- Keep both PRs unmerged until corrected heads and their CI are verified; do not restore the removed Jev patch to solve conflicts. Exclusion evidence and final qualification follow.
