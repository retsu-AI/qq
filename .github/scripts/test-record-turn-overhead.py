"""Public CLI behavior of the isolated turn-overhead recorder; no Rust build."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

RECORDER = Path(__file__).with_name("record-turn-overhead.py")


class RecorderTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.baseline = self.binary("baseline", 10)
        self.candidate = self.binary("candidate", 12)

    def binary(self, name, value, malformed=False):
        path = self.root / name
        path.write_text("#!/usr/bin/env python3\n" +
            "import json, os\n" +
            f"with open(os.environ['ORDER_LOG'], 'a') as log: log.write('{name}\\n')\n" +
            "for milestone in [10,100,1000]:\n" +
            f" print(json.dumps({{'metric':f'turn_overhead_{{milestone}}_ns_per_turn','milestone':milestone,'unit':'ns/turn','iterations':1,'median':{str(malformed) if malformed else value},'samples':[{value}],'boundary':'fixed fixture','h0_registered':False}}))\n")
        path.chmod(0o755)
        return path

    def run_recorder(self, *extra, baseline=None, candidate=None):
        return subprocess.run([sys.executable, str(RECORDER),
            "--baseline", str(baseline or self.baseline),
            "--candidate", str(candidate or self.candidate),
            "--baseline-sha", "a" * 40, "--candidate-sha", "b" * 40,
            "--output", str(self.root / "result"), *extra],
            env=dict(os.environ, ORDER_LOG=str(self.root / "order")),
            text=True, capture_output=True, timeout=30)

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
        self.assertFalse(raw["complete"])
        self.assertFalse((self.root / "result" / "summary.json").exists())

    def test_nonimmutable_sha_is_rejected_before_running(self):
        result = self.run_recorder("--baseline-sha", "main")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("SHA", result.stderr)
        self.assertFalse((self.root / "order").exists())


if __name__ == "__main__":
    unittest.main()
