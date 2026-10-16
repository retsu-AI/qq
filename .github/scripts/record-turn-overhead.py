"""Record the existing turn diagnostic from two prebuilt optimized binaries."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import selectors
import signal
import statistics
import subprocess
import sys
import time


def pressure():
    path = Path("/proc/pressure/io")
    return path.read_text() if path.exists() else None


def validate_fixture_manifest(manifest):
    expected = ("crates/qq-core/benches/turn_overhead.rs", "crates/qq-core/tests/support/soak.rs")
    entries = manifest["sources"]
    if [entry["path"] for entry in entries] != list(expected):
        raise ValueError("receipt fixture source closure is invalid")
    digest = hashlib.sha256(b"qq-turn-fixture-v1\0")
    for entry in entries:
        if (type(entry["bytes"]) is not int or not 0 <= entry["bytes"] < 2**64
                or re.fullmatch(r"[0-9a-f]{64}", entry["sha256"]) is None):
            raise ValueError("receipt fixture source identity is invalid")
        encoded = entry["path"].encode()
        digest.update(len(encoded).to_bytes(8, "big"))
        digest.update(encoded)
        digest.update(entry["bytes"].to_bytes(8, "big"))
        digest.update(bytes.fromhex(entry["sha256"]))
    if digest.hexdigest() != manifest["sha256"]:
        raise ValueError("receipt fixture manifest digest is invalid")


def metrics(log, fixture_digest):
    rows = [json.loads(line) for line in log.splitlines() if line.startswith("{")]
    if len(rows) != 3:
        raise ValueError("metric receipt must contain exactly three rows")
    by_milestone = {}
    for row in rows:
        if row.get("fixture_sha256") != fixture_digest:
            raise ValueError("sample fixture identity differs from the compiled build receipt")
        milestone = row.get("milestone")
        value = row.get("median")
        if (type(milestone) is not int or milestone not in (10, 100, 1000)
                or milestone in by_milestone or type(value) is not int or value <= 0
                or row.get("metric") != f"turn_overhead_{milestone}_ns_per_turn"
                or row.get("iterations") != 1 or row.get("unit") != "ns/turn"
                or row.get("samples") != [value] or row.get("h0_registered") is not False
                or not isinstance(row.get("boundary"), str) or not row["boundary"]):
            raise ValueError("invalid metric receipt or fixture shape")
        by_milestone[milestone] = row
    return [by_milestone[milestone] for milestone in (10, 100, 1000)]


def summarize(runs):
    result = {}
    for suite in ("AB", "AA"):
        result[suite] = {}
        for milestone in (10, 100, 1000):
            arms = []
            for slot in (0, 1):
                values = sorted(next(row["median"] for row in run["metrics"]
                                     if row["milestone"] == milestone)
                                for run in runs if run["suite"] == suite and run["slot"] == slot)
                arms.append({"median": statistics.median(values),
                             "p95": values[math.ceil(len(values) * .95) - 1], "samples": values})
            result[suite][str(milestone)] = {
                "arms": arms,
                "median_change_percent": round(100 * (arms[1]["median"] / arms[0]["median"] - 1), 6),
            }
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--baseline-sha", required=True)
    parser.add_argument("--candidate-sha", required=True)
    parser.add_argument("--baseline-fixture-sha256", required=True)
    parser.add_argument("--candidate-fixture-sha256", required=True)
    parser.add_argument("--baseline-fixture-receipt", type=Path, required=True)
    parser.add_argument("--candidate-fixture-receipt", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    for sha in (args.baseline_sha, args.candidate_sha):
        if re.fullmatch(r"[0-9a-f]{40}", sha) is None:
            parser.error("each SHA must be immutable, lowercase, forty-hex")
    if args.baseline_sha == args.candidate_sha:
        parser.error("baseline and candidate must name distinct commits")
    for digest in (args.baseline_fixture_sha256, args.candidate_fixture_sha256):
        if re.fullmatch(r"[0-9a-f]{64}", digest) is None:
            parser.error("each fixture identity must be lowercase sixty-four-hex SHA256")
    if args.baseline_fixture_sha256 != args.candidate_fixture_sha256:
        parser.error("compiled benchmark fixtures differ; use one common pinned fixture")
    baseline, candidate = args.baseline.resolve(strict=True), args.candidate.resolve(strict=True)
    if baseline.samefile(candidate):
        parser.error("baseline and candidate must be distinct files")
    if not all(path.is_file() and os.access(path, os.X_OK) for path in (baseline, candidate)):
        parser.error("each input must be an executable file")
    paths = {"baseline": baseline, "candidate": candidate}
    hashes = {}
    for name, path in paths.items():
        with path.open('rb') as binary:
            hashes[name] = hashlib.file_digest(binary, 'sha256').hexdigest()
    receipts = {}
    for arm, receipt_path, sha in (("baseline", args.baseline_fixture_receipt, args.baseline_sha),
                                   ("candidate", args.candidate_fixture_receipt, args.candidate_sha)):
        try:
            receipt = json.loads(receipt_path.read_text())
            validate_fixture_manifest(receipt["effective_fixture"])
            if (receipt.get("format_version") != 1
                    or receipt.get("compiled_fixture_verified") is not True
                    or receipt.get("production_sha") != sha
                    or receipt.get("binary_sha256") != hashes[arm]
                    or receipt.get("effective_fixture", {}).get("sha256") != args.baseline_fixture_sha256
                    or re.fullmatch(r"[0-9a-f]{40}", receipt.get("fixture_sha", "")) is None):
                raise ValueError("unverified or mismatched build receipt")
        except (OSError, ValueError, AttributeError, TypeError, KeyError) as error:
            parser.error(f"{arm} fixture receipt: {error}")
        receipts[arm] = receipt
    if receipts["baseline"]["effective_fixture"] != receipts["candidate"]["effective_fixture"]:
        parser.error("effective fixture receipt manifests differ")
    try:
        args.output.mkdir(parents=True, exist_ok=False)
    except FileExistsError:
        parser.error("output already exists; preserve it and choose a new directory")
    record = {
        "format_version": 2, "fixture_sha256": args.baseline_fixture_sha256, "baseline_sha": args.baseline_sha, "candidate_sha": args.candidate_sha,
        "iterations": 1, "pairs_per_suite": 30, "machine": platform.platform(),
        "binaries": {name: {"path": str(path), "sha256": hashes[name]}
                     for name, path in paths.items()},
        "build_receipts": receipts,
        "pressure_before": pressure(), "runs": [], "complete": False,
    }
    raw = args.output / "raw.json"

    def save():
        temporary = args.output / "raw.pending"
        temporary.write_text(json.dumps(record, indent=2) + "\n")
        temporary.replace(raw)

    save()
    boundary = None
    for suite in ("AB", "AA"):
        for pair in range(30):
            for slot, arm in enumerate(("baseline", "candidate") if suite == "AB" else ("baseline", "baseline")):
                name = f"{suite}-{pair:02d}-{slot}-{arm}.log"
                before = pressure()
                started = time.monotonic()
                run = {"suite": suite, "pair": pair, "slot": slot, "arm": arm, "log": name,
                       "pressure_before": before, "exit_code": None}
                record["runs"].append(run)
                save()
                child = None
                failure = None
                try:
                    with (args.output / name).open("xb") as log:
                        child = subprocess.Popen([str(paths[arm])], stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                                 env=dict(os.environ, QQ_BENCH_ITERATIONS="1"), start_new_session=True)
                        with selectors.DefaultSelector() as selector:
                            selector.register(child.stdout, selectors.EVENT_READ)
                            eof = False
                            while not eof or child.poll() is None:
                                if time.monotonic() - started > 180:
                                    failure = "benchmark timeout"
                                    break
                                if eof:
                                    time.sleep(.02)
                                    continue
                                if not selector.select(timeout=.02):
                                    continue
                                remaining = 1024 * 1024 - log.tell()
                                chunk = os.read(child.stdout.fileno(), min(64 * 1024, remaining + 1))
                                if not chunk:
                                    eof = True
                                    selector.unregister(child.stdout)
                                    continue
                                log.write(chunk[:remaining])
                                if len(chunk) > remaining:
                                    failure = "benchmark output exceeds 1MiB"
                                    break
                except OSError as error:
                    failure = str(error)
                finally:
                    if child is not None:
                        try:
                            # A terminal leader can leave descendants after closing stdout.
                            # Every sample owns a fresh process group, including failed metrics.
                            try:
                                os.killpg(child.pid, signal.SIGKILL)
                            except ProcessLookupError:
                                pass  # The owned process group has already exited.
                            child.wait(timeout=5)
                        except (OSError, subprocess.TimeoutExpired) as error:
                            failure = f"{failure + '; ' if failure else ''}benchmark cleanup: {error}"
                        run["exit_code"] = child.returncode
                        child.stdout.close()
                    run["wall_seconds"] = time.monotonic() - started
                    run["pressure_after"] = pressure()
                try:
                    if failure or run["exit_code"]:
                        raise ValueError(failure or f"benchmark exit {run['exit_code']}")
                    rows = metrics((args.output / name).read_text(), args.baseline_fixture_sha256)
                    current = [row["boundary"] for row in rows]
                    if boundary is not None and current != boundary:
                        raise ValueError("metric fixture boundaries differ between samples")
                    boundary = current
                    run["metrics"] = rows
                except (ValueError, OSError) as error:
                    run["error"] = str(error)
                    save()
                    print(str(error), file=sys.stderr)
                    return 1
                save()
            print(f"{suite} pair {pair + 1}/30 saved", flush=True)
    record["pressure_after"] = pressure()
    record["summary"] = summarize(record["runs"])
    record["complete"] = True
    save()
    (args.output / "summary.json").write_text(json.dumps(record["summary"], indent=2) + "\n")
    for suite, milestones in record["summary"].items():
        for milestone, summary in milestones.items():
            baseline, candidate = summary["arms"]
            print(json.dumps({
                "suite": suite, "milestone": int(milestone), "unit": "ns/turn",
                "baseline_p95": baseline["p95"], "candidate_p95": candidate["p95"],
                "median_change_percent": summary["median_change_percent"],
            }), flush=True)
    print(json.dumps({"pressure_samples": [
        {"suite": run["suite"], "slot": run["slot"],
         "before": run["pressure_before"], "after": run["pressure_after"]}
        for run in record["runs"]
    ]}), flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
