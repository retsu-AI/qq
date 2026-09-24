# ADR-0042 — Two session switchers: the model, and how much Jev does

**Status:** Proposed — the shape is implemented; the ladder rows in § 2 await
the founder's confirmation and change without a protocol bump
**Date:** 2026-09-24
**Deciders:** integration plan § 2 item 9 (two model switchers); founder for
the ladder rows
**Extends:** [ADR-0030](0030-optional-jev-decisions.md) (independent Jev
capabilities), [ADR-0031](0031-explicit-reasoning-effort.md) (explicit effort),
[ADR-0034](0034-concrete-jev-routing.md) (inherited activation),
[ADR-0041](0041-jev-delegated-approval.md) (delegate consent)
**Implements:** [`protocol.md`](../design/protocol.md) § Sessions,
[`architecture.md` § Extension Contract](../design/architecture.md#extension-contract)

## Context

The integration plan asks for two runtime switchers that every surface
(TUI, `qq serve` HTTP/SSE, and the web, desktop, and mobile clients behind it)
observes through the shared `qq-client` reducer and that the SQLite event
store persists per session:

1. the regular provider/model switcher, with a provider-neutral effort
   field that each adapter maps onto its own reasoning knob; and
2. a Jev switcher with five modes: `low | medium | high | max | ultrajev`.

Two facts decide the shape of both.

**The first switcher already exists.** Protocol 25–27 shipped
`set_session_model` / `session_model_set` (`/v1/sessions/model`),
`set_session_effort` / `session_effort_set` (`/v1/sessions/effort`), and the
`SessionSummary.model` / `reasoning_effort` fields every client reduces. The
provider-neutral field is `qq_reasoning::ReasoningEffort`
(`none|minimal|low|medium|high|xhigh`), re-exported by `qq-protocol`; ADR-0031
made it part of plan identity, and each adapter maps it onto its own request
knob (OpenAI `reasoning_effort`, Anthropic thinking and `output_config.effort`
in #154/#157, Bedrock and Mantle through the same descriptor). The TUI has
`/models` and `/effort`; `ModelDescriptor.reasoning_efforts` shapes the picker.
Adding a second `set_model` command would be a duplicate vocabulary for the
same durable state.

**Jev has no effort knob.** TypeSafe serves one model (`jev-1.13.0`, aliases
`jev-latest` / `jev-preview`) on `POST /v1/systemone`. The request has a
`model` field, a `state`, and typed questions; there is no reasoning-effort,
thinking-budget, or mode parameter, and Jev returns calibrated decisions, not
generated text. So the five Jev modes cannot be a provider-side effort ladder
and cannot map onto `ReasoningEffort`. What QQ *can* vary per session is how
much of the run Jev is asked to decide. QQ already has three independent,
default-off Jev capabilities (ADR-0030, ADR-0034, ADR-0041): task routing
(`jev_routing`), checkpoint review (`jev_review: off|final|enforce`), and the
approval delegate (`approval_delegate: by_mode|on|off`, with `jev_approval`
choosing Jev as the delegate). Today each is a configuration value or a
profile value, with `approval_delegate` alone switchable per session
(`set_approval_delegate`, protocol 28).

## Decision

1. **No second model switcher.** `set_session_model` and `set_session_effort`
   are the regular switcher. This ADR adds nothing to them; the reasoning
   effort field remains `ReasoningEffort`, and provider mapping stays in
   `qq-provider` under ADR-0031. The name the plan uses, `SetModel`, is
   `set_session_model`.

2. **`JevMode` is a session-level ladder over the existing Jev roles.**
   `qq_protocol::JevMode` has exactly the five values
   `low | medium | high | max | ultrajev`. It is provider-neutral wire data:
   it names how much of a session Jev decides, never a TypeSafe request
   parameter, a model alias, or a reasoning effort. Each mode resolves, at
   plan compile time, to values of the three existing capabilities:

   | mode | task routing | checkpoint review | approval delegate |
   | --- | --- | --- | --- |
   | `low` | on | `off` | `off` |
   | `medium` | on | `final` | `off` |
   | `high` | on | `enforce` | `off` |
   | `max` | on | `enforce` | `by_mode` |
   | `ultrajev` | on | `enforce` | `on` |

   The ladder is monotone: each step asks Jev to decide strictly more. The
   table lives in one function of the composition root (`src/runtime.rs`),
   where the other `RuntimeOverrides` are produced; `qq-core` carries the
   mode as an opaque value and never learns what it means. Changing a row is a
   root decision, not a protocol change.

3. **A session override, like `approval_delegate`.** New command
   `set_jev_mode { session_id, mode? }` on `/v1/sessions/jev-mode` with
   outcome `jev_mode_set { session_id, mode? }`; optional
   `SessionSummary.jev_mode` carries it. `None` clears the override so the
   configured and profile values apply again. The store gains a nullable
   `sessions.jev_mode` column (schema 37). The command writes the row and the
   `session_updated` event in one transaction and publishes only after commit
   (ADR-0003), so every surface reduces the same value from the same event
   and replay reconstructs it. `/jev` is reserved as the client slash command.

4. **Takes effect at the next run claim.** The mode is read with the rest of
   the session row when a run is claimed and travels to the loader in
   `RuntimeLoadRequest.jev_mode`. A run already executing keeps the plan it
   compiled; this matches `set_session_model` and `set_session_effort`, not
   `set_approval_delegate`, because review and routing are plan identity
   (ADR-0030, ADR-0032) and cannot change under a running plan.

5. **Precedence is explicit and never widens authority.**
   - The mode overrides configured and profile `jev_review` and `jev_routing`
     the same way `QQ_JEV_REVIEW` / `QQ_JEV_ROUTING` do: it is an explicit
     runtime choice and part of the plan cache key.
   - An owned child inherits its parent's *resolved* review and routing
     identities (ADR-0034); those win over the child's inherited mode, so a
     child cannot newly enable Jev because its parent's row says `max`.
   - The mode's delegate column sets the plan's default `approval_delegate`.
     The session's own `set_approval_delegate` override still wins at each
     held call (protocol 28), and *who* the delegate is remains the
     configured `jev_approval` consent (ADR-0041 § 1): `max` and `ultrajev`
     do not make Jev an approver in a workspace that did not opt in; they
     route held calls to whichever delegate the workspace composed.
   - The approval mode stays the ceiling. No `JevMode` lifts a floor, widens a
     mode, or answers a question.

6. **Credentials are resolved where they always were.** Setting a mode does
   not touch the credential store. A mode that enables review or routing in
   a workspace without a TypeSafe key fails the *next run* with the same
   configuration error a configured `jev_review: final` without a key does.
   The credential-free `--tui-qa-root` fixture rejects an enabled mode as it
   rejects every other Jev activation.

7. **No new adapter.** The TypeSafe transport already exists in the root
   package (`TypeSafeTaskRouter`, `TypeSafeCheckpointReviewer`,
   `JevApprovalReviewer`) behind `qq-core`'s `TaskRouter`,
   `CheckpointReviewer`, and `ApprovalReviewer` traits. Nothing in this
   decision needs a `qq-provider` adapter for Jev, and building one for a
   model that generates no text would be a placeholder (AGENTS.md).
   Consolidating the three TypeSafe clients is a separate refactor slice.

## Consequences

- Protocol 28 → 29 (new command, outcome, summary field, reserved slash
  command); store schema 36 → 37. Older clients reject the new command and
  summary field; version negotiation refuses the pair.
- An operator turns Jev up or down for one session from any surface without
  a config write or restart: `/jev high` in the TUI, `POST
  /v1/sessions/jev-mode` from a web or mobile client, and every attached
  client sees `session_updated.jev_mode` change.
- The ladder's rows are the founder's product decision. They are recorded
  here so a future agent does not relitigate them, and they are one table to
  change if the decision changes.
- Nothing about Jev's request bounds, thresholds, identities, or fail-closed
  behavior changes (ADR-0028, ADR-0034, ADR-0041).

## Alternatives

- **Mapping the five modes onto `ReasoningEffort`** (`max → xhigh`,
  `ultrajev → ?`) was rejected: the values do not align, TypeSafe accepts no
  such parameter, and it would silently make the regular effort switch and the
  Jev switch the same knob, contradicting "two switchers".
- **A `JevMode` that pins a `model` alias** (`jev-latest` / `jev-preview`) was
  rejected: the aliases both resolve to `jev-1.13.0` today, QQ pins the
  versioned id deliberately so thresholds stay calibrated (ADR-0034,
  ADR-0041), and an alias is not a ladder.
- **Three separate session commands** (`set_jev_review`, `set_jev_routing`,
  plus the existing delegate) were rejected for this slice: the plan asked
  for one five-mode switch, and three independent booleans expose eighteen
  combinations to a picker. The three configuration values remain for
  operators who want a combination the ladder does not name.
- **Mapping in `qq-core`** was rejected: core would learn Jev's capabilities
  and their meaning; the composition root already owns every other Jev
  decision (ADR-0041 § 5).
- **Letting the mode also set `jev_approval`** was rejected: that is the
  consent that lets Jev authorize side effects (ADR-0041), and a per-session
  switch must not be the place it is first granted.
