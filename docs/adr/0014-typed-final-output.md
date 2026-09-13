# ADR-0014 — Typed final output: a per-run contract compiled at admission, judged at the completion boundary, repaired within a bounded allowance

**Status:** Accepted
**Date:** 2026-09-12
**Deciders:** speed-first plan HC3 (headless contract)
**Implements:** [`headless-contract.md` § Gaps](../design/headless-contract.md#gaps-a-supervisor-currently-works-around)
(typed final output), [`architecture.md` § Structured Input And Steering](../design/architecture.md#structured-input-and-steering)

## Context

A supervisor that wants a structured report from `qq run` had to parse prose
or ask the model to write a file. The headless contract already fixed the
shape the fix must take: opt-in, generic (a local user, a CI job, and an
evaluation harness can all use it), off the default hot path, bounded in
schema size and repair work, persisted before publication, and additive on
the wire. Several designs satisfy the surface but differ in where the
authority lives and what the run loop pays.

## Decision

1. **The contract is per run and rides `SubmitPrompt`.** `OutputContract {
   schema, repair_turns }` is an optional field of the command, persisted on
   the run row (`runs.output_contract_json`, schema 27) and recompiled at
   claim, so a restart enforces exactly what the caller accepted. It is not
   part of the compiled agent plan: the plan is identity for a workspace,
   model, and profile; the schema is an argument of one invocation. Plan
   digests are unchanged.

2. **Compilation happens at admission and is the only place a schema can be
   refused.** `CompiledOutputSchema::compile` bounds the schema (64 KiB, 32
   levels, 4096 values, 0–8 repair turns) and accepts a closed keyword
   subset with no `$ref`, `$defs`, `pattern`, `format`, or conditionals. A
   schema outside the subset fails the command
   (`SessionRuntimeError::InvalidOutputContract`, HTTP invalid, `qq run`
   exit 2), never the run. The subset validates in time linear in the
   instance, so validation needs no timeout and no regular-expression
   engine. The runtime owns the validator; no schema crate is added.

3. **The answer is judged once at the completion boundary, after audit and
   steering.** The seam is the same one the final-answer audit uses: the
   turn that returned no tool calls, once steering has had its chance and
   the audit has passed or revised. A failure within the allowance pushes
   the assistant message and a runtime notice carrying the bounded errors
   (at most 8 KiB) and continues the loop; the repair turn is an ordinary
   model turn against the ordinary budgets. `repair_turns` counts for the
   whole run and is not reset by an audit revision or steering. When the
   allowance is spent, the run **completes** with `FinalOutput::Invalid`.
   No new `RunOutcome` variant: cancellation, failure, and budget
   exhaustion mean what they meant, and a completed-but-invalid answer is
   distinguished by the verdict, which `qq run` maps to exit 1
   (`task_failed`) because the codes are stable.

4. **The verdict is durable before it is visible.** `RuntimeEvent::Completed`
   carries the verdict into `settle_run`, which writes
   `runs.final_output_json` in the settlement transaction and publishes it
   on `RunFinished.final_output`; the snapshot exposes the same value.
   A verdict is written only for `Completed`.

5. **The model is told the contract up front.** A run with a contract gets
   the compact schema appended to its system prompt as an "Output contract"
   section. The prompt-identity digest therefore differs from the
   schema-less run, which is correct: it is a different prompt.

## Consequences

- Positive: a supervisor receives a parsed JSON value or a typed list of
  `<pointer>: <message>` failures on both `outcome` and `run_finished`,
  pinned by `v19` goldens. Nothing changes for runs without a contract:
  no allocation, no extra event, byte-identical default records after the
  version bump (`capabilities` gains four declared bounds).
- Cost: with a contract, one compile at admission (~37 µs for a 64-property
  schema) and one at claim, one validation per candidate answer (~11 µs
  for a 7 KiB answer), and up to `repair_turns` extra model turns. Default
  path `read_tool_loop` median 54.8 → 52.2 µs across 15 interleaved pairs
  (noise on a loaded host).
- Negative / risks: the keyword subset is deliberately small; callers with
  `$ref`-heavy schemas must inline them. Valid JSON is not a correct
  answer; the supervisor still verifies. A model that never produces JSON
  spends its full allowance before the typed failure — the allowance, not
  the runtime, bounds that spend, and the default is two turns.
- `PROTOCOL_VERSION` 18 → 19 because event and snapshot decoders are
  strict; `v18/` fixtures are retained decode-only. Store schema 26 → 27
  (two nullable columns; historical rows read as "no contract").

## Alternatives considered

| Alternative | Why not (now) |
| --- | --- |
| A new `RunOutcome::OutputInvalid` variant | Breaks every exhaustive match across core, client, TUI, and headless for a distinction the verdict already carries; exit codes are stable by contract |
| Contract on the compiled plan / profile | Makes plan identity depend on one invocation's argument and forces a plan compile per schema; the schema is per run |
| Validate on the session layer from persisted chunks | Re-derives the answer the loop already holds and cannot inject the repair turn; the loop owns the boundary |
| A JSON Schema validator crate | Adds a dependency with `$ref` resolution, regex, and format tables the contract forbids; the subset is ~400 lines with bounded behavior by construction |
| Provider-native structured output | Provider-specific and not universally available; may land later as an optimization behind the same contract |
| Reject the invalid answer as `Failed` | Discards a completed run's transcript and accounting semantics for a policy the caller can decide from the verdict |

## Evidence / references

- `crates/qq-core/src/output.rs`: `CompiledOutputSchema`, bounds,
  `OUTPUT_REPAIR_NOTICE`, `OUTPUT_CONTRACT_SYSTEM_NOTICE`.
- `crates/qq-core/src/lib.rs`: completion-boundary validation and repair
  turn; `RunCapabilities::with_output`.
- `crates/qq-core/src/sessions.rs`: admission compile, claim recompile,
  `settle_run` writes `final_output_json`; `store/schema.rs` schema 27.
- `crates/qq-protocol/src/sessions.rs`: `OutputContract`, `FinalOutput`,
  bounds; `tests/fixtures/v19/`.
- `src/main.rs` `prepare_headless`, `src/headless.rs`: `--output-schema`,
  `--output-repair-turns`, `final_output` on `outcome`.
- Tests: `output::tests::*`,
  `sessions::tests::a_valid_first_answer_completes_with_the_parsed_final_output`,
  `…an_invalid_answer_is_repaired_within_the_allowance…`,
  `…an_answer_that_never_validates_completes_with_the_typed_failure…`,
  `…the_contract_judges_the_audited_revision_and_a_revision_does_not_reset_repairs`,
  `…repair_turns_spend_the_turn_budget_and_exhaustion_publishes_no_verdict`,
  `…the_contract_survives_a_restart_and_is_enforced_by_the_recovering_runtime`,
  `…an_unenforceable_contract_is_refused_at_admission_and_creates_no_run`,
  `headless::tests::a_valid_typed_answer_rides_the_outcome_record…`,
  `main::tests::an_unenforceable_output_schema_is_invalid_configuration_before_config_loads`.
