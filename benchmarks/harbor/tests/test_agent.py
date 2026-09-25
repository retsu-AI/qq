"""Public-behavior tests for the QQ Harbor installed-agent adapter."""

from __future__ import annotations

import os
import shlex
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from qq_harbor.agent import QQAgent


class BuildCommandTests(unittest.TestCase):
    def _agent(self, **kwargs: object) -> QQAgent:
        logs_dir = Path(tempfile.mkdtemp())
        return QQAgent(logs_dir, model_name="litellm/us.anthropic.claude-sonnet-5", **kwargs)

    def _run_argv(self, agent: QQAgent, instruction: str = "do it") -> list[str]:
        command = agent._build_command(instruction)
        argv, redirect = command.rsplit(" >", 1)
        self.assertEqual(redirect, "/logs/agent/qq-run.log 2>&1")
        return shlex.split(argv)

    def test_default_approval_is_full_and_route_is_forwarded_unchanged(self) -> None:
        argv = self._run_argv(self._agent(), instruction="write a file with 'quotes'")

        self.assertEqual(argv[0], "/installed-agent/qq")
        self.assertEqual(argv[1:3], ["--model", "litellm/us.anthropic.claude-sonnet-5"])
        self.assertEqual(argv[3], "run")
        self.assertEqual(argv[argv.index("--approval") + 1], "full")
        self.assertEqual(argv[argv.index("--format") + 1], "jsonl")
        self.assertEqual(argv[argv.index("--trace") + 1], "/logs/agent/qq-trace.jsonl")
        self.assertEqual(argv[-2:], ["--", "write a file with 'quotes'"])

    def test_every_accepted_approval_mode_is_forwarded(self) -> None:
        for mode in ("read-only", "auto", "full"):
            with self.subTest(mode=mode):
                argv = self._run_argv(self._agent(approval=mode))
                self.assertEqual(argv[argv.index("--approval") + 1], mode)

    def test_unknown_approval_mode_is_rejected_before_any_run(self) -> None:
        for mode in ("ask", "yolo", "", "FULL"):
            with self.subTest(mode=mode), self.assertRaises(ValueError):
                self._agent(approval=mode)

    def test_limits_are_forwarded_only_when_set(self) -> None:
        argv = self._run_argv(self._agent())
        for flag in ("--timeout-seconds", "--max-turns", "--max-cost-usd"):
            self.assertNotIn(flag, argv)

        argv = self._run_argv(
            self._agent(timeout_seconds=900, max_turns=200, max_cost_usd=5.0)
        )
        self.assertEqual(argv[argv.index("--timeout-seconds") + 1], "900")
        self.assertEqual(argv[argv.index("--max-turns") + 1], "200")
        self.assertEqual(argv[argv.index("--max-cost-usd") + 1], "5.0")


class RunEnvTests(unittest.TestCase):
    def test_gateway_key_and_qq_config_pass_through_but_install_knobs_do_not(self) -> None:
        env = {
            "LITELLM_API_KEY": "gateway-secret",
            "ANTHROPIC_API_KEY": "anthropic-secret",
            "QQ_CONFIG_CONTENT": "(version: 1)",
            "QQ_EVAL_ARM": "A0",
            "QQ_BINARY_PATH": "/host/only",
            "QQ_BUILD_FROM_SOURCE": "1",
            "QQ_CA_BUNDLE_PATH": "/host/only/ca.pem",
            "HOME": "/home/operator",
        }
        with mock.patch.dict(os.environ, env, clear=True):
            agent = QQAgent(Path(tempfile.mkdtemp()))
            forwarded = agent._run_env()

        self.assertEqual(forwarded["LITELLM_API_KEY"], "gateway-secret")
        self.assertEqual(forwarded["ANTHROPIC_API_KEY"], "anthropic-secret")
        self.assertEqual(forwarded["QQ_CONFIG_CONTENT"], "(version: 1)")
        self.assertEqual(forwarded["QQ_EVAL_ARM"], "A0")
        self.assertEqual(forwarded["SSL_CERT_FILE"], "/installed-agent/ca-certificates.crt")
        self.assertNotIn("QQ_BINARY_PATH", forwarded)
        self.assertNotIn("QQ_BUILD_FROM_SOURCE", forwarded)
        self.assertNotIn("QQ_CA_BUNDLE_PATH", forwarded)
        self.assertNotIn("HOME", forwarded)

    def test_a_ca_bundle_is_always_resolved_on_the_host(self) -> None:
        with mock.patch.dict(os.environ, {}, clear=True):
            bundle = QQAgent(Path(tempfile.mkdtemp()))._resolve_host_ca_bundle()
        self.assertTrue(bundle.is_file())
        self.assertIn(b"BEGIN CERTIFICATE", bundle.read_bytes())

        with tempfile.NamedTemporaryFile(suffix=".pem") as override:
            with mock.patch.dict(
                os.environ, {"QQ_CA_BUNDLE_PATH": override.name}, clear=True
            ):
                chosen = QQAgent(Path(tempfile.mkdtemp()))._resolve_host_ca_bundle()
            self.assertEqual(chosen, Path(override.name).resolve())


class PostRunTests(unittest.TestCase):
    def test_a_trace_cut_off_before_its_outcome_is_recorded_not_raised(self) -> None:
        # Harbor calls populate_context_post_run while unwinding a trial its
        # own deadline cancelled; raising here would abort every other trial
        # in the job. The trace still converts to a trajectory.
        from harbor.models.agent.context import AgentContext

        logs_dir = Path(tempfile.mkdtemp())
        fixture = Path(__file__).resolve().parent / "fixtures" / "tool_loop.trace.jsonl"
        lines = [
            line
            for line in fixture.read_text(encoding="utf-8").splitlines()
            if '"type":"outcome"' not in line and '"type": "outcome"' not in line
        ]
        (logs_dir / "qq-trace.jsonl").write_text("\n".join(lines) + "\n", encoding="utf-8")

        agent = QQAgent(logs_dir)
        agent._exit_code = None
        context = AgentContext()
        agent.populate_context_post_run(context)

        self.assertTrue((logs_dir / "trajectory.json").is_file())
        self.assertIsNone(context.cost_usd)
        self.assertEqual(context.metadata["qq_status"], "interrupted_externally")
        self.assertGreater(context.metadata["qq_trace_events"], 0)


if __name__ == "__main__":
    unittest.main()
