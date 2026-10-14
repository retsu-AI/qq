"""Public CLI behavior of the isolated turn-overhead recorder; no Rust build."""
import json
import hashlib
import os
import signal
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest

RECORDER = Path(__file__).with_name("record-turn-overhead.py")
FIXTURE_DIGEST = "c" * 64


class RecorderTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.baseline = self.binary("baseline", 10)
        self.candidate = self.binary("candidate", 12)

    def binary(self, name, value, malformed=False, fixture_digest=FIXTURE_DIGEST):
        path = self.root / name
        path.write_text("#!/usr/bin/env python3\n" +
            "import json, os\n" +
            f"with open(os.environ['ORDER_LOG'], 'a') as log: log.write('{name}\\n')\n" +
            "for milestone in [10,100,1000]:\n" +
            f" print(json.dumps({{'metric':f'turn_overhead_{{milestone}}_ns_per_turn','milestone':milestone,'unit':'ns/turn','iterations':1,'median':{str(malformed) if malformed else value},'samples':[{value}],'boundary':'fixed fixture','h0_registered':False,'fixture_sha256':{fixture_digest!r}}}))\n")
        path.chmod(0o755)
        return path

    def run_recorder(self, *extra, baseline=None, candidate=None, fixture_identity=True, fixture_receipts=True, receipt_changes=None):
        for arm, binary, sha in (("baseline", baseline or self.baseline, "a" * 40),
                                 ("candidate", candidate or self.candidate, "b" * 40)):
            receipt = {"format_version": 1, "compiled_fixture_verified": True,
                       "production_sha": sha, "fixture_sha": "e" * 40,
                       "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
                       "effective_fixture": {"sha256": FIXTURE_DIGEST}}
            if arm == "candidate" and receipt_changes:
                receipt.update(receipt_changes)
            (self.root / f"{arm}-fixture.json").write_text(json.dumps(receipt))
        return subprocess.run([sys.executable, str(RECORDER),
            "--baseline", str(baseline or self.baseline),
            "--candidate", str(candidate or self.candidate),
            "--baseline-sha", "a" * 40, "--candidate-sha", "b" * 40,
            "--output", str(self.root / "result"),
            *(["--baseline-fixture-sha256", FIXTURE_DIGEST,
               "--candidate-fixture-sha256", FIXTURE_DIGEST] if fixture_identity else []),
            *(["--baseline-fixture-receipt", str(self.root / "baseline-fixture.json"),
               "--candidate-fixture-receipt", str(self.root / "candidate-fixture.json")] if fixture_receipts else []), *extra],
            env=dict(os.environ, ORDER_LOG=str(self.root / "order")),
            text=True, capture_output=True, timeout=30)

    def test_verified_build_receipts_are_required_before_execution(self):
        for changes in (None, {"compiled_fixture_verified": False},
                        {"production_sha": "f" * 40}, {"binary_sha256": "0" * 64},
                        {"effective_fixture": {"sha256": "d" * 64}}):
            with self.subTest(changes=changes):
                result = self.run_recorder(fixture_receipts=changes is not None, receipt_changes=changes)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("receipt", result.stderr)
                self.assertFalse((self.root / "order").exists())
                self.assertFalse((self.root / "result").exists())

    def test_fixture_identity_is_required_before_execution(self):
        result = self.run_recorder(fixture_identity=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("fixture", result.stderr)
        self.assertFalse((self.root / "order").exists())
        self.assertFalse((self.root / "result").exists())

    def test_changed_fixture_digests_refuse_before_execution(self):
        result = self.run_recorder("--candidate-fixture-sha256", "d" * 64)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("fixtures differ", result.stderr)
        self.assertFalse((self.root / "order").exists())
        self.assertFalse((self.root / "result").exists())

    def test_malformed_fixture_identity_refuses_before_execution(self):
        result = self.run_recorder("--baseline-fixture-sha256", "not-a-digest")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("sixty-four-hex", result.stderr)
        self.assertFalse((self.root / "order").exists())

    def test_sample_fixture_mismatch_preserves_failed_evidence(self):
        candidate = self.binary("changed-fixture", 12, fixture_digest="d" * 64)
        result = self.run_recorder(candidate=candidate)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("fixture", result.stderr)
        raw = json.loads((self.root / "result" / "raw.json").read_text())
        self.assertFalse(raw["complete"])
        self.assertEqual(len(raw["runs"]), 2)
        self.assertFalse((self.root / "result" / "summary.json").exists())

    def test_complete_recording_keeps_all_alternating_and_control_samples(self):
        result = self.run_recorder()
        self.assertEqual(result.returncode, 0, result.stderr)
        order = (self.root / "order").read_text().splitlines()
        self.assertEqual(order, ["baseline", "candidate"] * 30 + ["baseline", "baseline"] * 30)
        raw = json.loads((self.root / "result" / "raw.json").read_text())
        self.assertEqual(len(raw["runs"]), 120)
        summary = json.loads((self.root / "result" / "summary.json").read_text())
        self.assertEqual(summary["AB"]["10"]["median_change_percent"], 20)
        self.assertEqual(summary["AA"]["10"]["median_change_percent"], 0)

    def test_aliasing_input_files_is_rejected_before_running(self):
        result = self.run_recorder(candidate=self.baseline)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("distinct", result.stderr)
        self.assertFalse((self.root / "order").exists())

    def test_equal_commit_shas_are_rejected_before_running(self):
        result = self.run_recorder("--candidate-sha", "a" * 40)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("distinct commits", result.stderr)
        self.assertFalse((self.root / "order").exists())
        self.assertFalse((self.root / "result").exists())

    def test_malformed_row_keeps_failed_run_evidence(self):
        malformed = self.binary("malformed", 12, True)
        result = self.run_recorder(candidate=malformed)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("metric", result.stderr)
        self.assertTrue((self.root / "result" / "AB-00-1-candidate.log").exists())
        self.assertTrue((self.root / "result" / "raw.json").exists())

    def test_existing_recording_is_never_overwritten(self):
        directory = self.root / "result"
        directory.mkdir()
        (directory / "protected").write_text("retained")
        result = self.run_recorder()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("exists", result.stderr)
        self.assertEqual((directory / "protected").read_text(), "retained")
        self.assertFalse((self.root / "order").exists())

    def test_failed_launch_preserves_attempt_and_error(self):
        self.candidate.write_text("#!/nonexistent/qq-benchmark-interpreter\n")
        result = self.run_recorder()
        self.assertNotEqual(result.returncode, 0)
        raw = json.loads((self.root / "result" / "raw.json").read_text())
        self.assertFalse(raw["complete"])
        self.assertEqual(len(raw["runs"]), 2)
        failed = raw["runs"][-1]
        self.assertEqual(failed["arm"], "candidate")
        self.assertIsNone(failed["exit_code"])
        self.assertIn("No such file", failed["error"])
        self.assertTrue((self.root / "result" / failed["log"]).exists())
        self.assertFalse((self.root / "result" / "summary.json").exists())

    def test_nonzero_exit_preserves_output_and_failure(self):
        self.candidate.write_text("#!/usr/bin/env python3\nprint('fixture failed')\nraise SystemExit(7)\n")
        result = self.run_recorder()
        self.assertNotEqual(result.returncode, 0)
        raw = json.loads((self.root / "result" / "raw.json").read_text())
        failed = raw["runs"][-1]
        self.assertEqual(failed["exit_code"], 7)
        self.assertEqual(failed["error"], "benchmark exit 7")
        self.assertIn("fixture failed", (self.root / "result" / failed["log"]).read_text())
        self.assertFalse(raw["complete"])

    def test_oversized_output_never_qualifies_even_after_clean_exit(self):
        self.candidate.write_text("#!/usr/bin/env python3\nprint('x' * (1024 * 1024 + 1))\n")
        result = self.run_recorder()
        self.assertNotEqual(result.returncode, 0)
        raw = json.loads((self.root / "result" / "raw.json").read_text())
        self.assertEqual(raw["runs"][-1]["error"], "benchmark output exceeds 1MiB")
        self.assertLessEqual((self.root / "result" / raw["runs"][-1]["log"]).stat().st_size,
                             1024 * 1024)
        self.assertFalse(raw["complete"])
        self.assertFalse((self.root / "result" / "summary.json").exists())

    def test_nonimmutable_sha_is_rejected_before_running(self):
        result = self.run_recorder("--baseline-sha", "main")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("SHA", result.stderr)
        self.assertFalse((self.root / "order").exists())

    def test_unending_fast_output_is_killed_at_the_hard_log_cap(self):
        self.candidate.write_text("#!/usr/bin/env python3\nimport os\n"
            "while True: os.write(1, b'x' * 65536)\n")
        result = self.run_recorder()
        self.assertNotEqual(result.returncode, 0)
        raw = json.loads((self.root / "result" / "raw.json").read_text())
        failed = raw["runs"][-1]
        self.assertEqual(failed["error"], "benchmark output exceeds 1MiB")
        self.assertLessEqual((self.root / "result" / failed["log"]).stat().st_size, 1024 * 1024)
        self.assertIsNotNone(failed["exit_code"])
        self.assertFalse(raw["complete"])

    def test_failed_sample_terminates_descendants_after_leader_exit(self):
        for exit_code in (7, 0):
            with self.subTest(exit_code=exit_code):
                heartbeat = self.root / f"heartbeat-{exit_code}"
                pid_file = self.root / f"pid-{exit_code}"
                self.candidate.write_text("#!/usr/bin/env python3\nimport os, time\n"
                    "pid = os.fork()\nif pid == 0:\n"
                    " fd = os.open(os.devnull, os.O_WRONLY)\n"
                    " os.dup2(fd, 1); os.dup2(fd, 2); os.close(fd)\n"
                    " while True:\n"
                    f"  with open({str(heartbeat)!r}, 'a') as log: log.write('alive\\n')\n"
                    "  time.sleep(.01)\n"
                    f"with open({str(pid_file)!r}, 'w') as log: log.write(str(pid))\n"
                    f"while not os.path.exists({str(heartbeat)!r}): time.sleep(.01)\n"
                    "print('invalid metrics', flush=True)\n"
                    f"raise SystemExit({exit_code})\n")
                try:
                    result = self.run_recorder("--output", str(self.root / f"result-{exit_code}"))
                    self.assertNotEqual(result.returncode, 0)
                    raw = json.loads((self.root / f"result-{exit_code}" / "raw.json").read_text())
                    self.assertFalse(raw["complete"])
                    self.assertEqual(raw["runs"][-1]["exit_code"], exit_code)
                    self.assertIn("invalid metrics", (self.root / f"result-{exit_code}" /
                        raw["runs"][-1]["log"]).read_text())
                    time.sleep(.1)
                    size = heartbeat.stat().st_size
                    time.sleep(.1)
                    self.assertEqual(heartbeat.stat().st_size, size, "descendant survived recorder failure")
                finally:
                    if pid_file.exists():
                        try:
                            os.kill(int(pid_file.read_text()), signal.SIGKILL)
                        except ProcessLookupError:
                            pass


if __name__ == "__main__":
    unittest.main()
