# Ledger — Goals

Plan: [`../goals.md`](../goals.md). One writer per file.

| Slice | Goal | Status | Linear | Branch / PR | Notes |
| --- | --- | --- | --- | --- | --- |
| G0 | Foundation: durable goals and AC4 loop guard | Shipped (87d3ed1a) | ENG-1045 | [#308](https://github.com/retsu-AI/qq/pull/308) | Foundation merged; whole-plan acceptance remains open |
| G1 | — | Dropped (combined into G0) | | | No separate slice |
| G2 | Driver closeout | In review | ENG-1053 / ENG-1063 | [#330](https://github.com/retsu-AI/qq/pull/330) | G2.1 merged; full G2 acceptance belongs to this closeout |
| G2.1 | Idle driver, historical spend and admission checkpoint | Shipped (ffa46942) | ENG-1053 | [#309](https://github.com/retsu-AI/qq/pull/309) | Published checkpoint only; full G2 remains open |
| G3 | Completion checks and crash-safe recovery | In review | ENG-1063 | [#330](https://github.com/retsu-AI/qq/pull/330) | First new PR combines G2 closeout and G3 |
| G4 | TUI, CLI, foreground hosting and configuration | Planned | ENG-1064 | Next stacked PR | Depends on G2/G3 execution |
| G5 | Live multi-hour qualification | Planned | ENG-1065 / ENG-809 | Final stacked PR | Authorized: openai-codex/gpt-6.1-sol subscription only; no paid API fallback |

## Entries

### 2026-10-14 — G2 terminal accounting test-first

- ENG-1053 starts in isolated `.worktrees/autonomous-goal-driver`, branch `feat/eng-1053-goal-driver`, local stacked base G0e0b69446. Root is sole writer; G0 remains unmerged/unqualified for publication. Pre-change diagnostics are the preserved G0 production and enabled-goal recordings; neither substitutes for G2's scripted acceptance or quiet-host gates.
- Owned first unit: `sessions/{goals,settlement}`, goal-session tests and this ledger. No new protocol/schema/descriptor bump. Full G2 driver, clamping, recovery, cancellation and outcome acceptance remains required.
- Regression `goal_terminal_spend_is_once_and_stays_with_the_captured_historical_goal` compiles and fails at the first prompt-origin/live/unmeasured case: goal runs0 instead of1 despite terminal accounting with a completed model turn. Raw `g2-red-historical-terminal-spend.log`:0passes/1failure. The matrix also specifies goal-origin, replacement/history, known/unknown totals, replay no-op and live-only GoalUpdated publication; later cases are not claimed executed while the first assertion fails. Implementation follows.

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

- Completed-model terminal accounting now uses the existing durable outcome guard, captured historical goal row and live-only GoalUpdated before RunFinished. Matching pending ownership clears; uncertainty remains sticky. The eight-combination regression passes; terminal-event fault tests prove outcome/payload/event rollback, successful retry and replay no-op. Root owns compaction event-vector consumers and the architecture amendment.
- Broad session run:553passes/1failure/3skips; old model-update trigger also refused the newly required terminal counter event. Scoped that fixture to its intended notes mutation and added separate persistent terminal-event rollback coverage. Final fault suite18passes/0failures; goal suite36passes/0failures/1explicit skip, core all-target/all-feature Clippy passes35.41s, fmt/diff and independent final review approve. Raw g2-terminal-{session-suite,fault-suite,goals-final,core-clippy-final}.log retained.
- Independent render-width finding withdrawn: its fill-to-bound objective exceeded the legal2048-byte admission limit. Legal maximum fields with maximumu32/u64 counters pass notice bounds. Invalid-input probe retained only as diagnostic raw evidence, removed from tests; no speculative production fix.
- G2 remains incomplete: idle driver, exact remaining-budget/final-slot handoff, all uncertain/queued/reserved/recovery/panic paths, historical child rollup, waits/backoff and pause/cancellation semantics still require implementation/acceptance. Full workspace and performance gates remain before publication. No G2 PR, push or whole-plan completion claimed.

### 2026-10-14 — G2 provider rejection accounting regression

- Revalidated clean4d315d4d and runtime/streaming/recovery paths. Existing completed-turn bookkeeping also persists empty failed attempts; neither saw_turn nor model_turns existence proves chargeable work. Actual pre-provider WaitingForProvider activity is already durable; request uncertainty must retain a separate authoritative proof through recovery.
- New real SessionRuntime/scripted-provider test rejected_provider_outage_spends_no_goal_run_or_measured_budget reaches Paused after repeatedHTTP529 without output, then fails: captured goal runs1 instead of0. Raw g2-red-known-provider-outage-accounting.log:0passes/1failure. Expected unchanged known-zero usage/cost remain subsequent assertions, not yet proven. The1ms fixture is not the3-day outage acceptance.
- Chargeable-work provenance and unconfirmed-dispatch recovery implementation remain required, along with the driver and fullG2 matrix. No green source, push, PR or whole-plan completion claimed for this regression.

### 2026-10-14 — G2 durable chargeable-work evidence

- Replace coarse turn-row/saw_turn counting with private Committed/RejectedBeforeWork/Uncertain evidence. Schema48 records a goal-bound pending request in the existing activity transaction before provider dispatch and stores disposition with turn settlement; goal-less turns gain no extra transaction/store hop. Known pre-output API rejection retains its replay row, skips unknown turn charging, and preserves earlier measured totals. Pending/uncertain spending poisons certainty without proving a run-cap charge.
- c5103571's realHTTP529 regression now passes; actual committed-turn fixtures preserve historical replacement/replay behavior. New pending-request close/reopen fixture proves runs0/unknownusage+cost and recovery replay no-op. Broader goal suite initially37passes/1failure/1skip due old hardcoded schema47 assertion; use STORE_SCHEMA_VERSION. Final goal suite39passes/0failures/1explicit skip (g2-work-provenance-goals-final.log).
- Independent review caught schema48 validation after commit. New rollback regression failed47→48 on malformed existing columns; validation now runs inside migration before metadata/commit and for existing48 stores. All3malformed shape cases leave version47 and no added work_disposition column. Migration suite55passes/0failures/1skip, core all-target/all-feature Clippy-Dwarnings passes13.91s; fmt/diff and independent final review approve. Raw g2-red-request-migration-rollback.log, g2-work-provenance-{migrations,clippy}.log retained.
- Positive known helper spend is independent of run count; unknown pre-main routing/reviewer/owned-compaction spend still needs durable helper proof. Partial/empty terminal/provider transport/EOF/rejection-prefix combinations and full recovery/driver qualification remain required. Whole workspace/performance gates, G2 scheduling/clamping/final-slot/cancellation/backoff/multi-day acceptance and the rest of autonomous-core stay open. No G2 push/PR or completion claimed.

### 2026-10-14 — G2 pre-main helper accounting regressions

- `cargo test -p qq-core before_main_` completes with1pass/2failures: routing historical spend on reserved settlement remains zero, and unknown pre-main review spend remains falsely known-zero. Raw `target/qq-perf/ac3-stack-2026-10-13/g2-red-helper-spend-before-main.log`.
- Regression fixtures cover measured/unknown routing, goal replacement and replay; review fixture requires unknown spending with zero charged runs. Implementation and broader acceptance remain open.

### 2026-10-14 — G2 durable helper admission and queued finalization

- Goal-owned paid routing/reviewer admission is retained in schema48's private goal_helper_admitted column, in existing dispatch transactions. Empty/oversized routing fallback carries no admission. Queued/reserved/prepared/cancel/recovery/panic finalization charges the captured historical goal behind the exact-once terminal guard; helper work never consumes a main-run cap. Live GoalUpdated precedes RunFinished; replacement and replay retain their fences.
- Expanded regression matrix covers independently measured/unknown usage and cost, goal replacement, unsent fallback and event ordering. Original before_main_ suite passes3/0. Broad session suite passes559 with1failure/3skips: a preexisting hardcoded schema47 assertion. The corrected test verifies durable metadata against STORE_SCHEMA_VERSION and passes1/0 independently. Final broad rerun remains live7167, raw g2-helper-sessions-final.log; no full-suite green claim yet.
- Core all-target/all-feature Clippy-Dwarnings passes12.80s on corrected source; formatting and diff checks pass. Two independent read-only reviews approve the helper/queued unit without concrete blockers. Architecture and root schema reservation updated. Owned compactor accounting, scheduling/clamping/outage/driver acceptance and full workspace/performance/PR gates remain open; no push or G2 PR.

- Final9ddbe579 session rerun exits0:560passes/0failures/3explicit skips in175.26s (`g2-helper-sessions-final.log`). No full workspace, hot-path or PR qualification implied.

### 2026-10-14 — G2 owned compactor historical accounting regression

- `owned_compaction_spend_survives_owner_settlement_and_goal_replacement` fails0pass/1failure at the missing live GoalUpdated for pending helper spending after owner settlement; raw `g2-red-owned-compaction-spend.log`. Matrix covers created/pending/measured helpers, replacement, late settlement and replay; no helper goal tool authority.
- Implement historical owner lookup and helper admission in existing request/terminal transactions. Between-run dispatch classification, known rejection, complete child rollup and the full G2 driver remain required.

### 2026-10-14 — G2 owned compactor historical finalization

- Automatic compactor settlement joins its exact prompt owner in the same session and charges the owner's captured historical goal. Compactors consume no main-run allowance and receive no goal writer. In-run request admission retains helper proof in the existing aggregate write; owner teardown's abandoned-summary transaction charges before owner settlement, and late cleanup is a no-op.
- Corrected the first fixture's event assertion: owner teardown already settles an in-run helper, so events belong to owner.finish_run, not late cleanup. Created/pending/measured×replacement matrix now passes; injected helper GoalUpdated failure rolls back both outcomes and counters before one successful retry. Between-run measured-prefix fixtures cover queued owner cancellation, replacement, close/reopen, recovery and replay.
- Final goals suite43passes/0failures/1explicit skip in13.23s; compaction compatibility81passes/0failures in23.25s. Core all-target/all-feature Clippy-Dwarnings12.55s, formatting/diff checks and independent session/storage reviews pass. Raw g2-owned-compaction-goals-final.log, g2-owned-compaction-compat.log and g2-owned-compaction-clippy-final.log beneath the retained ac3-stack directory. Architecture amended.
- Known-rejection spending certainty, between-run actual-dispatch proof and complete child-rollup/driver/budget/lifecycle qualification remain required. Full workspace/performance gates and G0/G2 publication remain open; no full-plan completion claimed.

### 2026-10-14 — G2 real in-run rejection accounting regression

- Scheduler permit held before submission; owner reserves/starts through the real store and SessionInRunCompactor runs the actual provider-direct summarizer. Scripted knownHTTP529 before output currently returns unknown usage instead of knownzero:0pass/1failure, g2-red-real-compactor-rejection.log. Matrix includes prior measured rejected-tool work and uncertain transport loss, asserting both returned allowance spending and durable goal counters. Initial fixture compilation used an unsupported ProviderUsage default and was corrected before the behavioral red run.
- Typed pre-work rejection and exact prior-prefix terminal accounting remain to implement. Between-run admission/rejection, child rollup and whole driver/full-plan gates remain open.

### 2026-10-14 — G2 typed in-run compactor rejection receipt

- Provider-direct summarization preserves typed pre-work rejection only before text, reasoning, replay or calls. The in-run terminal transaction restores the observed exact prefix; no prior turn establishes explicit usage0/cost0 even without pricing. Returned spending and historical goal counters agree. EOF/transport/post-text/post-reasoning faults remain unknown. Existing guards keep cleanup/recovery idempotent.
- Real compactor fixture's scheduler permit now releases after owner settlement and before shutdown; the initial successful accounting path exposed a fixture-held-permit shutdown timeout, corrected without production shutdown changes. Ten-case matrix covers prior measured rejected-tool turn or no prefix × empty529/transport/partialtext529/reasoning529/EOF.
- Final goals44passes/0failures/1explicit skip in15.50s; summarizer compatibility14unit+8integration passes/0failures. Core all-target/all-feature Clippy-Dwarnings12.59s, fmt/diff and two independent read-only reviews approve. Raw g2-real-compactor-goals.log, g2-real-compactor-summarizer-compat.log and g2-real-compactor-clippy.log retained; architecture amended.
- Between-run actual admission/rejection, remaining child rollup, driver/clamping/final-slot/outage/lifecycle acceptance, workspace/performance and publication remain required. G0/G2 are local; no PR or full-plan completion claimed.

### 2026-10-14 — G2 real between-run dispatch regression

- A real SessionRuntime seeds29KiB×4 of prior history, sets a goal, then starts actual automatic compaction before its next prompt. EmptyHTTP529 vs transport fault matrix verifies helper usage/cost, absent goal authority and durable goal counters. Test fails0pass/1failure: empty rejection's helper usage remains unknown (`g2-red-real-between-compactor.log`).
- Accounting-only captured binding, provider-admission marker and typed internal rejection handling remain required. Full G2 driver and all original plan gates remain open.

### 2026-10-14 — G2 between-run accounting-only dispatch receipt

- Automatic compactors carry captured historical accounting attribution without GoalRunInput, goal tools or notices. Goal-owned creation seeds knownzero, actual provider admission records pending/admitted proof before polling, and internal completed/rejected/uncertain turns retain typed accounting. Goal-off dispatch adds no store hop.
- Actual32KiB-window matrix covers empty529, transport, hung-request owner cancellation, measured-prefix529 and measured-prefix→secondhung cancellation. Known-priced zero seed makes session certainty assertions unconfounded; helper rows, goal counters and public/durable session accounting agree. Helpers consume0main-run allowance and have no goal authority.
- Independent review reproduced cancellation overwriting NULL with zero/prefix; fixed durable pending preservation and typed-rejection clearing. Public folding now includes cancelled admitted requests. A second finding about placeholder recovery was fixed by reading evidence in the existing guarded row query; pending-goal reopen now asserts unknown durable/public session cost. Earlier fixture failures were an initial-only summarizer instruction assertion and a nonexistent public cost_known field, corrected before qualification.
- Final goals45passes/0failures/1explicit skip16.56s; full session suite564passes/0failures/3explicit skips228.20s. Core all-target/all-feature Clippy-Dwarnings18.53s, fmt/diff and independent session/storage re-reviews pass. Raw g2-between-goals-final-session-certainty.log, g2-between-sessions-final.log and g2-between-clippy-final.log retained. Architecture/root ownership amended; schema48/protocol33 unchanged.
- Completed-main crash recovery's stored session cost and full child spend rollup remain separate G2 accounting acceptance. Driver/clamping/final-slot/multi-day/outage/lifecycle, workspace/performance and publication gates remain required; no G0/G2 PR or full-plan completion claimed.

### 2026-10-14 — G2 committed-main crash accounting regression

- Main goal turn persists before close/reopen without terminal settlement. Measured/unknown cost × pending next request × goal replacement matrix checks historical counters, stored/public session totals and repeated recovery. Behavioral red0pass/1failure: unmeasured committed work leaves sessions.cost_known=true (`g2-red-committed-main-recovery.log`).
- Recovery must use durable aggregate/evidence for directly goal-bound main prompts as well as owned summaries; empty typed rejection must stay uncharged. Child rollup and full driver/whole-plan qualification remain open.

### 2026-10-14 — G2 completed-main crash accounting receipt

- Existing recovery spend query now includes directly goal-bound main rows. Pending/helper/Committed-or-Uncertain evidence charges durable known or unknown aggregates to stored session totals; rejection-only rows remain uncharged. No extra transaction or goal-off recovery behavior change.
- Eight-case committed turn matrix covers measured/unknown cost, pending next request and replacement, close/reopen and repeat recovery. Historical counters and cached/public session totals agree. Corrected the setup's prior unmeasured no-work row to knownzero and asserted that baseline; otherwise its public unknown total confounded the new measured case. Separate rejection-only reopen regression protects zero spending/runs.
- Final goals47passes/0failures/1explicit skip15.44s; settlement28passes/0failures3.79s. Core all-target/all-feature Clippy-Dwarnings13.14s, fmt/diff and independent session/storage review approve. Raw g2-main-recovery-goals-final.log, g2-main-recovery-settlement.log and g2-main-recovery-clippy.log retained; architecture amended.
- Child/audit/descendant spend is stored separately and not yet charged to goals. Full driver/budget/final-slot/outage/lifecycle/whole-plan acceptance, workspace/performance and publication remain required; no G0/G2 PR or completion claimed.

### 2026-10-14 — G2 delegated child historical spending regression

- Real store child admission, claim and committed paid turn are followed by parent completion and optional goal replacement; child settlement/replay must charge only the original captured goal, with0extra main-run count and a live event in the goal's original session. Behavioral red0pass/1failure: original goal usage stays0 instead of11input/3output (`g2-red-child-historical-spend.log`). Initial fixture incorrectly treated envelope.session_id as optional; corrected before behavioral run.
- Independent ownership investigation confirms parent RunAccounting excludes child/audit totals; caller BudgetMeter receipts remain separate. Persist immediate per-run paid-owner linkage on delegated initial runs; bounded ancestry resolves frozen historical goals, including independently active child goals. Later user prompts must inherit no old root attribution. Implementation, migration, audit/nested/helper/recovery qualification and full G2 remain open.

### 2026-10-14 — G2 sponsored rejection and deletion regressions

- Local test-first commits94ac25c5 and044f8587 reproduce respectively a sponsored request's rejection-without-turn leaving dispatch uncertainty, and deletion failing to preserve an unfinished sponsored child's ownership. Both fail behaviorally before their fixes; raw `g2-child-{rejected,delete}-red.log` under `target/qq-perf/ac3-stack-2026-10-13/`.
- Settlement now clears pending dispatch for paid-owner-bound typed rejections. Focused goal suite passes50/0/1explicit skip before deletion changes (`g2-child-goals-rejection-fixed.log`,17.89s). This store-level fixture verifies terminal accounting without a persisted turn; real nonrecoverable provider-path qualification remains required.
- Deletion now follows bounded paid-run ancestry, including exact same-session owned compactor obligations, refuses unsettled descendants, and clears immediate references atomically before deleting settled histories. Goal suite and core Clippy are running on this source; no green claim yet. Populated-schema48 migration, nested/intermediate deletion, audit/helper/recovery and the full G2 driver remain required. No G0/G2 PR or whole-plan completion claimed.

- Updated deletion/rejection source passes51goal tests/0failures/1explicit skip in17.84s (`g2-child-goals-delete-fixed.log`); core all-target/all-feature Clippy-Dwarnings passes17.36s (`g2-child-clippy-fixed.log`), formatting and diff checks pass. Independent read-only review finds no new concrete gap in the two fixes. Nested/helper deletion and real-provider rejection matrices remain unexecuted.
- Added pending-child upgrade regression covering direct pending child and pending grandchild through a settled initial child. It requires ancestry restoration without retroactive charging, including reopen idempotency. This test is running against the currently incomplete migration; no pass claimed.
- Upgrade regression exits101 with0pass/1behavioral failure: the pending child's paid-owner anchor remainsNULL after schema48→49 (`g2-child-upgrade-red.log`). Migration implementation is the next required fix. Earlier51-test receipt predates this new intentionally failing regression; current branch is not green or publishable.

### 2026-10-14 — G2 pending paid-owner migration implementation

- Schema49 migration streams unfinished initial delegated obligations and follows at mostMAX_CHILD_DEPTH exact owner edges through terminal intermediates. It verifies initial-message ownership, typed origin, same workspace, exact session parent/depth/owner and no cycles; retains only edges needed to reach frozen ancestor goals. No current goal lookup, counter change or terminal finalization occurs during upgrade.
- Initial direct/nested upgrade regression passes1/0 (`g2-child-upgrade-fixed.log`,0.60s), including reopen and unchanged counters. Independent review identifies terminal child with pending automatic compactor as another seed. Extended fixture initially used the root-only reservation lane; corrected to the depth-one lane before observing0/1behavioral failure (NULLanchor, `g2-child-helper-upgrade-red.log`). Test-first2e84a044 records this helper regression.
- Migration now includes exact same-session queued/running automatic compactor obligations. Full focused goal suite6798 and core Clippy are running; no green claim yet. Architecture describes paid ancestry, historical/live update boundaries, migration and deletion. Malformed/replaced/later-prompt migration, nested/helper deletion, audit/uncertain/recovery acceptance and the full G2 driver remain open. No PR or full-plan completion claimed.
- Final migration source passes52goal tests/0failures/1explicit skip in18.81s and55migration tests/0failures/1explicit skip in8.15s (`g2-child-migration-{goals,suite}.log`). Core all-target/all-feature Clippy-Dwarnings passes19.07s, fmt/diff pass; independent read-only review confirms helper seed closes its concrete gap. This locally qualifies the implemented accounting unit, not full G2 or the entire plan. Workspace/performance/publication and remaining acceptance matrices stay open.

### 2026-10-14 — G2 driver idle-wake regression

- Public SessionRuntime fixture creates an idle session then SetGoal without SubmitPrompt. It requires a fresh RunStarted with the exact Goal origin, objective in the actual provider request, terminal settlement andNULLcontinues_run_id. Current source fails0pass/1behavioral failure after2.21s: no autonomous work starts (`g2-driver-idle-wake-red.log`). The timeout first shuts down the runtime cleanly; no paid provider is used.
- Read-only driver investigation identifies existing scheduler/store integration: atomic bounded idle queueing and pending guard, SetGoal/GoalControl schedule signals, one nearest-deadline timer before permits, prompt preemption in admission, revalidation before claim, draining control cancellation, and common-start/restart budget handling. Startup already schedules after recovery; permit release already schedules after settlement. No additional executor or wire type is needed for initial dispatch.
- G2 driver implementation remains next; current test-first branch is intentionally red and must not be pushed. Existing accounting receipts qualify their earlier source only. All original driver-only multi-day/final-slot/outage/race/control/approval/auth and whole autonomous-core acceptance remain required. ContinueRun/AutoContinue are absent in current source; their planned integration cannot be claimed complete.

### 2026-10-14 — G2 initial guarded driver implementation

- Existing scheduler now has a single nearest-wake timer and a bounded32-candidate atomic queue sweep. Fresh Goal-origin rows, goal snapshot/notice, pending guard, messages and queue events share a durable transaction. Queue admission clamps goal-origin per-run limits to known remaining tokens/cost/deadline and marks exhausted/unknown capped budgets; common-start prompt limits and later elapsed preparation rechecks still require implementation.
- SetGoal/GoalControl schedule signals use the existing runtime channel. Review-found event publication and unconditional disabled-path store-hop issues are fixed: sweep returns bounded committed cursors; scheduler notifies after commit. Ordered store-worker goal-presence cache skips driver transactions/queries when false, seeds from durable presence at open, updates conservatively on SetGoal and recomputes within committed sweeps. Startup adds one read hop requiring final measurement. Rollback may conservatively retaintrue; no lost scheduling.
- Initial public dispatch now passes1/0 after correcting a fixture query for a nonexistent continues_run_id column (continuation types are absent here). Final fixture verifies exact Goal origin/objective and exactly1new run for its finite allowance. Concurrent queue and wait-deadline fixtures plus Clippy are running. No full-driver pass, publication, performance or complete-plan claim. Prompt precedence, stale queue/control draining, start-time/permit-wait clamping, outcome rules and all original G2 acceptance remain open.
- Final initial-dispatch focused checks pass: public idle wake1/0 in0.48s, concurrent queue guard1/0 in0.48s, waiting deadline1/0 in0.28s. Core all-target/all-feature Clippy-Dwarnings passes12.94s; fmt/diff and independent re-review pass. Raw `g2-driver-{idle-wake-final,queue-guard-final,wait-deadline,clippy}.log`. Full goal suite is now running to inspect lifecycle interactions; no full-suite or full-driver qualification claimed.
- Full focused goal suite completes51passes/4failures/1explicit skip in18.77s (`g2-driver-all-goals.log`). Client-control snapshots now see newly queued goal work; check-approval snapshot gains pendingrun; committed-replay test sees additional driver cursors; runtime goal-write fault fixture no longer reaches its old assumed terminal outcome. These failures are retained, not waived. Next work must reconcile these G0-only fixture assumptions while preserving durable-control/replay/fault guarantees and implement prompt preemption/control draining. Current integrated source is not fully green and must not be pushed.

### 2026-10-14 — G2 prompt precedence and cancellation drain regressions

- Test-firstb25deb63 reproduces queued goal getting the next claim after a user prompt arrives (0pass/1behavioral failure, `g2-driver-preempt-red.log`). Prompt admission now settles only unclaimed queued Goal/GoalCheck work in the same transaction. Event-write fault verifies rollback and retry; preemption fixture passes1/0 in0.57s before subsequent control edits. Ordered worker goal-presence admission skips preemption queries for goal-less stores.
- Test-first10f2cf59 reproduces pause leaving unclaimed goal work queued (0pass/1failure, `g2-driver-pause-drain-red.log`). Local control implementation settles unclaimed rows, requests cancellation without releasing reserved/running guards, reloads the post-settlement goal in responses, and rejects same-session draining resume. Queued/reserved/running fixture passes1/0 (`g2-driver-pause-drain-fixed.log`). Replacement/clear route through the same local cancellation helper.
- Independent read-only review identifies two remaining blockers before source commit: terminal parent with an unfinished sponsored detached child is outside current same-session cancellation/Resume checks; command replay restores signals only for CancelRun, not goal controls/replacement. Exact bounded paid ancestry must cover descendants/helpers without cancelling later unrelated prompts. Prompt-precedence repeat and core Clippy are running. Full four lifecycle failures, original driver start-budget/final-slot/backoff/outcome/multi-day acceptance and whole-plan gates remain open. No push/PR or completion claimed.
- Updated direct control source passes queued/reserved/running pause-drain fixture1/0 in0.83s and prompt-preemption+rollback repeat1/0 in0.52s; core all-target/all-feature Clippy-Dwarnings passes16.82s, fmt/diff pass. These are narrow results, not broader ownership acceptance.
- New terminal-parent/detached-child fixture fails0/1 in0.28s (`g2-driver-detached-drain-red.log`): Pause leaves its still-running sponsored child's cancellation flag false. Later unrelated child prompt exclusion is asserted after that fix; not yet reached. This is the next exact-ancestry lifecycle fix, alongside replay signal restoration. Current implementation remains uncommitted; no push or complete claim.

### 2026-10-14 — G2 exact paid cancellation ancestry and replay

- Local controls now resolve unfinished obligations from every frozen same-session goal run, including terminal historical parents, following only paid-owner edges up toMAX_CHILD_DEPTH and exact same-session automatic helpers. Pause/clear/replacement cancel these obligations without broad owned-session cancellation; later unrelated prompts remain independent. Resume and replacement queue admission use this same ancestry to wait for cancellation draining. Ordinary active edits cancel only unclaimed same-session root work.
- Detached-parent regression passes1/0 in0.33s (`g2-driver-detached-drain-fixed.log`), including later unrelated child-prompt independence. Test-first70dfb4d6 captures replay without restoration failing0/1 in0.27s (`g2-driver-control-replay-red.log`): pause replay lost child signal. Fixed code restores outstanding paid child/helper signals for SetGoal/GoalControl while preserving the original receipt and appending no duplicate events. Matrix covers pause/clear/replacement and excludes terminal obligations.
- Replay matrix89702 and focused control/dispatch repeat plus core Clippy are running. Independent read-only review finds no further concrete blocker in paid-obligation ownership, with active-edit same-session restriction now landed locally. Four prior lifecycle integration failures, full start-time/final-slot/deadline/outcome/backoff/multi-day acceptance, workspace/performance and original whole-plan gates remain open. Source stays local; no push or full completion claimed.
- Final control source passes replay pause/clear/replacement matrix1/0 in1.43s; repeated public idle-dispatch1/0 in0.27s, detached paid drain1/0 in0.30s, queued/reserved/running pause1/0 in1.14s and prompt-preemption+rollback1/0 in0.99s. Core all-target/all-feature Clippy-Dwarnings passes14.68s; fmt/diff and independent ownership review pass. Raw `g2-driver-control-replay-fixed.log`, `g2-control-*.log`, `g2-controls-paid-ancestry-clippy.log`. Architecture records exact paid controls and replay semantics.
- This locally qualifies the control unit only. Prior whole-goal suite51/4/1skip remains unresolved, and driver terminal outcome rules must stop immediate failed-run loops before broader lifecycle qualification. Auth/config pause, retry backoff, failure/no-progress streaks, final-slot/start-time budget clamps and full multi-day/workspace/performance/publication gates remain required. No G2 PR/push or whole-plan completion claimed.

### 2026-10-14 — G2 guarded terminal outcome policy

- Test-first629c34f6 reproduces auth failure leaving goalActive (0pass/1failure `g2-driver-outcomes-red.log`). Existing guarded finalization now applies outcome policy only to the direct live active exactrevision goal: auth/config needs-user pause, otherfailure3/noProgress2 blocking, completion preserving model wait, and global budget exhaustion distinct from per-run limits. Ancestor/historical spending remains independent. No goal-less store hop or new wire/schema field is added.
- Public loader fixture proves auth/config each stop after1preparation attempt and other failure blocks after3, charging0main runs (`g2-driver-loader-outcomes.log`,1/0,0.81s). Review identifies ADR49 consecutive-streak and any-committed-work backoff reset requirements. Test-first215ae03c reproduces current committed-prefix retry delay240000vs60000ms (0/1,0.31s `g2-driver-committed-backoff-red.log`); corrected policy resets nonmatching streaks and stops retry history at committed evidence.
- Corrected12-step retry/current+previous committed-prefix/reopen/cap/completion sequence passes1/0 in0.48s; six-case outcome matrix1/0 in1.66s; explicit Failed→NoProgress→Failed→Failed and NoProgress→ProviderRetry→NoProgress sequences1/0 in0.57s. Core Clippy-Dwarnings13.63s, fmt/diff and independent re-review pass. Raw `g2-driver-{committed-backoff-fixed,outcomes-reset-fixed,interleaved-streaks,outcomes-reset-clippy}.log`.
- Full goal suite60625 is running. Typed cancellation causes, held-approval pause priority, credentials-specific mapping, start-time/final-slot clamps, full driver multi-day/recovery and workspace/performance/publication acceptance remain required. Current focused receipts do not qualify the complete driver or whole plan. No G2 PR/push or completion claimed.
- Full suite before no-op correction completes57passes/6failures/1explicit skip in23.40s (`g2-driver-outcomes-all-goals.log`): client/check snapshots and replay cursors, paid-preparation cancellation expectations, routing outcome-event count, and owned compactor event count. The prior runtime goal-write fault test now passes; the suite remains unqualified.
- Fixed unnecessary outcome-only GoalUpdated when terminal policy changes no state. Existing owned-compactor regression now passes1/0 in1.63s (`g2-driver-outcome-noop-helper.log`); core Clippy13.24s (`g2-driver-outcomes-noop-clippy.log`), fmt/diff pass. Full final-source goal suite is running; remaining integration failures must be reconciled with exact durability/authority/cancellation invariants, not dropped or waived.
- Final-source goal suite completes59passes/4failures/1explicit skip in21.66s (`g2-driver-outcomes-final-goals.log`): client/check snapshot expectations, replay cursor expectation, and paid-preparation after cancellation. Owned-compactor and routing event-count failures are resolved by no-op suppression; runtime goal-write fault also passes. The complete suite is still red and must not be pushed. Next integration work preserves those tests' stale-control, replay, first-paid-binding and cancellation guarantees while reconciling them with the driver.

### 2026-10-14 — G2 driver-aware control and preparation fixtures

- Reconciled four prior integration failures without removing durability/authority assertions. Client/check fixtures hold root permits and deterministically queue goal work, verify exact pending Goal-origin identity, preserve full goal state and original replay receipt, and compare the post-queue stable cursor rather than the older command cursor. Active goal shutdown/reopen checks all stored fields and0charged runs while verifying a fresh pending run identity after restart; pause/edit retain typed cancelled run history.
- Paid-preparation fixture still verifies exact frozenSome/None binding and failed rebinding across replacement/reopen. A bound replaced run is now asserted cancelled and unable to start; capturedNone remains runnable and receives no later goal authority. First corrected run passes60/3/1skip but exposes fixture-held permits delaying shutdown and raw session deletion violating queued-run FK cleanup; start shutdown before releasing test permits and use publicPause/DeleteSession before replay.
- Final goal suite passes63tests/0failures/1explicit skip in20.83s (`g2-driver-control-fixtures-fixed.log`). Independent read-only review finds no weakened assertion or concrete regression. Core all-target/all-feature Clippy and broader sessions suite are running. No full driver/multi-day/approval/start-budget/performance/workspace/publication qualification claimed; the complete autonomous-core objective remains open.

### 2026-10-14 — G2 real-compaction fixture admission ordering

- Broader session verification completes627passes/1failure/3explicit skips in205.77s (`g2-driver-fixtures-sessions.log`); the sole failure is maximum-goal three-summary fixture admission racing automatic goal work. Hold root permits until the explicit prompt is admitted and retain a durable model-style wait to prevent a follow-up consuming the single script. Maximum fields, Completed outcome, three real summaries, exact notice placement and reopened window equality remain asserted.
- Corrected focused fixture passes1/0 in2.35s (`g2-driver-maximum-goal-compaction.log`). Independent read-only review confirms no weakened invariant. Full session repeat is running as21573 (`g2-driver-fixtures-sessions-fixed.log`); no broad green claim yet. Core Clippy before the final compaction test change passes14.29s (`g2-driver-fixtures-clippy.log`).
- Common admission still needs current goal remainder checks before preparation and main start, absolute deadlines across refresh, and final-slot handoff. Typed stop causes, checker, multi-day/soak, workspace/performance and the remaining autonomous-core deliverables are open; no G2 PR/push or whole-plan completion claimed.
- Final broader session repeat passes628tests/0failures/3explicit skips in209.16s (`g2-driver-fixtures-sessions-fixed.log`); fmt/diff and final core all-target/all-feature Clippy-Dwarnings pass (`g2-driver-fixtures-final-clippy.log`). This closes the observed lifecycle/compaction fixture failures while retaining exact receipts, frozen authority and real compaction assertions. Workspace-wide and complete G2/full-plan gates remain open.

### 2026-10-14 — G2 admission after permit wait

- Prior turn is progress:94d451fc committed independently reviewed fixtures with628/0/3skip sessions. New public scheduler regression controls queued Goal/user-prompt origins and expires the durable deadline while all root permits are held. Initial test missed a scheduler wake after releasing its artificial permit; corrected fixture now reproduces an expired Goal-origin run reaching the loader (1loadvs0,0pass/1fail in0.24s `g2-admission-permit-deadline-red-corrected.log`).
- Preserve immutable captured authority while deriving current historical goal budget admission in the existing reservation transaction. Goal-less runs must skip this derivation. Common main/first-paid revalidation, absolute preparation/helper deadline propagation, final-slot handoff and all remaining full-plan acceptance remain required; no push or PR qualification claimed.
- Initial admission source derives current durable historical budget after a scheduler permit, keeps captured authority immutable, and clamps Goal-origin per-run or user-prompt own limits to token/cost/time remainder. Goal-less claims skip the lookup. Execution refuses expired, unknown-capped or inactive ownership before loading. Independent review found pre-loader user prompts missing durable binding; capture Some/notice now freezes after the terminal/status guard inside the existing reserved settlement transaction. Panic/recovery metadataNone and settled replay preserve existing attribution.
- First source goal suite passes64/0/1explicit skip in21.25s (`g2-admission-first-goals.log`). Extended public six-case deadline/tokensUnknown/costUnknown × Goal/userPrompt matrix passes1/0 in1.54s (`g2-admission-refusal-matrix.log`), verifying0loads, matching durable exhaustion, exact bound terminal row and0charged runs. Current-remainder/stricter-prompt reservation matrix is running; main/paid refresh, automatic-helper fixed deadline and final-slot handoff remain open. No broad qualification or push claimed.
- Current-remainder Goal/unbounded-prompt/strict-prompt reservation matrix passes1/0 in0.84s (`g2-admission-remainder-matrix.log`), including post-queue spending changes. Final goal suite65/0/1explicit skip in21.30s (`g2-admission-final-goals.log`); core all-target/all-feature Clippy-Dwarnings12.51s, fmt/diff and final independent review pass. Architecture documents initial admission and same-transaction refused prompt attribution. This qualifies the initial reservation unit only; common main/first-paid refresh, absolute helper deadline propagation, final-slot handoff and original whole-plan gates remain open.

### 2026-10-14 — G2 main-start revalidation regression

- Initial-admission parent3bad8599 broader sessions complete630passes/0failures/3explicit skips in207.74s (`g2-admission-all-sessions.log`), resolving live handle13626. This verifies the initial permit-admission unit; it does not cover main/paid refresh or final-slot handoff.
- New public fixture holds loading open, expires the durable goal deadline, then releases the counting model. It verifies exact queued Goal/user-prompt run ID and stored origin before releasing the loader, matches that exact terminal event, and requires0provider requests, Duration exhaustion and0charged durable goal runs. Independent review confirms the loading boundary and requested stronger origin/event checks are included.
- Focused regression reproduces a Goal-origin main request after expiry (1requestvs0, `g2-main-start-deadline-origin-red.log`); source fix is pending. Main/paid transaction return must distinguish Started, typed goal refusal and lost reservation without public error expansion. Effective limits refresh must preserve the original execution anchor rather than subtract elapsed preparation twice; admission proof includes unfinalized sponsored work while own routing prefix must not be deducted and charged twice. Routing/unknown spend, helper deadlines, final-slot and remaining whole-plan gates stay open. No push or G2 PR qualification claimed.

### 2026-10-14 — G2 main-start exhaustion and ownership fence

- Prior test-first3bcefabf is progress, reproducing an exact Goal-origin main request after deadline expiry during loading (1requestvs0). Main-start now checks current durable goal exhaustion/ownership inside the existing start transaction before any writes and returns a private typed refusal separately from storage failures/lost reservation. Goal-less starts retain existing hops and skip this lookup. Runtime refusal uses guarded reserved settlement with exact captured metadata; no RunStarted is committed.
- Focused both-origin deadline-during-loading regression passes1/0 in0.49s (`g2-main-start-deadline-fixed.log`). Independent source review verifies rollback/publication and identified misleading 'no paid work started' wording after paid preparation; corrected to 'no main model request started'. Full first goal sweep64/2/1skip20.55s exposes two old fixtures expecting inactive captured ownership to start; corrected to typedCancelled refusal while preserving every exact frozen Some/None, stored payload, rebind rejection and restart assertion.
- Reconciled goal suite24645 is running (`g2-main-fence-goals-reconciled.log`). Main accepted-limit refresh remains unapplied; routing/compaction revalidation, pending sponsored spend proof without own-prefix double charge, helper absolute deadline, final-slot handoff and full original autonomous-core acceptance remain open. No push or G2 PR qualification claimed.
- Reconciled goal suite passes66tests/0failures/1explicit skip in21.33s (`g2-main-fence-goals-reconciled.log`). Stronger exact-run durable assertion verifies NULLstarted_at_ms, budget_exhausted status and non-NULLgoal attribution after both-origin refusal; focused1/0 in0.46s (`g2-main-fence-durable-start.log`). Final core all-target/all-feature Clippy-Dwarnings passes14.57s (`g2-main-fence-final-clippy.log`); fmt/diff and independent final fixture review pass. Architecture records main exhaustion/ownership fence. Full session repeat follows this local unit; original remaining limits/paid refresh/helper/final-slot/whole-plan gates are unchanged.

### 2026-10-14 — G2 accepted main allowance refresh

- Prior main-fence62eb9039 full session repeat completes631passes/0failures/3explicit skips in207.84s (`g2-main-fence-all-sessions.log`), resolving handle70744. New public held-loader regression changes known durable token spend0→9000 against10000 cap, then requires the exact Goal/user-prompt run's actual output cap≤1000, durable effective limits1000 and exact one-run/9002-token final accounting.
- Independent review confirms positive remainder is not confounded by mock measured input, origin is exact and persisted limits are checked. First focused run fails0/1 in0.25s with actual outputcap1024 (`g2-main-positive-remainder-red.log`). The source revalidated exhaustion but ignored accepted refreshed limits. Final-slot handoff will later extend this fixture's one-request expectation to include the required bounded response; allowance/accounting assertions must remain exact.
- Implement a fixed admission anchor plus typed reprepare result: rebuild an unpolled prepared request when effective bounds shrink, preserve resolved attachment bytes and the original deadline, and persist effective limits atomically with RunStarted. Paid-boundary checks, pending sponsored spending without routing-prefix double charge, helper fixed deadline, final-slot and original whole-plan gates remain open. No push or G2 PR qualification claimed.
- Accepted main refresh now carries a fixed epoch admission anchor, returns typed boxed-limit Reprepare before writes, drops the unpolled request and reloads reserved context under tighter bounds. Resolved attachment bytes and summary-only admission are retained; effective limits_json commits with RunStarted. Duration remains relative to the original anchor rather than subtracting preparation twice. Goal-less paths avoid the new admission lookup and request override.
- First refresh implementation still reproduced outputcap1024 (0/1, `g2-main-positive-remainder-fixed.log`): existing RunLimits meter did not shape provider output. Initial goal-owned output now caps at accepted total/output bounds. Positive exact-origin/effective-row/counter matrix passes1/0 in0.53s (`g2-main-positive-remainder-output-fixed.log`); independent review finds no blocker in this scope. Full goals97593 running. Subsequent-turn hard bounds, paid routing/compaction admission, pending sponsored spending, helper fixed deadlines, final-slot and original plan gates remain open; no push/PR qualification claimed.
- First tightened-source goal sweep67/0/1skip21.66s and core Clippy12.63s pass (`g2-main-refresh-{all-goals,clippy}.log`). Further independent review identifies raised goal budgets using already-clamped prompt limits and duration; public extension reproduces old60s bound after a raised120s deadline (0/1 in0.76s `g2-main-raised-allowance-red.log`). Main refresh now reads the prompt's original submitted limits in its goal-bound transaction, or starts from current goal per-run limits; reservation also discards automatic queue-cache bounds. No goal-less lookup or extra store hop is added.
- Five-case tighter Goal/prompt, raised tokens/deadline Goal/prompt and stricter2000-token prompt under raised3000-token goal matrix passes1/0 in1.33s (`g2-main-raised-allowance-fixed-anchor-fixture.log`), with exact actual outputcaps, durable limits and counters. First upper-duration assertion wrongly capped an allowance expressed against an earlier admission anchor at120000ms; a deadline explicitly extended120000ms after that anchor legitimately includes prior elapsed time. Corrected fixture retains rejection of the old60000ms bound. Current final goal sweep80688 is running; paid/pending/helper/subsequent-turn/final-slot/full-plan gates remain open.
- Final source goal sweep67/0/1explicit skip in21.64s (`g2-main-refresh-raised-all-goals.log`) and core all-target/all-feature Clippy-Dwarnings12.56s (`g2-main-refresh-raised-clippy.log`) pass. Fmt/diff and independent final base/anchor review approve. Effective main limits now recover explicit raised goal allowance while preserving submitted prompt bounds; source stays local and broader sessions follow. Paid/pending/helper/subsequent-turn/final-slot/workspace/performance/publication and original whole-plan gates remain required.

### 2026-10-14 — G2 paid-routing admission fence

- Prior main refresh5f830944 full sessions632/0/3explicit skips in209.70s (`g2-main-refresh-all-sessions.log`), resolving56263. New public fixture holds loading open, expires durable goal deadline, then requires0router calls/0main requests for exact Goal/user-prompt origin, Duration exhaustion, absent routing/payer/start markers and exact known0goal spending.
- Test-first routing expiry regression fails0/1 in0.23s with1paid router call after expiry (`g2-paid-routing-expiry-red.log`). Main fence already prevents the main request but is too late to prevent routing spend. Reuse original-base/fixed-anchor allowance refresh inside existing routing transaction; typed changed-limit reprepare must occur before pending/admitted writes. Existing router maximum-cost proof must be rerun on refreshed bounds before dispatch.
- Routing/helper positive and unknown-cap matrices, pending sponsored-spend proof, full helper deadline propagation, later-turn hard bounds, final-slot and original whole-plan acceptance remain open. No push or G2 PR qualification claimed.

### 2026-10-14 — G2 current paid-routing boundary qualified locally

- Private shared goal-limit refresh now runs inside the existing routing transaction before pending/spend/payer writes. Changed limits retry admission only; paid routing is never retried after dispatch. Finite goal cost allowances require a declared maximum within the refreshed allowance; unusable routing bounds pause NeedsUserConfiguration instead of repeating zero-work attempts.
- Public6-case deadline/over-cost/unknown-cost × Goal/prompt regression is green with0router/main calls and exact absent routing/payer/start rows and known0spend. Goal suite68/0/1skip22.03s (`g2-paid-routing-all-goals.log`); core all-target/all-feature Clippy-Dwarnings13.23s, fmt/diff pass; independent read-only review Approve. Prior reds retained.
- G0/AC4 is now draft PR308 at9b0130cb, parents296/297 merged and main301 integrated there. Driver source still uses the old parent and must reconcile schema48/descriptor15 before publication. Per-run duration, paid pending proofs, helper bounds, later-turn/final-slot and the original whole-plan gates remain open.

### 2026-10-14 — G2 own-duration admission regression

- Routing-only goal suite68/0/1skip and Clippy pass at79dd034a. Extending public admission fixture reproduces a paid router factory after a tightened1ms Goal per-run duration expires during loading:1vs0 (`g2-paid-routing-per-run-duration-red.log`).
- Stronger prompt case submits its original1000ms duration, holds the store worker, and uses the router's declared-cost query to prove loading finished and paid admission is waiting. After1005ms, releasing the writer stilldispatches1router instead of0 (`g2-paid-routing-prompt-admission-duration-red.log`,0/1,2.65s). Exact origin/run and absent paid/start markers remain acceptance; no inferred callback-free success.
- Independent premise/design review Approve: retain original monotonic execution start in the private goal admission envelope; current refreshed duration refusal is distinct from absolute goal deadline and must happen before writes. G0 PR308 has genuine Codex no-major-issues review atf7806bf4 and opening+1 reaction568729621 at2026-10-14T19:55:20Z; current9b0130cb changes documentation only. Current-head performance run38626845913/job116068065076 remains live; no qualification claimed.

- Own-duration source keeps the original TokioInstant in private goal admission while retaining epoch anchor for absolute goal remainder. Refreshed elapsed-duration refusal precedes routing/main writes and maps to Duration exhaustion without claiming the goal deadline expired. Reprepare never resets the instant. Public8-case matrix passes1/0 in3.12s; full goals68/0/1skip22.69s and core all-target/all-feature Clippy12.80s pass, fmt/diff and independent review Approve (`g2-paid-routing-per-run-duration-fixed.log`, `g2-own-duration-{goals,clippy}.log`). Parent reconciliation and broader paid/helper/final-slot/whole-plan gates remain open.

### 2026-10-14 — G2 integrates the published G0 parent

- Merge G0 PR3089b0130cb after own-durationc857126b. Keep parent native process preparation/receipts, source identities, descriptor15 historic13/14 domains and protocol33. Schema sequence is deployed47 receipts → draft48 goals →49 request/helper proof →50 paid-owner anchors. Historical receipts retain their original unshipped numbers.
- New parent48→50 upgrade/reopen test retains exact goal payload, frozen initial notice and finished process receipt, adds known-empty proof/anchor fields, and demonstrates no retroactive goal-counter change. Goal suite69/0/1skip23.09s passes (`g2-parent50-integration-goals.log`). Initial compilation missed the new test fixture's model_work field; retained log, fixed to Committed. Proof49 and anchor50 malformed migration/backfill tests also pass in this suite.
- Full current-source workspace qualification and independent integrated migration review follow before the next draft stack publication. G2 paid pending envelopes/helper deadlines/later-turn/final-slot/typed outcome races/soak and all original plan gates remain open.

### 2026-10-14 — G2 summary rejection and uncertain-work matrix

- Parent50 workspace run fails only the legacy soak assertion that every later summarizer fault makes aggregate usage unknown; its scripted API faults emit no output and are now typed pre-work rejections. Core1104/0/4skips passed before soak20/1/7skips stopped the pipeline (`g2-parent50-workspace-tests.log`).
- Test-only fixture now covers both exact11/13 measured prefix preserved through pre-work rejection and unknown aggregate after partial output then failure. Focused1/0 in1.32s; all soak21/0/7explicit skips in13.59s (`g2-summary-fault-{matrix,all-soak}.log`). Independent fixture and integrated schema48→50 migration review Approve. No production accounting assertion was weakened.
- Full workspace qualification, publication and remaining G2/whole-plan acceptance follow; no ready-to-merge claim.

#### G2 driver checkpoint receipt — 2026-10-14

Source:6cf23d58 workspace,1093e764 final parent test-only merge; production identical.
Tests: workspace2466top-level/0failed/15explicit skips in411.60s; raw2471 includes5nested child results. Core1104/0/4skips330.52s; soak21/0/7skips21.02s.
Checks: isolated workspace fmt1.42s, all-target/all-feature Clippy17.06s, build12.65s pass.
Parent merge: G0804948b0 native fixture and appended receipts; retain both ledgers. Final fmt1.34s/Clippy12.98s/build4.36s and focused native deadline1/0 pass. Full workspace was not repeated for this independently reviewed/tested fixture-only delta.
Review: independent implemented-unit, fixture and schema48→50 integration reviews Approve.
Evidence: target/qq-perf/ac3-stack-2026-10-13/g2-{isolated-final,published-parent-final}-*.log.
Open: sponsored pending allowances, routing-prefix request remainder, helper/child deadlines, later-turn bounds, final-slot, typed stop/approval causes, multi-day/outage/kill acceptance and all original plan gates. No paid runs or ready-to-merge claim.

- Published draft stack [PR309](https://github.com/retsu-AI/qq/pull/309) at92d40be3 against G0804948b0. Codex review requested and ENG1053 linked/In Progress. G2.1 is the implemented review checkpoint; G2 parent remains incomplete. Next public regression proves routing900 of1000 tokens leaves at most100 output while durable limits/accounting remain inclusive.

### 2026-10-14 — G2 routing-prefix request bound regression

- Published driver checkpoint2809b067 in PR309, base PR308804948b0; review requested. New public Goal/prompt × ordinary/strict-output matrix verifies inclusive durable1000-token limit and exact901input/1output accounting after routing900 then main1/1.
- Test-first focused regression fails0/1 in0.28s: initial Goal main request offers1000output instead of remaining100 (`g2-routing-prefix-remainder-red.log`). Exact durable allowance and once-only goal totals already pass before this assertion. Remaining matrix cases are not claimed executed while the first fails.
- Reuse the runtime BudgetMeter's post-routing positive allowance only for request shaping; retain inclusive durable/meter limits and existing typed exhausted/unknown boundary. Full G2/helper/final-slot/whole-plan gates remain open.

- Extended prefix matrix to measured900, fully spent1000 and unknown usage, both origins and strict output. Four fully-spent cases already refuse before main; the next positive case still fails1000vs100 in1.35s (`g2-routing-exhausted-prefix-red-fixed-type.log`). Initial compile had a fixture counter-width mismatch, corrected without changing assertions. Unknown cases remain unexecuted in this red run. Independent fixture review approves using the existing meter after its one routing charge; no duplicate meter or durable-limit subtraction.

- Initial request shaping now uses the existing runtime meter after its one routing charge, only for a direct goal with a routing receipt. Positive remaining total/output reduces the request cap; typed exhausted/unknown stops before main dispatch. Inclusive durable/meter limits and goal-less path remain unchanged. Public12-case matrix1/0 in3.39s; all goals70/0/1skip26.61s and core all-target/all-feature Clippy14.47s pass (`g2-routing-prefix-{remainder-fixed,goals,clippy}.log`); independent initializer review Approve. Workspace qualification precedes push. Later-turn/sponsored/helper/final-slot/whole-plan gates remain open.

### 2026-10-14 — side deadline fixture phase correction

- Routing-remainder source d6d99b3d workspace fmt1.35s/Clippy16.26s/build11.96s pass; tests stop at core1104/1/4skips267.39s (pipeline312.17s). Old side_timeout_drains_a_blocked_inspection_tool_before_the_answer_returns waits10s for a blocked read, but its200ms real budget can expire during preparation. Original narrow run1/0 in0.65s confirms the failure is load-sensitive; failed log remains retained.
- Test-only correction pauses time with bounded5s real-yield keepalive, waits actual read entry, advances400ms and awaits exact cancellation before asserting answer and durable row remain unsettled. Only releasing the blocked read allows TimedOut return/shutdown. Focused1/0 in0.30s (`g2-side-timeout-phase-fixed.log`); independent phase/drain review Approve. Broad qualification follows; no production timeout change or green push yet.

### 2026-10-14 — G2 Codex review regressions

- PR309 Codex findings4276108596/8614/8622/8628: skipped routing crash uncertainty, direct known-zero rejection snapshots, no-work budget retry loop and legacy schema48 unfinished-turn proof. These remain open; no settled parent finding is repeated.
- Use isolated .worktrees/autonomous-goal-budget on branchfix/eng-1053-goal-review, base driver e01901be while its broad check53282 runs. Sponsored test-first branch751c9f71 remains intact for later admission work; no conflicting writers.
- New public paused-clock loader advances200ms against100ms per-run duration; first3attempts must finish BudgetExhausted,0provider calls/0chargedruns/known0spending, then BlockedRepeatedFailure. Fourth-load Configuration fallback bounds the red fixture. Red0/1 in1.12s reaches that fallback instead of blocking (`g2-review-no-work-exhaustion-red.log`). Independent investigation confirms the streak reset and immediate scheduling path.

- P1 no-work exhaustion now retains/advances the existing3-failure streak without fabricating a Failed outcome; committed exhaustion resets it, and overall goal exhaustion precedes blocking. Public loader regression passes; Store sequence covers reopen after2, committed-work reset, replay no-op and deadline precedence (1/0 in0.71s). Goals/review suite72/0/1skip26.29s, core all-target/all-feature Clippy15.76s, fmt/diff and independent source review Approve. Proposed ADR49, plan acceptance and architecture clarify this distinction. P1 remains unpushed/unresolved externally until integration/workspace qualification; other3findings and original whole-plan gates remain open.

- Direct rejection snapshot regression extends the real529/no-output acceptance to direct/inclusive public session totals. It fails0/1 in0.32s withusage/costNULL instead ofknown0 despitecorrectgoal/storedcounters (`g2-review-direct-rejection-snapshot-red.log`). Shutdown precedes assertions so the red fixture leaves no live worker. Add directcapturedgoal to the existing knownzero seed; uncertainwork still poisons it through existing evidence.

- Candidate directzero seed passes real rejection1/0 in1.48s, but independent review finds pending poisoning restricted to compaction. New public Goal/prompt hung-request + Pause regression proves actual durable pending marker,0modelturns and exactorigin, then expects run/public/cachedsession/goal totals allunknown with0chargedruns. Candidate fails0/1 in0.30s withfalseknown0publictotals (`g2-review-main-cancel-uncertainty-red.log`). Seed remains uncommitted while this additional failure path is repaired; no green publication claim.

- Known-zero direct rejection seed is now paired with pending poisoning for every goal-paid request, not only compactors. Two-origin pending main cancellation1/0 in0.52s and direct typed rejection1/0 in0.32s pass (`g2-review-direct-{cancel-fixed,rejection-fixed}.log`). Pending rows preserve durableNULL and cachedsessionunknown; actual committed unpriced/transport work still follows existing unknown paths. Broader checks and independent re-review follow.

- Skipped-route crash regression stops after RoutingStarted before completion, injects event-write rollback, thenclose/reopen/recovery and replay. A clean knownzero fixture baseline prevents legacyNULLs frommasking the row. Red0/1 in1.42s: skippedroute publictotalsbecomeunknown althoughgoal totalsremainknown0 (`g2-review-skipped-routing-crash-red.log`). Paid=true is the unchanged unknown-spend control. Atomicallyseed knownzero aggregate onlypaid=false inexistingtransaction; no extraAPI/storehop/fallbackdecisionserialization needed. Independent design review approves.

- Skipped routing now atomically seeds known0usage/cost in its existing RoutingStarted transaction; paidrouting retainsNULL pending and admittedflagtrue. Crash/rollback/reopen/replay + paidcontrol passes1/0 in1.83s (`g2-review-skipped-routing-crash-fixed.log`). No new storehop/publicAPI/serializeddecision duplication. Broader qualification follows; original Codex thread staysopen untilpublished.

- Legacy schema48 regression fails0/1 in0.29s: persisted measured model work recovers with0charged runs instead of1 (`g2-review-legacy-work-proof-red-fixed-api.log`). Cases also cover zero measured work, ambiguous work, terminal preservation and replaced-goal isolation. Migration repair follows; no published fix claimed.

- Schema51 restores missing unfinished per-turn proof from the existing Legacy/Replay codec, measured usage (including zero) and truncation; empty unmeasured turns remain uncertain. Repair covers48 andalready50 without terminal recharging. Initial wrong provider-content decoder did not compile; corrected storage codec passes the migration/recovery matrix1/0. Added explicit malformed-second-turn transaction rollback and unmeasured content/replay/truncation coverage; qualification pending.

- Extended legacy suite6/0 in3.15s, including measured/zero/ambiguous/terminal/replaced/already50/unmeasuredcontent/replay/truncation recovery and malformed-second-turn rollback. Independent corrected schema51 review Approve; direct-zero+pending-uncertainty and skipped-route source review Approve. Full workspace qualification precedes integration/push; Codex findings still externally open.
#### G2 initial routing remainder qualification — 2026-10-14

Source:e01901be (productiond6d99b3d plus side deadline fixture).
Checks: isolated fmt1.36s, workspace all-target/all-feature Clippy13.84s, build4.40s pass.
Tests: workspace2467top-level/0failed/15explicit skips407.92s; raw2472 includes5isolated child results.
Focused: post-routing12-case Goal/prompt × positive/spent/unknown × strict-output matrix1/0; goals70/0/1skip; side blocked-read phase1/0 and independent initializer/drain reviews Approve.
Evidence: target/qq-perf/ac3-stack-2026-10-13/g2-routing-side-final-*.log; prior load-sensitive side failure retained.
Open: four Codex PR309 findings, sponsored admission751c9f71, helper/later-turn/final-slot/multi-day and original whole-plan/performance/T13 gates. No ready-to-merge or paid-run claim.

- Four-fix integrationa56c7ce2 fmt1.39s/Clippy25.64s/build16.26s pass. Workspace test run9712 terminates101 in440.59s; core1110/1/4skips354.82s. Sole failure is old hanging-provider fixture:100ms execution budget expires during preparation, so actual provider requests0vsasserted1. All four new review regressions pass. Retain failed log; paused-clock fixture now waits bounded actual provider entry before150msadvance and shuts down before assertions. Independent phase design review Approve; narrow verification follows.

- Hanging-provider phase fixture passes1/0 in0.26s (`g2-review-hanging-provider-phase-fixed.log`); independent actual-source review Approve. Production four-fix source remains unchanged. Full workspace qualification repeats once with the deterministic fixture; previous failed run remains retained.

#### G2 four Codex fixes workspace qualification — 2026-10-14

Source68f0b0a7: fmt1.66s, workspace all-target/all-feature Clippy15.70s, build5.73s pass.
Workspace2473top-level/0failed/15explicit skips341.94s; raw2478 includes5isolated child results.
Fixesbc011351/7cf81d3e/37e3065a/f79a1b2a cover bounded no-work exhaustion, direct rejection+pending main uncertainty, skipped routing crash certainty and legacy work proof.
Focused legacy suite6/0; hanging-provider phase1/0; independent fix and fixture reviews Approve.
Evidence target/qq-perf/ac3-stack-2026-10-13/g2-review-four-phase-final-*.log; prior full-run fixture failure retained.
Original fullG2 and autonomous-core acceptance, current-main integration and performance/T13 gates remain open.
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

### 2026-10-14 — G2 integrated deployed parent

- Resolve driver e9b5f2cb with G0parent7ff7358f in isolated fix/eng-1053-goal-review. Preserve main deployed47–50, goal51, and move request proof52/paid anchors53/legacy repair54. Keep source review fixes and exact main projection/authority semantics. Upgrade/rollback fixtures now use51/52/53 boundaries rather than unrelated main48/49/50; main47–50 preservation matrix also drops driver-only columns to reflect those old layouts.
- Added committed-work evidence to three upstream model-turn fixtures. Parent workspace75459 is still live; no child build or qualification yet. Original G2 sponsor/helper/later-turn/final-slot/multi-day and whole-plan/performance/T13 acceptance remain open.

### 2026-10-14 — G2 fresh Codex review

- Review5525585689 onpublished e9b5f2cb raises preparing-prompt cancellation fence4276954471, early post-start knownzero seed4276954480, and finite-cap rejection tracking4276954486. First two require regressions/fixes. Independent exact-source analysis finds direct tracking already enabled by runtime goalInput ORflag; add six public Goal/prompt × token/cost/both-cap priced529 cases, asserting retry pause, multiple attempts, direct ownership and zero accounting before deciding that finding.
- Work on isolated fix/eng-1053-review-fences frome9; integrated schema54/sponsored-test branchc397f48c preserved. No source behavior change or new review request yet.

- Exact e9 behavior baseline: six priced Goal/prompt × token/cost/both-cap529 cases pass1/0 in2.01s (`g2-review-finite-rejection-baseline.log`). All pause for ProviderRetry after multiple attempts, carry direct goal_id with no paid-owner anchor, spend0runs and retainknown0direct/inclusive/goal totals. Independent source analysis confirms env tracking already ORs directGoalInput. Reported P1path is not reproduced; no production patch for that claim. Test publication waits other two fixes and workspace qualification.

### 2026-10-14 — G2 integrated migration fixture

- Integrated345f3c00 fmt/Clippy/build pass. Full core1187/1/4skips fails only the rollback fixture retaining pre-integration expected49 while its setup and actual unchanged schema are52. Corrected expected52; focused migration test1/0 in0.75s, independent review Approve. Full publication checks repeat; no production change.

### 2026-10-14 — G2 second review corrections in progress

- Isolated `fix/eng-1053-goal-review-outcomes` starts at published `c18e831a`; expected-red regression commit `de71bbc6` reproduces explicit-stop restart, lost replay wake-up and discarded provider markers. Provider replay/empty reasoning and command-wake fixes pass their focused regressions.
- Explicit client stop pauses the same active GoalId at its current revision; runtime settlement and replacement identity remain separate. Three-phase stop regression passes queued/preparing/running; independent review identified distinct-ID repeated-cancel preparation release and replay signalling for earlier-root paid children. Fixes and controls are in progress.
- Codex no-first-turn schema51 dispatch recovery finding remains open for migration55; full workspace qualification and publication are pending. Existing performance and full autonomous-core acceptance are not closed.

- The expanded admission suite passes 15/0 in18.39s, including distinct-ID repeated preparing stops, newer-revision stop versus runtime cleanup, replacement identity, fault rollback and earlier-root child replay. Independent re-review Approve; workspace Clippy passes. Replay capture lookup tolerates a subsequently deleted run. Full workspace and migration55 remain pending; no publication claim.

- `legacy_no_turn_dispatch_stays_unknown_after_upgrade_and_recovery` reproduces the remaining finding on schema54: pending flag is false despite the durable WaitingForProvider event,0passes/1failure in1.27s (`g2-review2-no-turn-upgrade-red.log`). Schema51 case is specified but not claimed executed after the first assertion. Migration55 follows; no red change is pushed.

### 2026-10-14 — G2 legacy dispatch repair checkpoint

- Migration55 runs after published54, restoring latest-wait-after-completion pending evidence through a temporary indexed candidate table and one event scan, plus ambiguous owned-helper and routing admission flags. Measured turn rows, known routing scalars and terminal rows remain intact.
- Initial no-turn54/51 upgrade regression passes1/0 in0.69s. Prefix/completion/rejection controls and typed malformed-event rollback pass2/0 in1.89s (`g2-review2-upgrade-controls.log`): no send, completed zero, measured prefix followed by unresolved second send, rejection and terminal preservation, plus repeat recovery.
- Independent review identified candidate-workspace validation and pre-SQLite-JSON byte bounds; both corrected. Fresh stores have no store_id until after migrations, so the repair requires stored identity only when an event is decoded. Helper/router matrix, oversized/wrong-workspace rollback fixtures, final review and full qualification remain open. No migration completion or publication claim.

- Goal-wide current checkpoint suite reports91passes/1failure/1skip in57.44s: only `real_between_run_compactor_records_goal_dispatch_and_known_rejection` times out awaiting terminal output. Isolated reproduction is running; no broad green claim. A follow-up independent bounds review found that candidate-workspace prevalidation did not cover the global JSON scan. Empty candidates now skip parsing; otherwise global prevalidation bounds every scanned envelope first.
- Expanded rollback control passes2/0 with three fault variants: malformed typed activity, matching-run evidence moved to another workspace, and an oversized envelope in that other workspace. Independent correction review approves. Helper/router upgrade matrix and final qualification remain pending.

- Compactor timeout independently reproduced0/1 in31.05s, then stage isolation0/1 in31.13s identifies the first cancelled-helper case. After helper drain, `reload_reserved_messages` returnsNone for the retained cancelled owner; old execution cleanup removed only registration, stranding its queued/preparing row. Post-helper and post-pruning paths now use guarded prepared settlement with Cancelled.
- Extended existing real-compactor regression asserts owner Cancelled and released preparing pointer after helper drain. Focused five-case fixture passes; independent drain-fix review Approve. Exact output is `g2-review2-compactor-drain-green.log`; full goal suite and workspace qualification still follow.

- Final focused qualification at production `ebfcb8a9`: `cargo test -p qq-core goal` passes all executed goal tests, including real between-run compactor cancellation and the enabled-goal compaction soak; exact unit/integration counts are in `g2-review2-goal-suite-final.log`. Workspace Clippy passes in16.78s. Publication remains held by socket-blocked full-workspace gates; helper/router upgrade matrix and complete autonomous-core acceptance remain unfinished.

### 2026-10-14 — G2 legacy helper/router upgrade qualification

- Added `tests/goal_upgrade.rs`:28 fixture cases cover schema51/54 × in-run/between-run helpers with seeded no-send zero, pending dispatch, measured37 prefix, and prefix plus second pending send; routing covers skipped, pending, completed zero, measured, usage-only and cost-only receipts. Every case reopens and recovers twice, preserving prefix bytes and independently known totals without another goal-run charge, accounting change or event.
- `cargo test -p qq-core goal_upgrade -- --nocapture`:3passes/0failures in7.59s, including both new matrix tests. Independent read-only acceptance review approves; no critical helper/router acceptance gap found beyond separately covered main and rollback tests. Raw `g2-review2-helper-router-upgrade.log`.
- Migration55/current driver is still unpublished. Integrating current-main parent, full current-head qualification, hosted checks/review and the remaining G2/full autonomous-core scope remain open. Publishing awaits the pending workflow-exception decision for denied local test sockets.

### 2026-10-14 — G2 combined parent and cancellation fixture repair

- Driver integration `fd873edb` includes current-main G0parent3d4ea08f. Independent source integration review Approve; combined Clippy passes52.34s. Full workspace run11302 remains live with known denied-socket failures.
- Existing goal-less auto-compaction cancellation fixture assumed the owner finished before the helper. Reproduced1failure/2.32s. Retained preparing ownership requires helper drain before owner terminal publication; fixture now verifies both Cancelled, exactlyonce each, helper-before-owner, no compaction marker, and final owner summaryIdle/queued0/activeNone. Focused final run passes (`g2-review2-cancel-order-final.log`); independent review approves this contract correction with the helper-cancelled assertion retained. No production change.
- User's request to synchronize both PRs and existing authorization to publish the stack govern publication. The prior extra confirmation gate for denied test sockets was unnecessary. Publish concrete conflict repairs with exact blocked test results; hosted CI and current-head Codex review remain required, and no full-workspace green or merge readiness is claimed.

### 2026-10-14 — published sync and remaining fixture qualification

- Atomic push publishes G0parent3d4ea08f and driverbe13240f. Live GitHub confirms both mergeable, non-draft, with driver based on updated parent; mergeable_state unstable reflects unfinished checks, not conflicts. New-head Codex eyes/running summaries and hosted CI are confirmed. Prior extra socket-permission approval gate is withdrawn under the user's existing publication/synchronization authorization.
- Combined fd873 workspace run terminal:2,425 raw passes,168failures,15skips.160socket-denial failures+3 unavailable OAuth callback listeners are environmental. Five other failures are guide schema54 versus55 and four fixtures expecting owner terminal publication before preparation/helper drain. Auto-compactor fixture fixed in publishedbe13240f and passes1/0 in2.58s.
- Remaining three Store/runtime settlement fixtures now explicitly release simulated drained reservations or release the held loader before expecting terminal events. They retain start refusal, exclusivity, stale-panic protection, no provider poll, exactlyoneCancelled, cleared preparation and returned permit checks. Full settlement subset28/0 in4.58s; guide compatibility1/0. Independent fixture-contract review approves. Final fixture formatting/Clippy follow; full green workspace is not claimed.

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

### 2026-10-14 — G2 Git-policy parent integration

- Integrates parent Git-alias regression/fix `94ee5893` into driver `6d0462cb`. Production merges cleanly; append-only root ledger conflict retains both histories. User-requested GitHub CLI resolution confirms all previously fixed threads closed; only parent Git-alias thread awaits publication. Focused classifier and workspace checks follow. No merge performed.

### 2026-10-14 — G2 unsettled spend, crash-start accounting and neutral stops

- Three current-head Codex findings reproduce3failures/0passes in1.61s: automatic driver reused allowance with an uncancelled sponsored child, route-less RunStarted retained NULL accounting, and neutral interrupted/cancelled settlement reset retry streaks. Test-first `dc7ab93a`; logs `g2-review3-red.log`.
- Scheduler now waits for every unsettled sponsored obligation, including uncancelled queued/running children. Direct start atomically seeds known-zero only for captured-goal/sponsored work without routing/admission/pending evidence; measured and unknown routing prefixes remain authoritative. Neutral stops retain streaks while overall goal exhaustion still wins. No new store hop or schema.
- Regression fixture checks queued/running drainage and release, interrupted/cancelled streak preservation, route-less close/reopen recovery direct/inclusive accounting, and measured/unknown real routing receipts. Initial release assertion confused the helper's next-wake return with queue admission; corrected assertion reserves the newly queued goal run. Focused final and full goal qualification are running; no publication claim.

- Same spending invariant extended to the common reservation query after a fresh-prompt counterexample reproduces0/1 in1.23s (`g2-review3-prompt-fence-red.log`). Active-goal prompt candidates stay queued behind other unsettled sponsored obligations; candidate id is excluded and candidate selection preserves unrelated-session eligibility. Three corrected regression tests pass; broader goal suite follows.
- Earlier full goal subset found the cancellation-replay fixture depended on creating the forbidden automatic-root/older-child overlap. Fixture now explicitly creates the legacy queued state, restores paid ownership, and verifies queued-root cancellation/replay still cascades to that earlier child. The fence is not weakened to satisfy the old setup.

- Final focused regressions and complete goal filter pass after the common-reservation fence and legacy replay fixture correction; exact unit/soak counts are in `g2-review3-all-goals-final.log`. Independent review finds no concrete blocker in scheduling, no-send certainty, neutral streak preservation or legacy cancellation replay. Workspace formatting/Clippy/build and integrated parent qualification follow.
### 2026-10-14 — G0 bounded execution-surface review

- One independent pass over shell/wrapper/interpreter/configured-helper/explicit-hook/implicit-alias paths identifies tar checkpoint hooks, rg preprocessors, sort compressor commands and fd list-details execution. Tar and rg harmless probes execute their named programs; sort/fd local help documents their exact command behavior. New12-command parameterized regression first fails on tar0/1 in0.00s.
- Treat tar/rg as opaque configured launchers; refuse sort compressor spellings including GNU long-option abbreviations, and fd list-details/-l combined short flags. Controls retain ordinary sort/fd/find behavior and all empty managed-policy behavior. Focused classifier and independent correction review follow; no arbitrary-executable sandbox claim.

- Bounded policy correction passes all12classifier tests, workspace Clippy and build13.02s; formatting/diff checks pass. Independent review caught the missing long fd spelling before publication; exact --list-details added and full classifier rerun passes. No empty-policy or ordinary approval behavior changes. Earlier Git-only full-workspace run is finishing; complete current stack qualification follows integration.

### 2026-10-14 — published parent and final driver review integration

- Parent `0e85e5fc` is published. Git-alias thread replied/resolved against the fixing commit; all9parent threads now resolved. Current-head Codex review and fresh CI are running. Controlled turn-overhead run38687525601 is live on exact parent head against9868038c/common fixture9b0130cb; no performance pass claimed.
- Driver review correction `cecd6524` passes103goalunit/0failures/1skip43.86s plus enabled-goal compaction soak1/0 in3.17s. Integrates the parent's bounded execution-surface fix without changing driver source; both ledger histories retained. Workspace Clippy/build and final integrated checks follow.

- Integrated code `1969ea29` passes all12classifier tests, formatting, workspace Clippy17.23s and build; prior driver goal qualification103/0/1skip plus goal soak1/0 remains applicable to unchanged driver code. All three new findings have published-ready corrections and independent review approval. Final combined workspace run is launched with publication; known sandbox socket/OAuth failures are reported, not waived or called green. Parent turn-overhead run38687525601 remains live.
### 2026-10-14 — G0 recipe and package execution family

- Current Codex review names `make --eval='all:;cargo test' all`. Harmless make recipe probe prints its execution marker; regression reproduces0/1 in0.00s (`g0-review-recipes-red.log`). Recipe/package/environment drivers now use the existing opaque-program rule under nonempty managed denies, including make variants, Cargo aliases/build scripts, package exec and environment hooks. Empty policy and ordinary approvals remain unchanged.
- Safe-control commands now use printf rather than a Cargo plugin name, because Cargo is no longer treated as transparent under managed checks. A separate literal-word/launcher regression retains direct, shell, nice and stdbuf boundary coverage. This intentionally tightens only managed completion-check authority; design amended. Focused classifier and independent review follow.
- Published driverd5deaffc final full local workspace finishes2,440rawpasses/163failures/15skips:160socketpermission failures+3OAuth callback listeners. No code failure remains in that run; this is still not a green local workspace result. Parent turn recorder38687525601 completed successfully and retained its raw artifact; metric analysis is pending, not a performance-gate pass claim.

- Recipe/package/environment correction passes all14classifier tests, formatting, workspace Clippy16.32s and build. Independent focused review Approve; no empty-policy or ordinary-approval behavior change. Parent/G2 stack publication will rerun current-head hosted checks and genuine Codex reviews; no merge claimed.

- Controlled parent run38687525601 records all30A/B+30A/A pairs and uploads artifact11950189815 (sha25672e2ca529be615986aef14d70d1ebc400d434b3badbd56f6a46f8fdcb56ee6c9). Completion is not a performance pass: artifact download into this sandbox is DNS-blocked, so measured p95/control interpretation remains pending. Recorder now emits its existing six summary rows to job logs after preserving raw receipts; this adds visibility without changing measurements or gating.

### 2026-10-14 — G2 recipe-runner parent integration

- Integrates parent recipe/package/environment authority correction2b6fc845 and recorder result-visibility change02fd3ff8 into driverd5deaffc. Source merges cleanly; appended ledger sections retained. No G2 feature expansion. Parent classifier14/0, Clippy16.32s/build and independent review pass; recorder16CLI+4fixture tests pass. Integrated focused/classifier and Clippy/build checks follow.

### 2026-10-14 — hosted shell-drain fixture race

- Current driver CI38687997341 fails only `dropped_shell_waiter_is_reaped_before_drain_with_full_live_output`:1215corepasses/1failure/4skips. Its producer can exhaust the bounded best-effort live-output budget before the test consumes the PID; waiting for a second full channel then times out. Source forward_shell_chunk explicitly consumes dropped-full bytes. Delaying PID consumption100ms deterministically reproduces0/1 in5.10s (`g2-shell-drain-fixture-red.log`).
- Shell fixture now publishes PID and waits for an explicit workspace-file release before producing the flood. Slow consumer delay remains. The channel-full observation, drop, drain, actual PID exit and uncancelled-token assertions are unchanged. No production change or increased timeout. Focused verification and independent review follow.

- Controlled slow-consumer shell-drain fixture passes after the handshake; independent focused review Approve confirms all original drain/exit/cancellation assertions remain. Integrated recipe-policy classifier14/0, formatting, workspace Clippy/build also pass. Final test-only qualification follows before atomically publishing both current stack heads.

### 2026-10-14 — activity-write accounting and neutral provider retry history

- Test-first commit2df131d0 reproduces both current #309 findings: a first WaitingForProvider event-write rollback leaves public accounting NULL despite zero provider polls; six Cancelled/Interrupted no-work stops reduce the next outage delay from120000ms to60000ms. Evidence: `/tmp/g2-activity-red.log`, `/tmp/g2-backoff-red.log`.
- Fixf7570eb5 retains the accumulator in activity/owned-summary dispatch and Prepared identity/context failure settlement; durable pending-request evidence still protects unknown spending. Backoff filters neutral no-committed-work outcomes before the five-retry lookback, retaining committed-work and completion resets. Same regressions pass1/0 each (1.39s/3.65s); independent focused review Approve.
- Integrated current main3b763ea1 into parent/driver without conflicts. Hosted driver38689819487 failed the new lint's79/77 sleep ceiling; the two added goal routing-duration waits now advance paused Tokio time at held admission/loading boundaries. All eight expiry/cost/origin cases pass in the existing public regression1/0 (13.37s), preserving zero dispatch and no durable helper-admission assertions. Current affected-package checks follow; no merge or whole-G2 completion claimed.
- Correction before publication: paused-clock alternatives do not prove Store-worker duration expiry because that blocking thread reads real elapsed time. The scoped-clock attempt reproduced routing dispatch1 where0 was required (`/tmp/g2-routing-clock-scoped.log`); no paused-clock change ships. Retain the original two real-time expiry waits with the lint's explicit real-clock exception and unchanged durations/assertions. This exact eight-case regression passes1/0 in15.41s (`/tmp/g2-routing-real-clock.log`); independent review agrees the existing held-writer/router signals expose no public expiry event. No ceiling increase or production clock hook.

### 2026-10-14 — closed managed-check execution policy

- SQLite `-cmd '.shell cargo test'` reproduces the next configured-launcher bypass0/1 (`/tmp/g0-closed-policy-red.log`). Test-firstfc75959e expands the existing table to SQL clients, an unknown custom driver, path-qualified replacement programs, environment/startup assignments and shell login forms.
- Replaces the expanding opaque-program list with a closed match of inspected command forms. Unknown source executors refuse authority; deny-prefix parsing remains unrestricted. Existing shell/launcher/find/fd/sort constraints and all empty-policy behavior remain. Architecture and the AGENTS enforcer row record the execution model, including its trusted-executable/operator-environment boundary.
- All14classifier tests pass (`/tmp/g0-closed-policy-fixed.log`); independent focused review Approve. Earlier parentdd02 local check:fmt/lint/Clippy pass,2237/2307tests pass with65socket denials,3OAuth callback failures and2unclassified timeout failures; this is not green qualification. Final-head affected quick check and hosted checks follow; no performance waiver or merge.

### 2026-10-15 — recovered prompts and wait-aware retries

- Review test-first62856ec7 reproduces recovered queued UserPrompt cancellation on a normal same-goal edit and ProviderRetry shortening model wait. Corrects non-draining cancellation using exact autonomous origins; pause/clear/replacement drainage remains broad. Active terminal policy retains waits for provider backoff, ordinary failure, no-progress and per-run exhaustion, with absolute deadline precedence. Architecture amended; no schema/protocol change.
- Focused recovery/wait/ownership tests4/0 (21.92s). Expanded wait family additionally covers short wait vs backoff, no early queue, reopen and other automatic outcomes; exact final table1/0 (3.22s). Broader goal filter106passes/1fixturefailure/1skip; the fixture expected a change event despite unchanged state, then corrected setup retained that assertion and passed exact rerun. Independent focused review Approve. Current-head gate follows.
- Cache finding does not reproduce under supported ownership: both second opens fail StoreBusy before SetGoal, worker retains OS lock before SQLite through close, clones share StoreInner, successor reloads durable presence. Existing exclusive-owner and busy-before-create tests pass. Explanation posted in PR309 body; inline reply and thread-resolution tools reject actions as approval-required under approval-policy never, so no thread resolution claimed.


### 2026-10-15 — PR308 current-main merge-readiness checkpoint

- User prioritizes PR308 then PR309 before new autonomous-core PRs. Current main0b4f29e3 integrated in c2b56814; the sole conflict was root.md and both histories are retained. Legacy-site repair4f4101b1 preserves76 exact sleep identities, removes one exception already absent on main, and retains ceiling76. Independent integration review Approve.
- Exact clean4f4101b1 affected check against current main: fmt1.50s/lint1537files76/76sleeps1.99s/Clippy24.93s pass. Tests2246/2314pass,68fail,14skips; all failures classify as65socket EPERM and3unavailable callback listeners. No non-environment failure and no green gate claim. Overall210.70s; target/qq-check/last.json and immutable/tmp/pr308-current-main-qualification/.
- Hosted e0453af4 CI run38691879018 passes affected tests/smoke; genuine description bot+1 reaction569905016 and11/11review threads resolved. Those facts apply to the published head, not unpublished local integration. Current remote main verification0b4f29e3 explains the conflict despite stale base_sha in the PR response.
- Controlled record38687525601 artifact11950189815 remains unexpired; connector returns the ZIP file reference, but local retrieval still fails DNS. Metrics interpretation, quiet-host/performance/full/smoke qualification and GitHub publication remain open. Earlier automatic approval review rejected equivalent GitHub writes; no bypass or hosted merge-ready claim.


### 2026-10-16 — rebase over Jev provenance and retained installer

- Current main9e010507 adds Jev provenance60dd03db and installer287 after the previously published stack. Resolve PR308 deadline overlap by retaining the exact main fixture, avoiding duplicate spawn observers/clocks; remap the same76sleep identities without raising the ceiling. Independent static review confirms production Jev callers and goal branches remain intact.
- Existing v33Jev golden fails1/1 because incoming current constructor now supplies delegate_model/delegate_request_id. Add those two existing optional provenance fields to the current-version fixture; historical fixtures remain unchanged. Red evidence/tmp/pr308-jev-golden-red.log. Final checks follow.
- Published PR30982b2d7b7 has successful affected/Clippy/tests/smoke run38829861435; duplicate38829861446 is cancelled. Do not invent a current-head test failure or treat the cancelled duplicate as a green full gate. Rebased stack needs fresh CI and remains unmerged.

### 2026-10-15 — PR309 current-parent qualification and release-doc integration

- Prioritize PR308 then PR309; later autonomous-core slices remain parked. Exact clean driver2d043cb8 affected gate against parenteaf76f28: fmt1.51s, lint1540files75/75sleeps1.95s and Clippy21.54s pass; tests2040/2101pass,61socket-EPERM failures,14skips. Overall264.85s; gate remains red. Immutable evidence `/tmp/pr309-current-parent-qualification/last.json` and `test.log`.
- Fresh review2 compilation proves13/13 recovered-prompt/wait/ownership/shell-barrier regressions pass (3.548s); independent focused source review Approve. Both valid remaining Codex findings are fixed locally in817f03ca; the two-owner input is refused by StoreBusy before SQLite opens. Live threads: PR30811/11resolved; PR30917/20resolved. No unpublished fix is claimed resolved.
- Integrate PR3081608d8a1 and exact main9df32a5 release preparation. Conflicts only in docs: retain schema55/protocol33, current open-review status and both dated histories. No runtime/source change. Final docs_truth24/24pass (0.166s), lint1542files75/75sleeps and workspace fmt pass. PR308 docs_truth24/24pass (0.170s), lint1539files76/76sleeps pass. Existing red socket-dependent qualification remains open.
- Publication, inline replies and thread resolution remain unavailable after automatic approval review rejected those GitHub writes; no equivalent bypass attempted. Controlled performance artifact cannot be retrieved locally due DNS restrictions. Full/smoke, named performance and hosted final-head gates remain open; neither PR nor full autonomous-core is claimed merge-ready or complete.


### 2026-10-15 — consolidated rebase and requested Codex resolution

- User requests rebase, then resolve Codex conversations. Preserve pre-rebase refs backup/pr308-before-squash-rebase-20261015 and backup/pr309-before-squash-rebase-20261015. Consolidate PR308 onto verified main9df32a5 as8dfb7108; consolidate driver onto8dfb7108. Both implementation trees match their previously resolved checkpoints exactly; this receipt is the only additional content. Old test-first and review-fix commits remain reachable through backup refs.
- No conflict markers or unmerged paths; clean pre-receipt worktrees. PR308 rebase validation passes fmt/lint/Clippy; affected tests running under session45420, log/tmp/pr308-rebased-check.log. Prior integration tests and current docs results remain recorded above; no new green overall claim.
- Attempt to post the concrete ownership-lock counterexample to PR309 review comment4279909067 is rejected again: MCP tool call requires approval, but approval policy is never. No reply or resolution occurred. Do not resolve the two valid fixed findings until fixes are published. No alternate write route attempted; hosted refs remain unchanged and full goal unfinished.

- Rebase gate finished: fmt1.54s/lint2.34s/Clippy13.12s pass; tests2242/2314pass,72fail,14skips (237.67s). Failures comprise68socket/callback restrictions plus4checkpoint timeouts. All4checkpoint tests pass isolated serial rerun (52.110s), without code/assertion/timeout changes; the full gate remains red and performance qualification is not inferred. Reports preserved in/tmp/pr308-rebased-qualification/ and/tmp/pr308-rebased-checkpoint-recheck.log. No build process remains live.


### 2026-10-15 — v0.1.7 rebase published and existing Codex conversations resolved

- Main advanced to55936a1e through release318/docs321. PR308 rebased cleanly asb39fd753; PR309 rebased onto it as82b2d7b7. Runtime source unchanged by the release rebase. Explicit-lease atomic Git push succeeded; GitHub reports both MERGEABLE. Earlier connector approval failures did not establish CLI unavailability: installed gh and Git work; nix-shell daemon access remains denied.
- Current affected gate: fmt1.45s/lint1.99s/Clippy24.25s pass; tests2240/2314pass,74fail,14skips (398.28s).68socket/callback restrictions plus6session timeouts. Isolated serial6test rerun5pass/1checkpoint timeout (123.734s); no assertion/timeout change, no overall green claim. Reports/tmp/pr308-v017-rebase-qualification/ and/tmp/pr308-v017-timeout-recheck.log. Hosted final-head CI pending; full/smoke/performance qualification remains open.
- Ownership counterexample reply4289609637 posted and thread resolved. Published recovered-prompt fix reply4289652892 and wait-policy reply4289653929 posted and their threads resolved. Existing conversations: PR30811/11resolved, PR30920/20resolved. New Codex findings, if any, still require triage; no full autonomous-core completion or merge claimed. This local receipt follows the published implementation heads without restarting their CI.


### 2026-10-16 — current-main integration qualification

- PR308 local0edd0615 includes exact main9e010507, deadline conflict resolution and the current Jev golden repair. Protocol63/63pass; fmt1.51s/lint1.94s/affectedClippy26.09s pass. Affected tests2250/2319pass,69fail,14skips (251.38s), all69explicit socket/callback restrictions; no other failures reported. Evidence/tmp/pr308-jev-final-qualification/ and/tmp/pr308-jev-protocol-check.log. Independent final review Approve.
- PR309 is rebased onto that corrected parent; workspace all-target/all-feature Clippy passes31.00s, workspace fmt and diff checks pass. Existing75sleep identities retained; no new runtime behavior introduced by conflict resolution. Published82b2d7b7 still has successful CI38829861435 and cancelled duplicate38829861446, not a reported failing test run.
- Publication remains pending in this session: Git SSH rejects mode777userconfig; a private config copy preserves settings but control-socket access/DNS fails. HTTPS Git cannot resolvegithub.com; repeated direct gh Git-data commit requests fail connecting toapi.github.com. One tree request succeeded and its hash matches the exact local parent tree; no branch ref was updated. Prepared scripts/payloads and verified bundle/tmp/pr308-pr309-jev-rebase.bundle preserve the work. These are specific current CLI failures, not proof that all GitHub writes are unavailable. No CI/merge-ready/completion claim.


### 2026-10-16 — exclude accidentally merged PR319 from the goal stack

- User removed PR319 (Jev protocol/response/transport provenance) from main. Clean main73ff0647 excludes60dd03db; published30881eee50 still carried replaybf786880 and30919c255aa inherited it. Rebuild from reviewed pre-Jev foundationsb39fd753/82b2d7b7 onto clean main, preserving current installer/docs consolidation and retired-plan deletion.
- Independent exclusion audit Approve: both candidates descend from73ff0647; neither removed/replayed commit is reachable. Runtime source and manifests exactly match each corresponding pre-Jev baseline. All4PR319-added files and added approval/reviewer provenance fields are absent. Original G0/G2 source behavior is preserved; earlier Jev-integration receipts above are historical, not current source claims.
- Keep both PRs unmerged until corrected heads and their CI are verified; do not restore the removed Jev patch to solve conflicts. Exclusion evidence and final qualification follow.


### 2026-10-16 — PR309 final sponsored-budget and scheduling review

- Reconfirmed published308f078ccaa/3090b15c8c7 exclude PR319: runtime/manifests match pre-Jev baselines, removed files/provenance and ancestry absent; clean main73ff0647 retained. Both hosted CI runs passed.308received genuine Codex thumbs-up;309received4new findings, all reproduced before implementation.
- Regression commitsab8d1dd6/21942446 reproduce lost cancellation replay wake, generic automatic routing input, expired sponsored-child admission and request cap256against inherited60. Fix retains replay receipts, routes captured objectives only for Goal-origin work, intersects all paid-ancestor absolute deadlines at claim/routing/main start, and shapes child provider output including routing spend. Independent reviews approve both scheduling/routing and revised ancestry admission; no ancestor goal authority granted.
- Focused goal_review7/7pass (2.39s), including own-goal child deadline variants. Final repository gate and hosted review follow. PR308 remains unmerged: normal async merge failed last-pusher approval and active github-pages deployment requirements. No protection bypass, fabricated review or deployment. Four309threads remain open until fixing commits are published.

### 2026-10-16 — post-merge G2/G3, G4 and G5 stack

- User requests three reviewable PRs: G2 closeout+G3 (ENG-1063), G4 surfaces (ENG-1064), G5 evidence (ENG-1065), parent ENG-1011. Start clean isolated worktree `/tmp/qq-goal-delivery-20261015/g3` on exact main3d58b3d3; root is sole writer. Preserve #293 cooperative preparation drain and all #309 review fixes; exclude removed PR319.
- Pre-change optimized turn_overhead diagnostic:3iterations×1000executed/durable calls,9compactions each, all assertions pass. Median gaps at10/100/1000 are92.690/102.274/57.591ms; broad sample variation is not quiet-host/tail qualification. Exact source/binary/fixture/raw samples: `/tmp/qq-goal-delivery-20261015/baseline.json` and `g3-baseline-turn-overhead.log`. Candidate comparison and acceptance follow.
- Reuse prepared test-first runtime/check/surface work rather than replay old merge history. Independent static G2/G3 review confirms principal Unix acceptance paths; add literal simulated20-minute permit-wait coverage and resolve/document platform boundaries. G5 provider selected:openai-codex/gpt-6.1-sol; no paid allowance inferred.

### 2026-10-16 — G2/G3 integration and G5 authorization

- Integrated G2 paid-request admission, caller remainders and final-slot handoff onto main3d58b3d3; retained all-sponsor deadlines and #293 cancellation/drain ownership. Corrected the older tiny-total child fixture to retain a binding60-token output allowance with room for request input; focused regression1/1 passes.
- Ported G3 exact check authorization, bounded process execution, independent model audits, historical results and durable interrupted-check holds; configuration/policy foundations move into this PR because execution must enforce them. G4 retains the TUI, CLI and foreground surfaces. Protocol34/schema56–58 are reserved in root; descriptor15 is unchanged.
- Driver-only and checked40-run scenarios pass with actual process kills and simulated multi-day waits/outages. Added literal20-minute held-permit acceptance; integrated final checks remain pending. Independent read-only integration review of53043b99 found no defects and verified no PR319 provenance.
- G5 user authorization: openai-codex/gpt-6.1-sol through subscription only, with no paid API fallback. A failed subscription/auth path must stop rather than select a billable provider. Live evaluation has not started.

### 2026-10-16 — G2/G3 qualification and push-boundary regression

- Runtime ae835237 passed full workspace2686/2686 (15 deliberate skips), doctests/all-feature Clippy, minimal-provider211/211 and real-binary smoke6/6. Both multi-day scenarios passed under full load. Fresh manual captures prove8provider request bodies byte-identical to main3d58b3d3; an initial stale-cache comparison was discarded, and candidate was rebuilt before comparison.
- Goal-less optimized loop diagnostic:10/100/1000-turn medians92.69/102.27/57.59ms baseline →17.92/17.57/18.92ms candidate;3×1000 durable calls,0duplicates,9compactions each. Shared-host variation prevents causal/H0 claims. Raw logs, hashes and immutable binaries: /tmp/qq-goal-delivery-20261015/. G4 rendering baseline is captured separately.
- First required push failed because inherited Git-local variables let a temporary Git fixture change this linked worktree's HEAD/index; no physical source blobs changed and nothing was published. Restored owned metadata, reproduced on disposable regular/linked pushes, and fixed the hook boundary. The final gate must run on the resulting head.

### 2026-10-16 — final fixture qualification

- Required push gate exposed a sponsored-approval test race: a five-second real-time keeper delayed scheduler timers and let virtual deadlines race real Store I/O. Setup now uses the normal clock with a one-hour fixture allowance, then crosses both clocks explicitly only after actual approval and paid-owner assertions. Same regression passes1/1 in1.72s; independent read-only review approved. Runtime bounds are unchanged. Final-head push gate follows.

### 2026-10-16 — published stack and current-main reconciliation

- Opened #330 (G2/G3) and #331 (G4 on #330), both non-draft with ci:full. Final old-base gates passed2687/2687 and2733/2733 respectively, plus doctests/Clippy/minimal profiles. Publication revealed bare-repository fetches had left origin/main stale; fixed the owned remote fetch mapping and refreshed explicitly to3e34eea2.
- Rebased #330 onto current main, preserving #325 fixture stabilization and #299 auth changes. Only conflict was legacy sleep locations: retain main's73 sites and lowered ceiling, then refresh positions against integrated source. Main auth source is unchanged by this PR. Resulting-head qualification follows; previous reports remain evidence for their exact old heads.

### 2026-10-16 — preparation fixture entry fence

- Rebased full gate exposed #293's preparation test expecting a checkpoint stage after its real100ms setup deadline could already expire. Narrow and8concurrent diagnostic copies pass; source review confirms an expired first poll can emit entered only during cancellation drain. Production snapshots the stage before cancellation correctly.
- Replace the ambiguous Notify with a bounded entry receipt proving not-yet-cancelled, establish that stage before explicit virtual expiry, and retain exact stage/no-request/cooperative-drain assertions. Narrow corrected regression1/1 passes in1.74s; independent review approved. No production bounds changed. The entry receipt encodes the fixture ordering rather than loosening the verdict. Final-head gates follow.

### 2026-10-16 — G3 managed prompt ceilings review repair

- New Codex finding330/f1 reproduced independently on originating #330: unmanaged prompt claim lacks managed per-run limits (g3-managed-caps-red.log). Ported test-first792e2d95/ba072461 from the isolated subscription-only review task as a51fef7e/978c3d3a.
- Intersect all ten execution-limit fields with current managed policy at claim and refreshed admission, retaining stricter caller limits and final-slot/audit bounds. Independent fixed-snapshot review found no source defect; identified that initial regression only proved claim.
- Extended the regression to tighten policy after reservation, require Reprepare, retry actual start and inspect durably persisted limits. Narrow test passes1/1 (9448918a); broader/final-head gate follows after remaining related findings are integrated. Nothing from this repair is published yet.

### 2026-10-16 — G3 paid input and sponsored approval review repairs

- Ported dense-input regression and conservative paid-admission bounds from the live review task. Its actual red receipt7fb496ec dispatches1request despite a binding token/cost remainder; repaired narrow pair c6762cc6 passes2/2 after integration with managed ceilings. Requests and owned summaries reserve byte upper bounds rather than occupancy estimates.
- Independent review rejected the first sponsor-duration repair: RunStarted excludes preparation. Added a real held20-minute preparation row;3079a366 reproduces Active instead of needs_user/approval. A first attempted post-event repair was too late because runtime deadline enforcement drains children before emitting exhaustion; discarded that experiment.
- Retain bounded live monotonic deadlines through paid execution lifetime, capture expiry on the executor and recheck bound active/non-cancelled owners in the existing store transaction. Focused40dd7664 passes2/2 including absolute deadline, per-run duration, preparation delay and wall-clock lag rows; independent mechanism review and broader final gates follow. No protocol/schema or dependency changes.

### 2026-10-16 — G3 review repair integration gate

- All four hosted330findings have local test-first repairs. Policy pool saturation now awaits a cancellation-safe bounded semaphore rather than durably pausing valid goals; its focused regression passes1/1 (031ffd42). Current goal refresh already bounds that wait by cancellation and deadline.
- Independent read-only reviews approve managed ceiling source plus strengthened actual-start test, conservative paid input, and monotonic sponsor registration (a8933e72). Rejected timestamp-based and post-event experiments do not define the resulting source. Four-row sponsor test and ordinary approval regression pass2/2, including nonzero preparation and wall-clock lag.
- Preserve all73existing sleep sites; final head full pre-push gate and runtime smoke follow. Review threads stay open until fixing commits are published and final evidence is verified.

### 2026-10-16 — G3 conservative admission outage fixture

- Full gate on1763403e stops at the outage fixture's10000-token row. Exact diagnostic rerun proves TotalTokens refusal before dispatch, not provider-retry/accounting failure. The new paid input bound correctly includes complete request bytes.
- Keep caps finite but leave100000tokens of room to exercise the intended HTTP529 rejection; retain every retry, direct ownership, known-zero accounting and zero-run-charge assertion. Full goal_admission group passes19/19 (4c2ac02f), including token-only/cost-only/combined rows for goal and prompt origins. Resulting-head full push gate follows.

### G3 full-gate descendant exit correction

The full gate on `2a75c4aa` caught a descendant still in state `R` when completion
check cancellation returned. A narrow rerun passed, but independent source review
confirmed the real gap: killpg plus leader wait did not observe descendant exit.
The test now pins the descendant with a pidfd before cancellation and checks
terminal readiness at the return boundary. It retains its original /proc check.

Linux completion checks now retain the WNOWAIT leader pin while confirming every
matching process through pidfd readiness, including zombie leaders with live
threads. A bounded off-executor /proc scan (65,536 entries, 4,096 members, 4,096
bytes per stat) is repeated after observed exits to close the fork/enumeration
window. All scans and waits share one five-second cleanup deadline. Identity is
checked around pidfd acquisition. Failure still reaps the shell but leaves the
process lease unconfirmed and the runtime failed, preserving workspace exclusion.
This adds Linux check-cleanup work only; ordinary prompt requests are unchanged.
Existing rustix enables its event API without a new dependency or version change.

Focused run `14ab4e7d-0c5e-408e-95fe-5d8af95054c4`: 5/5 process supervision
tests passed. Final full gate, smoke and independent source review remain pending.
