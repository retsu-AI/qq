# ADR-0042 — The trust prompt is a client-side hold fed by the composition root; no protocol change

**Status:** Accepted
**Date:** 2026-09-24
**Deciders:** onboarding-ux OB7 (ENG-881)
**Implements:** [`onboarding-ux.md` § OB7](../plans/progress/onboarding-ux.md#2026-09-24-ob7-in-tui-trust-prompt-in-review); [`tools.md` § trust](../design/tools.md)

## Context

Bare `qq` in a workspace whose project configuration declares sensitive
sections (a model, providers, MCP servers, grants, packs) that no trust
record covers exited with `ConfigError::TrustRequired` and told the user to
run `qq trust`. Every other incomplete-start state (no model, OB1; no
credential, OB2) already opens the TUI and says what to do; trust was the
one remaining exit, and the one a new user hits first when cloning a
repository that ships `.qq/config.ron`. Trust is a decision about files on
the host that owns `trust.ron`: the process that reads the project files and
the process that records the decision must be the same one. The plan left
open whether the prompt is a new protocol hold kind or a client-side prompt
fed by the `TrustRequired` details.

## Decision

The prompt is client-side. The composition root (`src/main.rs
interactive()`) matches `TrustRequired` on its own `load_for_client`, renders
each `PendingTrust` into a plain `qq_tui::PendingTrustNotice` (path plus one
human line per `TrustDeclaration`: route, provider name and kind, MCP server
with command or URL, grant counts, pack ids — never a secret, argument, or
environment value), and opens the TUI with no model and no catalog. `App`
enters `Mode::Trust` while notices are pending; `t` and `s` emit
`Effect::ResolveTrust`, which the loop hands to a root-provided
`TrustResolver` and awaits like the external editor. `Persist` calls
`ConfigLoader::grant_pending_trust`, the same function as `qq trust`;
`Session` calls `RuntimeFactory::trust_for_process`, which stores
`(path, digest)` grants that `request_for_workspace` attaches to every load
request via `LoadRequest::with_process_trust` and that
`ProcessTrustFingerprint` folds into `PlanKey`, so a plan compiled while the
file was withheld is never served after the grant and nothing is written to
disk. The resolver returns the recomputed `TuiModelState` (client default,
catalog, credential remedies); `App::apply_trust_resolved` installs it, asks
the server for `Capabilities` and `Models` again, and the OB1/OB2 guidance
takes over. A TUI attached to a server it did not reserve gets no resolver:
`t`/`s` show `run qq trust on the server host`. Headless surfaces are
unchanged and keep failing fast.

## Consequences

- Positive: no `PROTOCOL_VERSION` change; the server never writes trust on
  behalf of a client; `qq trust`, the TUI's `t`, and `qq doctor` share one
  scan (`loader::scan_pending_trust`) and one write path; "this session" is
  a load-request input, so every server-side path (catalog, capabilities,
  plans) sees the same trusted set the client does.
- Negative / risks: the prompt exists only in a client that owns the server;
  a remote client sees the old error text. Process-scoped trust lives in
  `RuntimeFactory`, so an embedded server that outlives the TUI keeps the
  grant until it exits (it is the same process, which is the definition of
  the scope). Startup for an untrusted project pays one extra
  `load_for_client` after the answer. The embedded server is opened before
  the answer, so its `approval_timeout` is the not-yet-loaded default
  (none), exactly as `qq serve` behaves for a configuration that does not
  load yet.
- Follow-ups: when ADR-0015 enrollment lands, a remote client with an
  enrolled identity could be offered a server-side trust command; revisit
  the protocol-level alternative then.

## Alternatives considered

| Alternative | Why not (now) |
| --- | --- |
| A new protocol hold kind (`trust_required` event, `resolve_trust` command) | Holds are run- and tool-call-scoped; this is a workspace-load state. It needs a new command route, `PROTOCOL_VERSION` 28 → 29, and the server writing `trust.ron` for an unauthenticated remote principal. Defer to ADR-0015 |
| Exit with the error and print the TUI hint | Leaves the one exit path a new user hits first; contradicts OB1/OB2's "the TUI asks" |
| Trust automatically for the session on bare `qq` | Removes the review step the trust boundary exists for |
| Reuse `ClientUpdate`/`ClientRequest` for the prompt | Adds a fake server round trip for data the root already has |

## Evidence / references

- `crates/qq-config/src/loader.rs` (`pending_trust`, `grant_pending_trust`,
  `scan_pending_trust`, `TrustState::admit_process_trust`),
  `crates/qq-config/src/lib.rs` (`LoadRequest::with_process_trust`,
  `ProcessTrust`, `TrustDeclaration`, `PendingTrust::declarations`),
  `crates/qq-config/src/document.rs` (`sensitive_declarations`)
- `src/runtime.rs` (`trust_for_process`, `resolve_trust`, `tui_model_state`,
  `request_for_workspace`), `src/plan.rs` (`ProcessTrustFingerprint`),
  `src/main.rs` (`interactive`: `TrustRequired` match, `TrustResolver`)
- `crates/qq-tui/src/app.rs` (`Mode::Trust`, `handle_trust_key`,
  `apply_trust_resolved`, `note_trust_failure`),
  `crates/qq-tui/src/terminal.rs` (`Effect::ResolveTrust`),
  `crates/qq-tui/src/view/overlay.rs` (`trust_block`);
  `crates/qq-client/src/port.rs` (`ClientRequest::Models`, client-internal)
- Tests: `pending_trust_scans_without_writing_and_declarations_name_what_is_admitted`,
  `process_trust_admits_pending_files_without_writing_and_repends_on_edit`
  (qq-config); `plan_keys_differ_when_process_trust_differs`,
  `trust_prompt_persist_records_like_qq_trust_and_session_grants_only_this_process`,
  `a_session_trust_grant_is_a_new_plan_cache_slot` (qq bin);
  `trust_prompt_owns_input_and_maps_t_s_q_to_effects`,
  `resolved_trust_drops_the_prompt_applies_the_catalog_and_refreshes_the_server_view`,
  `an_untrusted_project_renders_the_trust_block_and_nothing_else_to_do`,
  `without_a_trust_resolver_the_prompt_stays_and_names_the_server_host`,
  `a_trust_resolver_is_awaited_off_the_loop_and_its_result_applied` (qq-tui)
