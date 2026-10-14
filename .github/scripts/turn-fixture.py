"""Pin the turn diagnostic's direct sources independently of production code."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shlex
import subprocess

SOURCES = ("crates/qq-core/benches/turn_overhead.rs", "crates/qq-core/tests/support/soak.rs")


def manifest(root):
    digest = hashlib.sha256(b"qq-turn-fixture-v1\0")
    entries = []
    for name in sorted(SOURCES):
        path = root / name
        if path.is_symlink() or not path.is_file():
            raise ValueError(f"fixture source must be a regular file: {name}")
        data = path.read_bytes()
        source_hash = hashlib.sha256(data).digest()
        encoded = name.encode()
        digest.update(len(encoded).to_bytes(8, "big"))
        digest.update(encoded)
        digest.update(len(data).to_bytes(8, "big"))
        digest.update(source_hash)
        entries.append({"path": name, "bytes": len(data), "sha256": source_hash.hex()})
    return {"sha256": digest.hexdigest(), "sources": entries}


def git(root, *args):
    return subprocess.check_output(["git", "-C", str(root), *args]).decode().strip()


def require_head(root, sha):
    if re.fullmatch(r"[0-9a-f]{40}", sha) is None or git(root, "rev-parse", "HEAD") != sha:
        raise ValueError("checkout must match its immutable forty-hex commit")


def verify_dependencies(root, dep_info):
    line = dep_info.read_text().replace("\\\n", "").splitlines()[0]
    _, inputs = line.split(": ", 1)
    actual = {((root / name) if not Path(name).is_absolute() else Path(name)).resolve()
              for name in shlex.split(inputs)}
    expected = {(root / name).resolve() for name in SOURCES}
    if actual != expected:
        raise ValueError("compiled benchmark dependency closure differs from declared fixture sources")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=("prepare", "verify"))
    parser.add_argument("--production-root", type=Path, required=True)
    parser.add_argument("--receipt", type=Path, required=True)
    parser.add_argument("--fixture-root", type=Path)
    parser.add_argument("--production-sha")
    parser.add_argument("--fixture-sha")
    parser.add_argument("--dep-info", type=Path)
    parser.add_argument("--binary", type=Path)
    args = parser.parse_args()
    root = args.production_root.resolve(strict=True)
    if args.operation == "prepare":
        if not all((args.fixture_root, args.production_sha, args.fixture_sha)):
            parser.error("prepare requires fixture root and both commit identities")
        fixture = args.fixture_root.resolve(strict=True)
        require_head(root, args.production_sha)
        require_head(fixture, args.fixture_sha)
        for checkout in (root, fixture):
            if git(checkout, "status", "--porcelain", "--untracked-files=no"):
                raise ValueError("tracked checkout changes are forbidden before fixture preparation")
        original, effective = manifest(root), manifest(fixture)
        receipt = {"format_version": 1, "production_sha": args.production_sha,
                   "fixture_sha": args.fixture_sha, "original_fixture": original,
                   "effective_fixture": effective, "overlay_paths": list(SOURCES),
                   "compiled_fixture_verified": False}
        with args.receipt.open("x") as output:
            output.write(json.dumps(receipt, indent=2) + "\n")
            output.flush()
            for name in SOURCES:
                (root / name).write_bytes((fixture / name).read_bytes())
    else:
        if not all((args.dep_info, args.binary)):
            parser.error("verify requires compiler dep-info and the saved binary")
        receipt = json.loads(args.receipt.read_text())
        require_head(root, receipt["production_sha"])
        if receipt["overlay_paths"] != list(SOURCES) or manifest(root) != receipt["effective_fixture"]:
            raise ValueError("effective fixture changed after preparation")
        for diff in (("diff", "--name-only", "HEAD"), ("ls-files", "--others", "--exclude-standard")):
            changed = set(git(root, *diff).splitlines())
            if changed - set(SOURCES):
                raise ValueError("production checkout changed outside the pinned fixture")
        verify_dependencies(root, args.dep_info)
        with args.binary.open("rb") as binary:
            receipt["binary_sha256"] = hashlib.file_digest(binary, "sha256").hexdigest()
        receipt["compiled_fixture_verified"] = True
        args.receipt.write_text(json.dumps(receipt, indent=2) + "\n")


if __name__ == "__main__":
    main()
