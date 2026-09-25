"""Regenerate the qq JSONL trace fixtures.

Each fixture mirrors the exact wire shapes emitted by ``qq run --format
jsonl`` (see ``src/headless.rs`` and ``crates/qq-protocol/src/sessions.rs``):
one ``trial`` record, ordered ``event`` records whose envelopes decode as
``SessionEventEnvelope`` (the Rust test ``tests/harbor_atif_fixtures.rs``
enforces this), and one terminal ``outcome`` record.

The expected ``*.trajectory.json`` files are frozen converter outputs that
were reviewed by hand; this script does not regenerate them.

Usage: python tests/make_fixtures.py  (from benchmarks/harbor/)
"""

from __future__ import annotations

import json
from pathlib import Path

FIXTURES = Path(__file__).resolve().parent / "fixtures"

STORE = "aa" * 16
WORKSPACE = "ab" * 16
SESSION = "ac" * 16
RUN = "ad" * 16
CHILD_SESSION = "ae" * 16
CHILD_RUN = "af" * 16

QQ_VERSION = "0.1.0"
# Keep generated trace identity aligned with qq_protocol::PROTOCOL_VERSION.
# Traces need not contain every optional event, but they must identify the
# current wire contract. test_atif.py checks this against the Rust source.
PROTOCOL_VERSION = 32
MODEL = {"model": "anthropic/claude-sonnet-4-5", "max_output_tokens": 32000}
PROMPT_IDENTITY = {
    "version": 7,
    "instruction_hash": "11" * 32,
    "system_prompt_hash": "22" * 32,
    "tool_schema_hash": "33" * 32,
}


def message_id(n: int) -> str:
    return f"{0xB0 + n:02x}" * 16


def call_id(n: int) -> str:
    return f"{0xC0 + n:02x}" * 16


class Trace:
    def __init__(self) -> None:
        self.lines: list[dict] = []
        self.sequence = 0
        self.now_ms = 1_754_000_000_000

    def trial(self, **overrides) -> None:
        record = {
            "type": "trial",
            "qq_version": QQ_VERSION,
            "qq_source_revision": "fixture-revision",
            "protocol_version": PROTOCOL_VERSION,
            "workspace_identity": "44" * 32,
            "model": MODEL,
            "context_window": 200000,
            "pricing_provenance": "fixture",
            "approval": "auto",
            "timeout_seconds": 900,
            "workspace_id": WORKSPACE,
            "session_id": SESSION,
            "run_id": RUN,
        }
        record.update(overrides)
        self.lines.append(record)

    def event(self, event: dict, *, session=SESSION, run=RUN) -> None:
        self.sequence += 1
        self.now_ms += 250
        envelope = {
            "cursor": {
                "store_id": STORE,
                "workspace_id": WORKSPACE,
                "sequence": self.sequence,
            },
            "session_id": session,
            "occurred_at_ms": self.now_ms,
            "event": event,
        }
        if run is not None:
            envelope["run_id"] = run
        self.lines.append({"type": "event", "envelope": envelope})

    def outcome(self, status: str, exit_code: int, **extra) -> None:
        record = {
            "type": "outcome",
            "status": status,
            "exit_code": exit_code,
            "prompt_identity": PROMPT_IDENTITY,
        }
        record.update(extra)
        self.lines.append(record)

    def write(self, name: str) -> None:
        path = FIXTURES / name
        path.parent.mkdir(parents=True, exist_ok=True)
        with open(path, "w", encoding="utf-8") as handle:
            for line in self.lines:
                handle.write(json.dumps(line, separators=(",", ":")) + "\n")
        print(f"wrote {path}")


def session_summary(
    *,
    session=SESSION,
    parent=None,
    status="running",
    active_run=RUN,
    model="anthropic/claude-sonnet-4-5",
    updated_at_ms,
    last_outcome=None,
    accounting=None,
    cost=None,
):
    summary = {
        "id": session,
        "workspace_id": WORKSPACE,
        "title": "Task",
        "status": status,
        "queued_prompts": 0,
        "model": model,
        "approval_mode": "auto",
        "updated_at_ms": updated_at_ms,
    }
    if parent is not None:
        summary["parent_id"] = parent
    if active_run is not None:
        summary["active_run_id"] = active_run
    if last_outcome is not None:
        summary["last_outcome"] = last_outcome
    if accounting is not None:
        summary["accounting"] = accounting
    if cost is not None:
        summary["estimated_cost_usd_nanos"] = cost
    return summary


def user_message(mid: str, prompt: str, created_at_ms: int, *, session=SESSION, run=RUN):
    return {
        "id": mid,
        "session_id": session,
        "run_id": run,
        "turn_ordinal": 0,
        "role": "user",
        "state": "complete",
        "output": prompt,
        "refusal": "",
        "created_at_ms": created_at_ms,
    }


def assistant_message(
    mid: str, turn: int, created_at_ms: int, *, session=SESSION, run=RUN
):
    return {
        "id": mid,
        "session_id": session,
        "run_id": run,
        "turn_ordinal": turn,
        "role": "assistant",
        "state": "streaming",
        "output": "",
        "refusal": "",
        "created_at_ms": created_at_ms,
    }


def run_snapshot(*, session=SESSION, run=RUN, status="queued"):
    return {"id": run, "session_id": session, "status": status}


def tool_call(
    cid: str,
    turn: int,
    ordinal: int,
    name: str,
    arguments: str,
    state: str,
    *,
    session=SESSION,
    run=RUN,
    result=None,
    is_error=False,
):
    snapshot = {
        "id": cid,
        "session_id": session,
        "run_id": run,
        "turn_ordinal": turn,
        "call_ordinal": ordinal,
        "provider_call_id": f"call_{ordinal}",
        "name": name,
        "arguments": arguments,
        "state": state,
        "is_error": is_error,
    }
    if result is not None:
        snapshot["result"] = result
    return snapshot


def model_turn_completed(
    trace: Trace,
    turn: int,
    *,
    session=SESSION,
    run=RUN,
    usage=None,
    cost=None,
) -> None:
    event = {
        "type": "model_turn_completed",
        "run_id": run,
        "turn_ordinal": turn,
        "model": MODEL,
    }
    if usage is not None:
        event["usage"] = usage
    if cost is not None:
        event["estimated_cost_usd_nanos"] = cost
    trace.event(event, session=session, run=run)


def prompt_flow(trace: Trace, prompt: str, *, session=SESSION, run=RUN) -> None:
    trace.event(
        {
            "type": "prompt_queued",
            "session": session_summary(
                session=session,
                status="queued",
                active_run=None,
                updated_at_ms=trace.now_ms,
                parent=SESSION if session != SESSION else None,
            ),
            "message": user_message(message_id(0 if session == SESSION else 8),
                                    prompt, trace.now_ms,
                                    session=session, run=run),
            "run": run_snapshot(session=session, run=run),
            "queue_position": 0,
        },
        session=session,
        run=run,
    )
    trace.event(
        {
            "type": "run_started",
            "session": session_summary(session=session,
                                       active_run=run,
                                       updated_at_ms=trace.now_ms,
                                       parent=SESSION if session != SESSION else None),
            "run_id": run,
        },
        session=session,
        run=run,
    )
    trace.event(
        {"type": "run_activity_changed", "run_id": run,
         "activity": "waiting_for_provider"},
        session=session,
        run=run,
    )


def finish_run(
    trace: Trace,
    *,
    session=SESSION,
    run=RUN,
    outcome=None,
    usage=None,
    context_tokens=None,
    accounting=None,
    cost=None,
):
    event = {
        "type": "run_finished",
        "session": session_summary(
            session=session,
            status="idle",
            active_run=None,
            updated_at_ms=trace.now_ms,
            last_outcome=outcome or {"type": "completed"},
            parent=SESSION if session != SESSION else None,
            accounting=accounting,
            cost=cost,
        ),
        "run_id": run,
        "outcome": outcome or {"type": "completed"},
    }
    if usage is not None:
        event["usage"] = usage
    if context_tokens is not None:
        event["context_tokens"] = context_tokens
    trace.event(event, session=session, run=run)


def text_only() -> None:
    trace = Trace()
    trace.trial()
    prompt_flow(trace, "Say hello to the grader.")
    trace.event(
        {
            "type": "assistant_message_started",
            "message": assistant_message(message_id(1), 1, trace.now_ms),
        }
    )
    trace.event(
        {"type": "text_appended", "message_id": message_id(1),
         "channel": "output", "text": "Hello"}
    )
    trace.event(
        {"type": "text_appended", "message_id": message_id(1),
         "channel": "output", "text": " from qq."}
    )
    usage = {
        "input_tokens": 120,
        "cache_read_input_tokens": 0,
        "cache_write_input_tokens": 0,
        "output_tokens": 9,
    }
    model_turn_completed(trace, 1, usage=usage, cost=1_234_500)
    trace.event(
        {"type": "session_context_updated", "run_id": RUN, "context_tokens": 42}
    )
    finish_run(trace, usage=usage, context_tokens=42)
    trace.outcome(
        "completed", 0, usage=usage, estimated_cost_usd_nanos=1_234_500
    )
    trace.write("text_only.trace.jsonl")


def tool_loop() -> None:
    trace = Trace()
    trace.trial()
    prompt_flow(trace, "Fix the failing test in this repository.")
    # Turn 1: exposed reasoning, then a read and an approved shell call.
    trace.event(
        {"type": "reasoning_started", "run_id": RUN, "kind": "exposed_thinking"}
    )
    trace.event(
        {"type": "reasoning_delta", "run_id": RUN, "kind": "exposed_thinking",
         "text": "I should inspect the test first."}
    )
    trace.event(
        {"type": "reasoning_completed", "run_id": RUN, "kind": "exposed_thinking"}
    )
    model_turn_completed(
        trace,
        1,
        usage={
            "input_tokens": 800,
            "cache_read_input_tokens": 200,
            "cache_write_input_tokens": 100,
            "output_tokens": 80,
        },
        cost=15_000_000,
    )
    read_args = '{"path":"tests/test_math.py"}'
    trace.event(
        {"type": "tool_call_requested",
         "tool_call": tool_call(call_id(1), 1, 1, "read_file", read_args,
                                "requested")}
    )
    trace.event(
        {"type": "tool_call_started",
         "tool_call": tool_call(call_id(1), 1, 1, "read_file", read_args,
                                "running")}
    )
    trace.event(
        {"type": "tool_call_finished",
         "tool_call": tool_call(call_id(1), 1, 1, "read_file", read_args,
                                "completed",
                                result="def test_add():\n    assert add(2, 2) == 5\n")}
    )
    shell_args = '{"command":"sed -i \'s/== 5/== 4/\' tests/test_math.py"}'
    trace.event(
        {"type": "tool_call_requested",
         "tool_call": tool_call(call_id(2), 1, 2, "shell", shell_args,
                                "requested")}
    )
    trace.event(
        {"type": "tool_approval_requested",
         "tool_call": tool_call(call_id(2), 1, 2, "shell", shell_args,
                                "awaiting_approval"),
         "shell": {"command": "sed -i 's/== 5/== 4/' tests/test_math.py",
                   "cwd": "/app"}}
    )
    trace.event(
        {"type": "tool_approval_resolved",
         "tool_call": tool_call(call_id(2), 1, 2, "shell", shell_args,
                                "requested"),
         "resolution": "approved_once"}
    )
    trace.event(
        {"type": "tool_call_started",
         "tool_call": tool_call(call_id(2), 1, 2, "shell", shell_args,
                                "running")}
    )
    trace.event(
        {"type": "tool_call_output_delta", "tool_call_id": call_id(2),
         "chunk": ""}
    )
    trace.event(
        {"type": "tool_call_finished",
         "tool_call": tool_call(call_id(2), 1, 2, "shell", shell_args,
                                "completed", result="")}
    )
    # Turn 2: the model reports completion.
    trace.event(
        {"type": "run_activity_changed", "run_id": RUN,
         "activity": "waiting_for_provider"}
    )
    trace.event(
        {
            "type": "assistant_message_started",
            "message": assistant_message(message_id(2), 2, trace.now_ms),
        }
    )
    trace.event(
        {"type": "text_appended", "message_id": message_id(2),
         "channel": "output", "text": "Fixed the assertion; the test now passes."}
    )
    model_turn_completed(
        trace,
        2,
        usage={
            "input_tokens": 1600,
            "cache_read_input_tokens": 1000,
            "cache_write_input_tokens": 200,
            "output_tokens": 100,
        },
        cost=26_000_000,
    )
    trace.event(
        {"type": "run_context_updated", "run_id": RUN, "context_tokens": 1900}
    )
    usage = {
        "input_tokens": 2400,
        "cache_read_input_tokens": 1200,
        "cache_write_input_tokens": 300,
        "output_tokens": 180,
    }
    finish_run(trace, usage=usage, context_tokens=1900)
    trace.outcome(
        "completed", 0, usage=usage, estimated_cost_usd_nanos=41_000_000
    )
    trace.write("tool_loop.trace.jsonl")


def failure() -> None:
    trace = Trace()
    trace.trial()
    prompt_flow(trace, "Summarize the repository.")
    trace.event(
        {
            "type": "assistant_message_started",
            "message": assistant_message(message_id(3), 1, trace.now_ms),
        }
    )
    trace.event(
        {"type": "text_appended", "message_id": message_id(3),
         "channel": "output", "text": "This repository"}
    )
    model_turn_completed(trace, 1)
    failure_outcome = {
        "type": "failed",
        "failure": {
            "kind": "provider_api",
            "message": "the provider rejected the request: 400 invalid_request",
        },
    }
    finish_run(trace, outcome=failure_outcome)
    trace.outcome(
        "task_failed",
        1,
        message="the provider rejected the request: 400 invalid_request",
    )
    trace.write("failure.trace.jsonl")


def cancellation() -> None:
    trace = Trace()
    trace.trial(timeout_seconds=60)
    prompt_flow(trace, "Run the full benchmark suite.")
    model_turn_completed(trace, 1)
    shell_args = '{"command":"sleep 100000"}'
    trace.event(
        {"type": "tool_call_requested",
         "tool_call": tool_call(call_id(3), 1, 1, "shell", shell_args,
                                "requested")}
    )
    trace.event(
        {"type": "tool_call_started",
         "tool_call": tool_call(call_id(3), 1, 1, "shell", shell_args,
                                "running")}
    )
    trace.event(
        {
            "type": "cancellation_requested",
            "session": session_summary(updated_at_ms=trace.now_ms),
            "run_id": RUN,
        }
    )
    trace.event(
        {"type": "tool_call_finished",
         "tool_call": tool_call(call_id(3), 1, 1, "shell", shell_args,
                                "interrupted", is_error=True)}
    )
    finish_run(trace, outcome={"type": "cancelled"})
    trace.outcome(
        "timed_out", 3,
        message="the run was cancelled after its timeout elapsed",
    )
    trace.write("cancellation.trace.jsonl")


def compaction() -> None:
    # Mid-run auto-compaction does not happen in today's runtime (compaction
    # is idle-only); this fixture exercises the schema-valid event so the
    # converter is ready when Phase 2 makes it observable in a trace.
    trace = Trace()
    trace.trial()
    prompt_flow(trace, "Refactor the parser module.")
    trace.event(
        {
            "type": "assistant_message_started",
            "message": assistant_message(message_id(4), 1, trace.now_ms),
        }
    )
    trace.event(
        {"type": "text_appended", "message_id": message_id(4),
         "channel": "output", "text": "Surveying the module."}
    )
    model_turn_completed(
        trace,
        1,
        usage={
            "input_tokens": 1800,
            "cache_read_input_tokens": 500,
            "cache_write_input_tokens": 0,
            "output_tokens": 250,
        },
    )
    trace.event(
        {
            "type": "session_compacted",
            "session": session_summary(updated_at_ms=trace.now_ms),
            "summary": "intent: refactor parser; done: survey",
            "before_bytes": 3_200_000,
            "after_bytes": 240_000,
        },
        run=None,
    )
    trace.event(
        {"type": "run_activity_changed", "run_id": RUN,
         "activity": "waiting_for_provider"}
    )
    trace.event(
        {
            "type": "assistant_message_started",
            "message": assistant_message(message_id(5), 2, trace.now_ms),
        }
    )
    trace.event(
        {"type": "text_appended", "message_id": message_id(5),
         "channel": "output", "text": "Refactor complete."}
    )
    model_turn_completed(
        trace,
        2,
        usage={
            "input_tokens": 3200,
            "cache_read_input_tokens": 1500,
            "cache_write_input_tokens": 0,
            "output_tokens": 450,
        },
    )
    usage = {
        "input_tokens": 5000,
        "cache_read_input_tokens": 2000,
        "cache_write_input_tokens": 0,
        "output_tokens": 700,
    }
    finish_run(trace, usage=usage)
    trace.outcome("completed", 0, usage=usage)
    trace.write("compaction.trace.jsonl")


def subagent() -> None:
    trace = Trace()
    trace.trial()
    prompt_flow(trace, "Survey the codebase, then report.")
    model_turn_completed(trace, 1)
    spawn_args = '{"task":"List the crates in this workspace."}'
    trace.event(
        {"type": "tool_call_requested",
         "tool_call": tool_call(call_id(4), 1, 1, "spawn_agent", spawn_args,
                                "requested")}
    )
    trace.event(
        {"type": "tool_call_started",
         "tool_call": tool_call(call_id(4), 1, 1, "spawn_agent", spawn_args,
                                "running")}
    )
    # The child session runs through the ordinary command machinery, so its
    # events interleave with the parent's in the workspace stream.
    trace.event(
        {
            "type": "session_created",
            "session": session_summary(
                session=CHILD_SESSION,
                parent=SESSION,
                status="idle",
                active_run=None,
                updated_at_ms=trace.now_ms,
            ),
        },
        session=CHILD_SESSION,
        run=None,
    )
    prompt_flow(
        trace,
        "List the crates in this workspace.",
        session=CHILD_SESSION,
        run=CHILD_RUN,
    )
    trace.event(
        {
            "type": "assistant_message_started",
            "message": assistant_message(
                message_id(6), 1, trace.now_ms,
                session=CHILD_SESSION, run=CHILD_RUN,
            ),
        },
        session=CHILD_SESSION,
        run=CHILD_RUN,
    )
    trace.event(
        {"type": "text_appended", "message_id": message_id(6),
         "channel": "output",
         "text": "qq-core, qq-protocol, qq-provider."},
        session=CHILD_SESSION,
        run=CHILD_RUN,
    )
    child_usage = {
        "input_tokens": 900,
        "cache_read_input_tokens": 0,
        "cache_write_input_tokens": 0,
        "output_tokens": 40,
    }
    model_turn_completed(
        trace,
        1,
        session=CHILD_SESSION,
        run=CHILD_RUN,
        usage=child_usage,
        cost=3_000_000,
    )
    finish_run(
        trace,
        session=CHILD_SESSION,
        run=CHILD_RUN,
        usage=child_usage,
        accounting={
            "direct": {"usage": child_usage,
                       "estimated_cost_usd_nanos": 3_000_000},
            "inclusive": {"usage": child_usage,
                          "estimated_cost_usd_nanos": 3_000_000},
        },
    )
    trace.event(
        {"type": "tool_call_finished",
         "tool_call": tool_call(call_id(4), 1, 1, "spawn_agent", spawn_args,
                                "completed",
                                result="qq-core, qq-protocol, qq-provider.")}
    )
    trace.event(
        {"type": "run_activity_changed", "run_id": RUN,
         "activity": "waiting_for_provider"}
    )
    trace.event(
        {
            "type": "assistant_message_started",
            "message": assistant_message(message_id(7), 2, trace.now_ms),
        }
    )
    trace.event(
        {"type": "text_appended", "message_id": message_id(7),
         "channel": "output",
         "text": "The workspace holds qq-core, qq-protocol, and qq-provider."}
    )
    usage = {
        "input_tokens": 1500,
        "cache_read_input_tokens": 600,
        "cache_write_input_tokens": 0,
        "output_tokens": 120,
    }
    model_turn_completed(trace, 2, usage=usage, cost=21_000_000)
    finish_run(trace, usage=usage)
    trace.outcome(
        "completed", 0, usage=usage, estimated_cost_usd_nanos=21_000_000
    )
    trace.write("subagent.trace.jsonl")


def main() -> None:
    text_only()
    tool_loop()
    failure()
    cancellation()
    compaction()
    subagent()


if __name__ == "__main__":
    main()
