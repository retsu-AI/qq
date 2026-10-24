"""Convert a ``qq run`` JSONL trace into an ATIF trajectory.

The input is the durable trial stream emitted by ``qq run --format jsonl``
(or ``--trace PATH``): one ``trial`` record, ordered ``event`` records (full
``SessionEventEnvelope`` payloads with monotonic cursors), and exactly one
terminal ``outcome`` record.

The output targets ATIF-v1.7 as shipped by Harbor (the pydantic models in
``harbor.models.trajectories``). This module deliberately uses only the
standard library so the conversion is testable without Harbor installed; the
agent validates the result with Harbor's ``TrajectoryValidator`` when the
package is available.

Mapping rules (unknown never becomes fabricated):

- The queued user prompt becomes the leading ``source: "user"`` step.
- Each model turn (keyed by the run and ``turn_ordinal``) becomes one
  ``source: "agent"`` step with ``llm_call_count: 1``: assistant text from the
  message deltas, provider-exposed reasoning, the turn's tool calls, and an
  observation carrying each call's result, state, and error flag.
- ``session_compacted`` becomes a ``source: "system"`` step at its position.
- A non-completed run outcome appends a ``source: "system"`` step describing
  the failure or cancellation.
- Child sessions (``session_created`` with a ``parent_id``) become embedded
  ``subagent_trajectories``; each is referenced from the ``spawn_agent`` tool
  call whose lifetime covers the child's creation.
- Durable ``model_turn_completed`` records populate per-step model and usage
  metrics. Inclusive run totals from the ``outcome`` record populate
  ``final_metrics``; missing usage or pricing stays absent.
"""

from __future__ import annotations

import argparse
import json
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterable

ATIF_SCHEMA_VERSION = "ATIF-v1.7"

AGENT_NAME = "qq"

_SPAWN_AGENT_TOOL = "spawn_agent"

_NANOS_PER_USD = 1_000_000_000


class TraceError(ValueError):
    """The trace is missing records required to build a trajectory."""


def load_trace(path: str | Path) -> list[dict[str, Any]]:
    """Read one JSONL trace file into a list of records.

    Blank lines are skipped. A malformed line raises ``TraceError`` with its
    line number: a truncated trace should fail loudly, not silently drop
    history.
    """
    records: list[dict[str, Any]] = []
    with open(path, "r", encoding="utf-8") as handle:
        for number, line in enumerate(handle, start=1):
            line = line.strip()
            if not line:
                continue
            try:
                record = json.loads(line)
            except json.JSONDecodeError as error:
                raise TraceError(f"line {number} is not valid JSON: {error}") from error
            if not isinstance(record, dict):
                raise TraceError(f"line {number} is not a JSON object")
            records.append(record)
    return records


def convert_trace(records: Iterable[dict[str, Any]]) -> dict[str, Any]:
    """Convert trial records into an ATIF trajectory dictionary.

    Raises ``TraceError`` when the trace lacks a trial record or contains no
    steps for the root session (nothing durable happened before the harness
    failed).
    """
    trial: dict[str, Any] | None = None
    outcome: dict[str, Any] | None = None
    envelopes: list[dict[str, Any]] = []

    for record in records:
        kind = record.get("type")
        if kind == "trial":
            if trial is None:
                trial = record
        elif kind == "event":
            envelope = record.get("envelope")
            if isinstance(envelope, dict):
                envelopes.append(envelope)
        elif kind == "outcome":
            outcome = record

    if trial is None:
        raise TraceError("the trace has no trial record")

    root_session = trial.get("session_id")
    if not root_session:
        raise TraceError("the trial record has no session_id")

    builders = _build_sessions(envelopes)
    root = builders.get(root_session)
    if root is None:
        raise TraceError(f"the trace has no events for session {root_session}")

    return _session_trajectory(
        root,
        builders,
        trial=trial,
        outcome=outcome,
        embedded=False,
    )


# --------------------------------------------------------------------------
# Event accumulation
# --------------------------------------------------------------------------


class _Call:
    """One tool call, keyed by its durable ToolCallId."""

    def __init__(self, snapshot: dict[str, Any], sequence: int) -> None:
        self.id: str = snapshot.get("id", "")
        self.snapshot = snapshot
        self.requested_seq = sequence
        self.started_seq: int | None = None
        self.finished_seq: int | None = None
        self.approval: str | None = None

    def update(self, snapshot: dict[str, Any]) -> None:
        self.snapshot = snapshot

    @property
    def name(self) -> str:
        return self.snapshot.get("name", "")

    @property
    def state(self) -> str:
        return self.snapshot.get("state", "requested")

    def spawn_interval(self) -> tuple[int, int]:
        """The cursor interval during which this call was live."""
        start = self.started_seq if self.started_seq is not None else self.requested_seq
        end = self.finished_seq if self.finished_seq is not None else sys.maxsize
        return start, end


class _Turn:
    """One model turn: assistant text, reasoning, and its tool calls."""

    def __init__(self, run_id: str | None, ordinal: int, occurred_at_ms: int) -> None:
        self.run_id = run_id
        self.ordinal = ordinal
        self.occurred_at_ms = occurred_at_ms
        self.text_parts: list[str] = []
        self.refusal_parts: list[str] = []
        self.reasoning_parts: list[str] = []
        self.calls: list[_Call] = []
        self.model_name: str | None = None
        self.usage: dict[str, Any] | None = None
        self.cost_usd_nanos: int | None = None


class _System:
    """A system-originated step (compaction, terminal failure)."""

    def __init__(self, occurred_at_ms: int, message: str, extra: dict[str, Any]) -> None:
        self.occurred_at_ms = occurred_at_ms
        self.message = message
        self.extra = extra


class _User:
    def __init__(self, occurred_at_ms: int, message: str) -> None:
        self.occurred_at_ms = occurred_at_ms
        self.message = message


class _RecoveredAnswer:
    """Canonical final text recovered after delivery expiry, not a new LLM call."""

    def __init__(self, occurred_at_ms: int, message: str) -> None:
        self.occurred_at_ms = occurred_at_ms
        self.message = message


class _Session:
    def __init__(self, session_id: str) -> None:
        self.session_id = session_id
        self.parent_id: str | None = None
        self.created_seq: int | None = None
        self.summary: dict[str, Any] = {}
        # Ordered step sources: _User | _Turn | _System.
        self.items: list[Any] = []
        self.turns: dict[tuple[str | None, int], _Turn] = {}
        self.open_turn: _Turn | None = None
        self.pending_reasoning: list[str] = []
        self.message_turns: dict[str, _Turn] = {}
        self.calls: dict[str, _Call] = {}
        self.run_finished: dict[str, Any] | None = None
        self.run_finished_at_ms: int | None = None

    def turn(self, run_id: str | None, ordinal: int, occurred_at_ms: int) -> _Turn:
        key = (run_id, ordinal)
        turn = self.turns.get(key)
        if turn is None:
            turn = _Turn(run_id, ordinal, occurred_at_ms)
            self.turns[key] = turn
            self.items.append(turn)
            if self.pending_reasoning:
                turn.reasoning_parts.extend(self.pending_reasoning)
                self.pending_reasoning.clear()
        self.open_turn = turn
        return turn


def _build_sessions(envelopes: list[dict[str, Any]]) -> dict[str, _Session]:
    sessions: dict[str, _Session] = {}

    def session(envelope: dict[str, Any]) -> _Session:
        session_id = envelope.get("session_id", "")
        builder = sessions.get(session_id)
        if builder is None:
            builder = _Session(session_id)
            sessions[session_id] = builder
        return builder

    for envelope in envelopes:
        event = envelope.get("event")
        if not isinstance(event, dict):
            continue
        kind = event.get("type")
        builder = session(envelope)
        occurred = int(envelope.get("occurred_at_ms", 0))
        sequence = int(envelope.get("cursor", {}).get("sequence", 0))
        run_id = envelope.get("run_id")

        if kind == "session_created":
            summary = event.get("session", {})
            builder.summary = summary
            builder.parent_id = summary.get("parent_id")
            builder.created_seq = sequence

        elif kind == "prompt_queued":
            message = event.get("message", {})
            if message.get("role") == "user":
                builder.items.append(_User(occurred, message.get("output", "")))

        elif kind == "run_activity_changed":
            # A fresh provider request is being assembled: the previous turn
            # is over, so later reasoning belongs to the next turn.
            if event.get("activity") == "waiting_for_provider":
                builder.open_turn = None

        elif kind == "assistant_message_started":
            message = event.get("message", {})
            if message.get("role") != "assistant":
                continue
            turn = builder.turn(run_id, int(message.get("turn_ordinal", 0)), occurred)
            builder.message_turns[message.get("id", "")] = turn
            initial = message.get("output", "")
            if initial:
                turn.text_parts.append(initial)

        elif kind == "text_appended":
            turn = builder.message_turns.get(event.get("message_id", ""))
            if turn is None:
                continue
            text = event.get("text", "")
            if event.get("channel") == "refusal":
                turn.refusal_parts.append(text)
            else:
                turn.text_parts.append(text)

        elif kind == "reasoning_delta":
            text = event.get("text", "")
            if not text:
                continue
            if builder.open_turn is not None:
                builder.open_turn.reasoning_parts.append(text)
            else:
                builder.pending_reasoning.append(text)

        elif kind == "model_turn_completed":
            turn = builder.turn(run_id, int(event.get("turn_ordinal", 0)), occurred)
            model = event.get("model")
            if isinstance(model, dict) and model.get("model"):
                turn.model_name = str(model["model"])
            usage = event.get("usage")
            if isinstance(usage, dict):
                turn.usage = usage
            cost = event.get("estimated_cost_usd_nanos")
            if isinstance(cost, int) and not isinstance(cost, bool) and cost >= 0:
                turn.cost_usd_nanos = cost

        elif kind in (
            "tool_call_requested",
            "tool_approval_requested",
            "tool_call_started",
            "tool_call_finished",
        ):
            snapshot = event.get("tool_call", {})
            call_id = snapshot.get("id", "")
            call = builder.calls.get(call_id)
            if call is None:
                call = _Call(snapshot, sequence)
                builder.calls[call_id] = call
                turn = builder.turn(
                    run_id, int(snapshot.get("turn_ordinal", 0)), occurred
                )
                turn.calls.append(call)
            else:
                call.update(snapshot)
            if kind == "tool_call_started":
                call.started_seq = sequence
            elif kind == "tool_call_finished":
                call.finished_seq = sequence

        elif kind == "tool_approval_resolved":
            snapshot = event.get("tool_call", {})
            call = builder.calls.get(snapshot.get("id", ""))
            if call is not None:
                call.approval = event.get("resolution")

        elif kind == "session_compacted":
            before = event.get("before_bytes")
            after = event.get("after_bytes")
            summary = event.get("summary")
            message = f"Session context compacted: {before} bytes -> {after} bytes."
            if summary:
                message += f"\nSummary excerpt:\n{summary}"
            extra: dict[str, Any] = {
                "qq_event": "session_compacted",
                "before_bytes": before,
                "after_bytes": after,
            }
            builder.items.append(_System(occurred, message, extra))
            # Compaction happens between provider turns.
            builder.open_turn = None

        elif kind == "run_finished":
            # A session may own several runs (a compaction run precedes a
            # prompt run); keep the last terminal outcome, which is the run
            # the trial ends on.
            builder.run_finished = event
            builder.run_finished_at_ms = occurred
            builder.open_turn = None

    return sessions


# --------------------------------------------------------------------------
# Trajectory assembly
# --------------------------------------------------------------------------


def _iso(occurred_at_ms: int | None) -> str | None:
    if not occurred_at_ms:
        return None
    return datetime.fromtimestamp(occurred_at_ms / 1000, tz=timezone.utc).isoformat()


def _arguments(raw: str) -> dict[str, Any]:
    """Parse tool-call arguments, preserving unparsable payloads verbatim."""
    if not raw:
        return {}
    try:
        parsed = json.loads(raw)
    except json.JSONDecodeError:
        return {"_raw": raw}
    if isinstance(parsed, dict):
        return parsed
    return {"_raw": raw}


def _prune(mapping: dict[str, Any]) -> dict[str, Any]:
    return {key: value for key, value in mapping.items() if value is not None}


def _usage_metrics(
    usage: dict[str, Any] | None, cost_usd_nanos: int | None, total_steps: int
) -> dict[str, Any] | None:
    """Build ATIF final_metrics from a TokenUsage payload; absent stays absent."""
    metrics: dict[str, Any] = {"total_steps": total_steps}
    if usage is not None:
        fresh = int(usage.get("input_tokens", 0))
        cache_read = int(usage.get("cache_read_input_tokens", 0))
        cache_write = int(usage.get("cache_write_input_tokens", 0))
        metrics["total_prompt_tokens"] = fresh + cache_read + cache_write
        metrics["total_completion_tokens"] = int(usage.get("output_tokens", 0))
        metrics["total_cached_tokens"] = cache_read
    if cost_usd_nanos is not None:
        metrics["total_cost_usd"] = cost_usd_nanos / _NANOS_PER_USD
    return metrics


def _turn_metrics(
    usage: dict[str, Any] | None, cost_usd_nanos: int | None
) -> dict[str, Any] | None:
    """Build one ATIF Metrics payload without inventing unknown values."""
    metrics: dict[str, Any] = {}
    if usage is not None:
        fresh = int(usage.get("input_tokens", 0))
        cache_read = int(usage.get("cache_read_input_tokens", 0))
        cache_write = int(usage.get("cache_write_input_tokens", 0))
        metrics["prompt_tokens"] = fresh + cache_read + cache_write
        metrics["completion_tokens"] = int(usage.get("output_tokens", 0))
        metrics["cached_tokens"] = cache_read
    if cost_usd_nanos is not None:
        metrics["cost_usd"] = cost_usd_nanos / _NANOS_PER_USD
    return metrics or None


def _child_index(builders: dict[str, _Session]) -> dict[str, list[_Session]]:
    children: dict[str, list[_Session]] = {}
    for builder in builders.values():
        if builder.parent_id:
            children.setdefault(builder.parent_id, []).append(builder)
    for siblings in children.values():
        siblings.sort(key=lambda child: child.created_seq or 0)
    return children


def _match_children(
    session: _Session, children: list[_Session]
) -> tuple[dict[str, list[_Session]], list[_Session]]:
    """Assign child sessions to the spawn_agent call live at their creation.

    Returns (call id -> children, unmatched children). A child whose creation
    falls inside no spawn window (or with an unknown creation cursor) stays
    unmatched and is still embedded so no trajectory is dropped.
    """
    spawn_calls = [
        call for call in session.calls.values() if call.name == _SPAWN_AGENT_TOOL
    ]
    matched: dict[str, list[_Session]] = {}
    unmatched: list[_Session] = []
    for child in children:
        created = child.created_seq
        owner = None
        if created is not None:
            for call in spawn_calls:
                start, end = call.spawn_interval()
                if start <= created <= end:
                    owner = call
                    break
        if owner is None:
            unmatched.append(child)
        else:
            matched.setdefault(owner.id, []).append(child)
    return matched, unmatched


def _call_observation(
    call: _Call, spawned: list[_Session] | None
) -> dict[str, Any]:
    snapshot = call.snapshot
    result: dict[str, Any] = {"source_call_id": call.id}
    content = snapshot.get("result")
    if content is not None:
        result["content"] = content
    extra: dict[str, Any] = {"state": call.state}
    if snapshot.get("is_error"):
        extra["is_error"] = True
    if call.approval is not None:
        extra["approval"] = call.approval
    result["extra"] = extra
    if spawned:
        result["subagent_trajectory_ref"] = [
            {
                "trajectory_id": child.session_id,
                "session_id": child.session_id,
            }
            for child in spawned
        ]
    return result


def _turn_step(
    step_id: int,
    turn: _Turn,
    spawn_map: dict[str, list[_Session]],
    model_name: str | None,
) -> dict[str, Any]:
    step: dict[str, Any] = {
        "step_id": step_id,
        "source": "agent",
        "message": "".join(turn.text_parts),
        "llm_call_count": 1,
    }
    timestamp = _iso(turn.occurred_at_ms)
    if timestamp is not None:
        step["timestamp"] = timestamp
    selected_model = turn.model_name or model_name
    if selected_model is not None:
        step["model_name"] = selected_model
    metrics = _turn_metrics(turn.usage, turn.cost_usd_nanos)
    if metrics is not None:
        step["metrics"] = metrics
    if turn.reasoning_parts:
        step["reasoning_content"] = "".join(turn.reasoning_parts)
    if turn.calls:
        calls = sorted(
            turn.calls,
            key=lambda call: int(call.snapshot.get("call_ordinal", 0)),
        )
        step["tool_calls"] = [
            _prune(
                {
                    "tool_call_id": call.id,
                    "function_name": call.name,
                    "arguments": _arguments(call.snapshot.get("arguments", "")),
                    "extra": _prune(
                        {"provider_call_id": call.snapshot.get("provider_call_id")}
                    )
                    or None,
                }
            )
            for call in calls
        ]
        step["observation"] = {
            "results": [
                _call_observation(call, spawn_map.get(call.id)) for call in calls
            ]
        }
    extra: dict[str, Any] = {"turn_ordinal": turn.ordinal}
    if turn.refusal_parts:
        extra["refusal"] = "".join(turn.refusal_parts)
    step["extra"] = extra
    return step


def _outcome_system_item(session: _Session) -> _System | None:
    """A terminal system step for a run that did not complete."""
    finished = session.run_finished
    if finished is None:
        return None
    outcome = finished.get("outcome", {})
    kind = outcome.get("type")
    if kind in (None, "completed"):
        return None
    occurred = session.run_finished_at_ms or 0
    extra: dict[str, Any] = {"qq_event": "run_finished", "outcome": outcome}
    if kind == "failed":
        failure = outcome.get("failure", {})
        message = (
            f"Run failed ({failure.get('kind', 'unknown')}): "
            f"{failure.get('message', '')}"
        )
    elif kind == "cancelled":
        message = "Run cancelled before completion."
    else:
        message = f"Run ended without completing: {kind}."
    return _System(occurred, message, extra)


def _session_trajectory(
    session: _Session,
    builders: dict[str, _Session],
    *,
    trial: dict[str, Any] | None,
    outcome: dict[str, Any] | None,
    embedded: bool,
) -> dict[str, Any]:
    children_by_parent = _child_index(builders)
    children = children_by_parent.get(session.session_id, [])
    spawn_map, unmatched = _match_children(session, children)

    steps: list[dict[str, Any]] = []
    model_name = _model_name(session, trial)
    items = list(session.items)
    if not embedded and outcome is not None and "recovered_answer" in outcome:
        answer = outcome["recovered_answer"]
        if not isinstance(answer, str):
            raise TraceError("recovered_answer must be a string")
        items.append(_RecoveredAnswer(session.run_finished_at_ms or 0, answer))
    terminal = _outcome_system_item(session)
    if terminal is not None:
        items.append(terminal)

    for item in items:
        step_id = len(steps) + 1
        if isinstance(item, _User):
            step: dict[str, Any] = {
                "step_id": step_id,
                "source": "user",
                "message": item.message,
            }
            timestamp = _iso(item.occurred_at_ms)
            if timestamp is not None:
                step["timestamp"] = timestamp
            steps.append(step)
        elif isinstance(item, _Turn):
            steps.append(_turn_step(step_id, item, spawn_map, model_name))
        elif isinstance(item, _RecoveredAnswer):
            step = {"step_id": step_id, "source": "agent", "message": item.message,
                    "extra": {"qq_recovered_answer": True}}
            timestamp = _iso(item.occurred_at_ms)
            if timestamp is not None:
                step["timestamp"] = timestamp
            steps.append(step)
        elif isinstance(item, _System):
            step = {
                "step_id": step_id,
                "source": "system",
                "message": item.message,
                "extra": item.extra,
            }
            timestamp = _iso(item.occurred_at_ms)
            if timestamp is not None:
                step["timestamp"] = timestamp
            steps.append(step)

    if not steps:
        raise TraceError(
            f"session {session.session_id} produced no trajectory steps"
        )

    # Unmatched children still need a resolvable reference from the parent;
    # attach one to the final step's observation without a source call.
    if unmatched:
        refs = [
            {
                "trajectory_id": child.session_id,
                "session_id": child.session_id,
            }
            for child in unmatched
        ]
        last = steps[-1]
        observation = last.setdefault("observation", {"results": []})
        observation["results"].append({"subagent_trajectory_ref": refs})

    trajectory: dict[str, Any] = {
        "schema_version": ATIF_SCHEMA_VERSION,
        "session_id": session.session_id,
        "agent": _agent(session, trial, embedded),
        "steps": steps,
    }
    if embedded:
        trajectory["trajectory_id"] = session.session_id

    final_metrics = _final_metrics(session, outcome if not embedded else None, len(steps))
    if final_metrics is not None:
        trajectory["final_metrics"] = final_metrics

    extra = _trajectory_extra(session, trial if not embedded else None,
                              outcome if not embedded else None)
    if extra:
        trajectory["extra"] = extra

    embedded_children = [
        _session_trajectory(
            child, builders, trial=trial, outcome=None, embedded=True
        )
        for child in children
    ]
    if embedded_children:
        trajectory["subagent_trajectories"] = embedded_children

    return trajectory


def _model_name(session: _Session, trial: dict[str, Any] | None) -> str | None:
    model = session.summary.get("model")
    if model:
        return str(model)
    if trial is not None:
        selection = trial.get("model")
        if isinstance(selection, dict) and selection.get("model"):
            return str(selection["model"])
    return None


def _agent(
    session: _Session, trial: dict[str, Any] | None, embedded: bool
) -> dict[str, Any]:
    version = "unknown"
    if trial is not None and trial.get("qq_version"):
        version = str(trial["qq_version"])
    agent: dict[str, Any] = {"name": AGENT_NAME, "version": version}
    model_name = _model_name(session, trial)
    if model_name is not None:
        agent["model_name"] = model_name
    if embedded:
        agent["extra"] = {"role": "subagent"}
    return agent


def _final_metrics(
    session: _Session, outcome: dict[str, Any] | None, total_steps: int
) -> dict[str, Any] | None:
    if outcome is not None:
        return _usage_metrics(
            outcome.get("usage"),
            outcome.get("estimated_cost_usd_nanos"),
            total_steps,
        )
    finished = session.run_finished
    if finished is None:
        return {"total_steps": total_steps}
    summary = finished.get("session", {})
    cost = None
    accounting = summary.get("accounting")
    if isinstance(accounting, dict):
        cost = accounting.get("direct", {}).get("estimated_cost_usd_nanos")
    if cost is None:
        cost = summary.get("estimated_cost_usd_nanos")
    return _usage_metrics(finished.get("usage"), cost, total_steps)


def _trajectory_extra(
    session: _Session,
    trial: dict[str, Any] | None,
    outcome: dict[str, Any] | None,
) -> dict[str, Any] | None:
    qq: dict[str, Any] = {}
    if trial is not None:
        qq.update(
            _prune(
                {
                    "qq_version": trial.get("qq_version"),
                    "qq_source_revision": trial.get("qq_source_revision"),
                    "protocol_version": trial.get("protocol_version"),
                    "workspace_identity": trial.get("workspace_identity"),
                    "model": trial.get("model"),
                    "context_window": trial.get("context_window"),
                    "pricing_provenance": trial.get("pricing_provenance"),
                    "approval": trial.get("approval"),
                    "timeout_seconds": trial.get("timeout_seconds"),
                    "max_turns": trial.get("max_turns"),
                    "max_cost_usd_nanos": trial.get("max_cost_usd_nanos"),
                    "arm": trial.get("arm"),
                    "workspace_id": trial.get("workspace_id"),
                    "session_id": trial.get("session_id"),
                    "run_id": trial.get("run_id"),
                }
            )
        )
    else:
        qq["session_id"] = session.session_id
        if session.parent_id:
            qq["parent_session_id"] = session.parent_id
        finished = session.run_finished
        if finished is not None and finished.get("run_id"):
            qq["run_id"] = finished["run_id"]
    if outcome is not None:
        qq["outcome"] = _prune(
            {
                "status": outcome.get("status"),
                "exit_code": outcome.get("exit_code"),
                "message": outcome.get("message"),
                "usage": outcome.get("usage"),
                "estimated_cost_usd_nanos": outcome.get("estimated_cost_usd_nanos"),
                "prompt_identity": outcome.get("prompt_identity"),
            }
        )
    elif session.run_finished is not None:
        qq["outcome"] = {"run": session.run_finished.get("outcome")}
    if not qq:
        return None
    return {"qq": qq}


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Convert a qq run JSONL trace to an ATIF trajectory."
    )
    parser.add_argument("trace", type=Path, help="Path to the qq JSONL trace file")
    parser.add_argument(
        "-o",
        "--output",
        type=Path,
        default=None,
        help="Trajectory output path (default: stdout)",
    )
    args = parser.parse_args(argv)

    try:
        trajectory = convert_trace(load_trace(args.trace))
    except (OSError, TraceError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1

    rendered = json.dumps(trajectory, indent=2)
    if args.output is None:
        print(rendered)
    else:
        args.output.write_text(rendered + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main())
