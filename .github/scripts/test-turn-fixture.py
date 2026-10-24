"""Verify the pinned benchmark source and compiled dependency closure."""
import importlib.util
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


class FixtureTests(unittest.TestCase):

    def test_h0_dispatch_refuses_changed_budgets_outside_pinned_legacy_contract(self):
        import os
        import textwrap
        workflow = Path(__file__).parents[1].joinpath("workflows/ci.yml").read_text()
        start = workflow.index("          baseline = json.loads(Path('baseline/benchmarks/perf/budgets-v1.json')")
        end = workflow.index("          PYTHON", start)
        code = textwrap.dedent(workflow[start:end])
        original = Path.cwd()
        baseline_sha = os.environ.get("BASELINE_SHA")
        try:
            os.chdir(self.root)
            os.environ["BASELINE_SHA"] = "a" * 40
            for arm in ["baseline", "candidate"]:
                path = Path(arm) / "benchmarks/perf"
                path.mkdir(parents=True)
                (path / "budgets-v1.json").write_text(json.dumps({"fixture_version": 5, "metrics": [{"metric": "provider_retry_amplification_milli", "maximum_p95": 6300}]}))
            exec(code, {"json": json, "Path": Path, "os": os})
            candidate = Path("candidate/benchmarks/perf/budgets-v1.json")
            changed = json.loads(candidate.read_text())
            changed["metrics"][0]["maximum_p95"] = 7000
            candidate.write_text(json.dumps(changed))
            with self.assertRaisesRegex(SystemExit, "unchanged budgets"):
                exec(code, {"json": json, "Path": Path, "os": os})
        finally:
            os.chdir(original)
            if baseline_sha is None:
                os.environ.pop("BASELINE_SHA", None)
            else:
                os.environ["BASELINE_SHA"] = baseline_sha

    def test_benchmark_arm_comparison_rejects_debuginfo_mismatch(self):
        import textwrap
        workflow = Path(__file__).parents[1].joinpath("workflows/ci.yml").read_text()
        start = workflow.index("              for key in ('debug_assertions',")
        end = workflow.index("          PYTHON", start)
        code = textwrap.dedent(workflow[start:end])
        profile = {"debug_assertions": False, "overflow_checks": False, "test": False, "debuginfo": 0}
        exec(code, {"baseline": {"profile": profile}, "profile": profile.copy()})
        for key in profile:
            candidate = profile.copy()
            candidate[key] = 2 if key == "debuginfo" else True
            with self.subTest(setting=key), self.assertRaisesRegex(SystemExit, key):
                exec(code, {"baseline": {"profile": profile}, "profile": candidate})

    def test_workflow_optimization_fallback_matches_build_and_validation(self):
        workflow = Path(__file__).parents[1].joinpath("workflows/ci.yml").read_text()
        build_line = next(line for line in workflow.splitlines() if "final_opt=$(python3" in line)
        build = build_line.split("print(", 1)[1].rsplit(")", 2)[0]
        validation_line = next(line for line in workflow.splitlines() if "expected_opt = str(" in line)
        validation = validation_line.split("str(", 1)[1].rsplit(")", 1)[0]
        for release, expected in [
            ({}, "3"),
            ({"opt-level": "s"}, "s"),
            ({"opt-level": 2}, "2"),
            ({"opt-level": "s", "package": {"qq-core": {"opt-level": "z"}}}, "z"),
        ]:
            with self.subTest(release=release):
                self.assertEqual(str(eval(build, {"p": release})), expected)
                self.assertEqual(str(eval(validation, {"release": release})), expected)
    def setUp(self):
        spec = importlib.util.spec_from_file_location(
            "fixture", Path(__file__).with_name("turn-fixture.py"))
        self.fixture = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.fixture)
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        for name in self.fixture.SOURCES:
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(name.encode())

    def test_each_included_source_changes_identity(self):
        original = self.fixture.manifest(self.root)
        for name in self.fixture.SOURCES:
            path = self.root / name
            previous = path.read_bytes()
            path.write_bytes(previous + b"changed")
            self.assertNotEqual(original, self.fixture.manifest(self.root))
            path.write_bytes(previous)

    def test_dependency_closure_accepts_only_the_declared_sources(self):
        dep = self.root / "turn.d"
        inputs = " ".join(str(self.root / name) for name in self.fixture.SOURCES)
        dep.write_text(f"turn: {inputs}\n")
        self.fixture.verify_dependencies(self.root, dep)
        dep.write_text(f"turn: {inputs} {self.root / 'extra.rs'}\n")
        with self.assertRaisesRegex(ValueError, "dependency closure"):
            self.fixture.verify_dependencies(self.root, dep)
        dep.write_text(f"turn: {self.root / self.fixture.SOURCES[0]}\n")
        with self.assertRaisesRegex(ValueError, "dependency closure"):
            self.fixture.verify_dependencies(self.root, dep)

    def test_dependency_closure_supports_continuation_and_escaped_spaces(self):
        spaced = self.root / "source root"
        for name in self.fixture.SOURCES:
            path = spaced / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"source")
        dep = self.root / "turn.d"
        inputs = " \\\n ".join(str(spaced / name).replace(" ", "\\ ") for name in self.fixture.SOURCES)
        dep.write_text(f"turn: {inputs}\n")
        self.fixture.verify_dependencies(spaced, dep)

    def test_prepare_and_verify_preserve_production_identity_and_refuse_other_changes(self):
        production, fixture = self.root / "production", self.root / "fixture"
        for checkout, content in ((production, b"old"), (fixture, b"common")):
            for name in self.fixture.SOURCES:
                path = checkout / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(content)
            (checkout / "production.rs").write_bytes(content)
            subprocess.run(["git", "init", "-q", str(checkout)], check=True)
            subprocess.run(["git", "-C", str(checkout), "add", "."], check=True)
            subprocess.run(["git", "-C", str(checkout), "-c", "user.name=fixture",
                            "-c", "user.email=fixture@example.invalid", "commit", "-qm", "fixture"], check=True)
        receipt = self.root / "receipt.json"
        head = self.fixture.git(production, "rev-parse", "HEAD")
        command = [sys.executable, str(Path(__file__).with_name("turn-fixture.py"))]
        prepare = command + ["prepare", "--production-root", str(production),
                             "--fixture-root", str(fixture), "--production-sha", head,
                             "--fixture-sha", self.fixture.git(fixture, "rev-parse", "HEAD"),
                             "--receipt", str(receipt)]
        subprocess.run(prepare, check=True, capture_output=True)
        saved = json.loads(receipt.read_text())
        self.assertEqual(saved["production_sha"], head)
        self.assertNotEqual(saved["original_fixture"], saved["effective_fixture"])
        self.assertEqual(saved["effective_fixture"], self.fixture.manifest(production))
        self.assertFalse(saved["compiled_fixture_verified"])
        self.assertNotEqual(subprocess.run(prepare, capture_output=True).returncode, 0)
        binary = self.root / "binary"
        binary.write_bytes(b"compiled binary")
        dep = self.root / "compiled.d"
        dep.write_text("turn: " + " ".join(str(production / name) for name in self.fixture.SOURCES) + "\n")
        verify = command + ["verify", "--production-root", str(production), "--receipt", str(receipt),
                            "--dep-info", str(dep), "--binary", str(binary)]
        subprocess.run(verify, check=True, capture_output=True)
        self.assertTrue(json.loads(receipt.read_text())["compiled_fixture_verified"])
        (production / "production.rs").write_bytes(b"unexpected production change")
        failed = subprocess.run(verify, capture_output=True, text=True)
        self.assertNotEqual(failed.returncode, 0)
        self.assertIn("outside the pinned fixture", failed.stderr)
        (production / "production.rs").write_bytes(b"old")
        (production / self.fixture.SOURCES[1]).write_bytes(b"late fixture change")
        failed = subprocess.run(verify, capture_output=True, text=True)
        self.assertNotEqual(failed.returncode, 0)
        self.assertIn("changed after preparation", failed.stderr)


if __name__ == "__main__":
    unittest.main()
