"""Verify the pinned benchmark source and compiled dependency closure."""
import importlib.util
import tempfile
import unittest
from pathlib import Path


class FixtureTests(unittest.TestCase):
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
        inputs = " \\\n+ ".join(str(spaced / name).replace(" ", "\\ ") for name in self.fixture.SOURCES)
        dep.write_text(f"turn: {inputs}\n")
        self.fixture.verify_dependencies(spaced, dep)


if __name__ == "__main__":
    unittest.main()
