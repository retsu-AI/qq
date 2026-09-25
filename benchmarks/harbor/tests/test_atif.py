"""Public-behavior tests for QQ trace to ATIF conversion."""

from __future__ import annotations

import json
import re
import runpy
import tempfile
import unittest
from pathlib import Path

from qq_harbor.atif import ATIF_SCHEMA_VERSION, TraceError, convert_trace, load_trace


FIXTURES = Path(__file__).resolve().parent / "fixtures"
SMOKE_TASK = Path(__file__).resolve().parents[1] / "smoke-task"
PROTOCOL_SOURCE = Path(__file__).resolve().parents[3] / "crates/qq-protocol/src/lib.rs"


def current_protocol_version() -> int:
    versions = re.findall(
        r"^pub const PROTOCOL_VERSION: u16 = (\d+);$",
        PROTOCOL_SOURCE.read_text(encoding="utf-8"),
        re.MULTILINE,
    )
    if len(versions) != 1:
        raise AssertionError("expected one authoritative Rust PROTOCOL_VERSION declaration")
    return int(versions[0])


class AtifConversionTests(unittest.TestCase):
    def test_fixture_generator_uses_current_protocol_without_writing_fixtures(self) -> None:
        generator = runpy.run_path(str(FIXTURES.parent / "make_fixtures.py"))
        trace = generator["Trace"]()
        trace.trial()
        self.assertEqual(trace.lines[0]["protocol_version"], current_protocol_version())

    def test_all_durable_trace_shapes_convert_deterministically(self) -> None:
        for trace in sorted(FIXTURES.glob("*.trace.jsonl")):
            with self.subTest(trace=trace.name):
                first = convert_trace(load_trace(trace))
                second = convert_trace(load_trace(trace))
                self.assertEqual(first, second)
                self.assertEqual(first["schema_version"], ATIF_SCHEMA_VERSION)
                self.assertEqual(first["agent"]["name"], "qq")
                self.assertTrue(first["steps"])
                qq = first["extra"]["qq"]
                self.assertEqual(qq["protocol_version"], 23)
                self.assertEqual(qq["qq_source_revision"], "fixture-revision")
                self.assertEqual(qq["outcome"]["prompt_identity"]["version"], 7)
                json.dumps(first)

    def test_parent_child_trace_embeds_a_referenced_subagent_trajectory(self) -> None:
        trajectory = convert_trace(load_trace(FIXTURES / "subagent.trace.jsonl"))

        children = trajectory["subagent_trajectories"]
        self.assertEqual(len(children), 1)
        child_id = children[0]["trajectory_id"]
        rendered = json.dumps(trajectory)
        self.assertIn(child_id, rendered)

    def test_model_turn_identity_usage_and_cost_are_preserved(self) -> None:
        trajectory = convert_trace(load_trace(FIXTURES / "text_only.trace.jsonl"))

        step = next(step for step in trajectory["steps"] if step["source"] == "agent")
        self.assertEqual(step["model_name"], "anthropic/claude-sonnet-4-5")
        self.assertEqual(
            step["metrics"],
            {
                "prompt_tokens": 120,
                "completion_tokens": 9,
                "cached_tokens": 0,
                "cost_usd": 0.0012345,
            },
        )

    def test_malformed_or_incomplete_trace_fails_loudly(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            malformed = Path(directory) / "trace.jsonl"
            malformed.write_text('{"type":"trial"}\nnot-json\n', encoding="utf-8")
            with self.assertRaisesRegex(TraceError, "line 2"):
                load_trace(malformed)

        with self.assertRaisesRegex(TraceError, "no trial record"):
            convert_trace([])

    def test_harbor_accepts_every_generated_trajectory(self) -> None:
        from harbor.utils.trajectory_utils import format_trajectory_json
        from harbor.utils.trajectory_validator import TrajectoryValidator

        validator = TrajectoryValidator()
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "trajectory.json"
            for trace in sorted(FIXTURES.glob("*.trace.jsonl")):
                with self.subTest(trace=trace.name):
                    trajectory = convert_trace(load_trace(trace))
                    output.write_text(
                        format_trajectory_json(trajectory) + "\n", encoding="utf-8"
                    )
                    self.assertTrue(
                        validator.validate(output),
                        "; ".join(validator.get_errors()),
                    )

    def test_installed_agent_rejects_a_missing_durable_trace(self) -> None:
        from qq_harbor.agent import QQAgent

        with tempfile.TemporaryDirectory() as directory:
            agent = QQAgent.__new__(QQAgent)
            agent.logs_dir = Path(directory)
            with self.assertRaisesRegex(RuntimeError, "no durable trace"):
                agent.populate_context_post_run(object())

    def test_installed_agent_reports_qq_semver(self) -> None:
        from qq_harbor.agent import QQAgent

        self.assertEqual(QQAgent.parse_version(object(), "qq 0.1.0\n"), "0.1.0")

    def test_smoke_task_is_a_loadable_harbor_task(self) -> None:
        from harbor.models.task.task import Task

        self.assertTrue(Task.is_valid_dir(SMOKE_TASK))
        task = Task(SMOKE_TASK)
        self.assertEqual(task.name, "smoke-task")
        self.assertIn("qq-smoke.txt", task.instruction)


if __name__ == "__main__":
    unittest.main()
