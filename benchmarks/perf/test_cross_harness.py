import copy
import json
from pathlib import Path
import sys
import tempfile
import unittest

import cross_harness as bench


class CrossHarnessTests(unittest.TestCase):
    def receipt(self, status="ok"):
        return {"schema_version": 1, "status": status, "unsupported_reason": "not available" if status == "unsupported" else None,
                "correctness": [{"name": "literal output", "passed": True, "detail": "x"}],
                "counts": {"offered": 1, "admitted": 1, "rejected": 0},
                "measurements": [{"name": "turn", "unit": "ns", "value": 10, "boundary": "submit to terminal"}],
                "spend": {key: None for key in ["provider_requests", "retries", "compactions", "child_requests", "input_tokens", "output_tokens", "cost_usd"]},
                "evaluation": None, "raw": ["x"]}

    def manifest(self):
        return {"schema_version": 1, "tiers": {"embedded_core": "ephemeral"}, "provider": {"model": "fake"},
                "filesystem": {"input.txt": "benchmark\n"}, "workloads": [{"id": "streaming", "semantics": {"text": "x"}}]}

    def test_schedule_is_seeded_randomized_paired_and_retains_warmups_and_aa(self):
        adapters = [{"id": "a"}, {"id": "b"}]
        first = bench.schedule(self.manifest(), adapters, 20, 1, 42)
        self.assertEqual(first, bench.schedule(self.manifest(), adapters, 20, 1, 42))
        self.assertNotEqual(first, bench.schedule(self.manifest(), adapters, 20, 1, 43))
        self.assertEqual(len(first), 84)
        self.assertEqual(sum(r["phase"] == "warmup" for r in first), 4)
        self.assertEqual({r["arm"] for r in first}, {"a", "b", "control_a", "control_b"})
        for offset in range(0, len(first), 2):
            self.assertEqual(first[offset]["pair"], first[offset + 1]["pair"])
            self.assertNotEqual(first[offset]["arm"], first[offset + 1]["arm"])

    def test_percentiles_require_independent_attempt_counts(self):
        self.assertIsNone(bench.summary(list(range(1, 20)))["p95"])
        self.assertEqual(bench.summary(list(range(1, 21)))["p95"], 19)
        self.assertIsNone(bench.summary(list(range(1, 100)))["p99"])
        self.assertEqual(bench.summary(list(range(1, 101)))["p99"], 99)
        self.assertEqual(bench.summary([1, 3, 9])["median_absolute_deviation"], 2)

    def test_replay_subsamples_do_not_inflate_attempt_count(self):
        receipt = self.receipt()
        receipt["measurements"] *= 100
        attempt = dict(group="aa", arm="control_a", tier="embedded_core", workload="streaming", phase="sample", receipt=receipt)
        result = bench.summarize([attempt])[0]
        self.assertEqual(result["attempt_count"], 1)
        self.assertIsNone(result["p95"])
        self.assertEqual(result["p50"], 10)
        warmup = dict(attempt, phase="warmup")
        failed = dict(attempt, error="failure")
        self.assertEqual(bench.summarize([attempt, warmup, failed]), bench.summarize([attempt]))

    def test_accounting_correctness_and_nonfinite_values_rejected(self):
        mutations = [lambda r: r["counts"].update(admitted=2),
                     lambda r: r["correctness"][0].update(passed=False),
                     lambda r: r["measurements"][0].update(value=float("nan")),
                     lambda r: r["spend"].update(cost_usd=-1)]
        for mutate in mutations:
            receipt = self.receipt()
            mutate(receipt)
            with self.assertRaises(ValueError):
                bench.validate_receipt(receipt, "streaming")
        bench.validate_receipt(self.receipt(), "streaming")
        self.assertIsNone(self.receipt()["spend"]["cost_usd"])
        with self.assertRaises(ValueError):
            bench.validate_receipt(self.receipt(), "paired_coding_task")

    def test_spend_includes_failures_and_unknowns_prevent_comparison(self):
        receipt = self.receipt()
        receipt["spend"]["provider_requests"] = 2
        receipt["spend"]["cost_usd"] = .5
        attempt = dict(group="paired", arm="a", tier="durable_library", workload="paired_coding_task", phase="sample", receipt=receipt)
        failed = dict(attempt, error="failed", receipt=self.receipt("failed"))
        result = bench.spend_accounting([attempt, failed])[0]
        self.assertEqual(result["attempts_including_warmups_and_failures"], 2)
        self.assertEqual(result["known_subtotals"]["cost_usd"], .5)
        self.assertEqual(result["unknown_attempts"]["cost_usd"], 1)
        self.assertFalse(result["total_spend_comparable"])

    def test_paired_deltas_refuse_different_boundaries(self):
        left = dict(group="paired", arm="a", tier="embedded_core", workload="streaming", pair=0, phase="sample", receipt=self.receipt())
        right = copy.deepcopy(left)
        right["arm"] = "b"
        right["receipt"]["measurements"][0]["value"] = 13
        self.assertEqual(bench.paired_observations([left, right])[0]["deltas"][0]["right_minus_left"], 3)
        right["receipt"]["measurements"][0]["boundary"] = "help startup"
        result = bench.paired_observations([left, right])[0]
        self.assertEqual(result["deltas"], [])
        self.assertIn("boundaries differ", result["incomparable_reason"])

    def execute_fake(self, script, timeout=5):
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            fake = directory / "fake.py"
            fake.write_text(script)
            entry = dict(tier="embedded_core", workload="streaming", phase="sample", pair=0, group="aa", arm="control_a", adapter="a")
            adapter = {"command": [sys.executable, str(fake), "{output}"]}
            result = bench.attempt_run(entry, adapter, self.manifest(), directory / "attempt", timeout)
            self.assertTrue((directory / "attempt" / "attempt.json").is_file())
            self.assertTrue((directory / "attempt" / "stdout").is_file())
            self.assertTrue((directory / "attempt" / "stderr").is_file())
            return copy.deepcopy(result)

    def test_unsupported_and_failure_raw_outputs_retained(self):
        receipt = self.receipt("unsupported")
        receipt["counts"] = {"offered": 0, "admitted": 0, "rejected": 0}
        receipt["measurements"] = []
        script = f"import sys\nfrom pathlib import Path\nprint('raw output')\nPath(sys.argv[1]).write_text({json.dumps(receipt)!r})\n"
        result = self.execute_fake(script)
        self.assertIsNone(result["error"])
        self.assertEqual(result["receipt"]["status"], "unsupported")
        failed = self.execute_fake("import sys\nprint('failed raw output')\nsys.exit(7)\n")
        self.assertEqual(failed["exit_code"], 7)
        self.assertIn("missing receipt", failed["error"])

    def test_malformed_receipt_retained_instead_of_crashing_campaign(self):
        result = self.execute_fake("import sys\nfrom pathlib import Path\nPath(sys.argv[1]).write_text('[]')\n")
        self.assertIn("receipt must be an object", result["error"])
        self.assertEqual(result["receipt"], [])
        self.assertFalse(result["receipt_valid"])
        self.assertFalse(bench.spend_accounting([result])[0]["total_spend_comparable"])

    def test_nonfinite_receipts_fail_one_attempt_without_breaking_serialization(self):
        for value in [float("nan"), float("inf"), float("-inf")]:
            receipt = self.receipt()
            receipt["measurements"][0]["value"] = value
            script = f"import sys\nfrom pathlib import Path\nPath(sys.argv[1]).write_text({json.dumps(receipt)!r})\n"
            result = self.execute_fake(script)
            self.assertIn("nonfinite JSON constant", result["error"])
            self.assertNotIn("receipt", result)
            self.assertFalse(result["receipt_valid"])
            json.dumps(result, allow_nan=False)

    def test_three_adapters_rejected_before_recording(self):
        with self.assertRaisesRegex(ValueError, "one or two adapters"):
            bench.schedule(self.manifest(), [{"id": "a"}, {"id": "b"}, {"id": "c"}], 1, 0, 42)

    def test_timeout_retained_and_reaped(self):
        result = self.execute_fake("while True: pass\n", timeout=.1)
        self.assertIn("timeout", result["error"])

    def test_output_is_never_overwritten(self):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "report.json"
            bench.save(path, {"first": True})
            with self.assertRaises(FileExistsError):
                bench.save(path, {"second": True})
            self.assertEqual(json.loads(path.read_text()), {"first": True})


if __name__ == "__main__":
    unittest.main()
