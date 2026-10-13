"""Record the existing turn diagnostic from two prebuilt optimized binaries."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import signal
import statistics
import subprocess
import sys
import time


def pressure():
    path = Path("/proc/pressure/io")
    return path.read_text() if path.exists() else None


def metrics(log):
    rows = [json.loads(line) for line in log.splitlines() if line.startswith("{")]
    if len(rows) != 3:
        raise ValueError("metric receipt must contain exactly three rows")
    by_milestone = {}
    for row in rows:
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
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    for sha in (args.baseline_sha, args.candidate_sha):
        if re.fullmatch(r"[0-9a-f]{40}", sha) is None:
            parser.error("each SHA must be immutable, lowercase, forty-hex")
    baseline, candidate = args.baseline.resolve(strict=True), args.candidate.resolve(strict=True)
    if baseline.samefile(candidate):
        parser.error("baseline and candidate must be distinct files")
    if not all(path.is_file() and os.access(path, os.X_OK) for path in (baseline, candidate)):
        parser.error("each input must be an executable file")
    try:
        args.output.mkdir(parents=True, exist_ok=False)
    except FileExistsError:
        parser.error("output already exists; preserve it and choose a new directory")
    paths = {"baseline": baseline, "candidate": candidate}
    hashes = {}
    for name, path in paths.items():
        with path.open('rb') as binary:
            hashes[name] = hashlib.file_digest(binary, 'sha256').hexdigest()
    record = {
        "format_version": 1, "baseline_sha": args.baseline_sha, "candidate_sha": args.candidate_sha,
        "iterations": 1, "pairs_per_suite": 30, "machine": platform.platform(),
        "binaries": {name: {"path": str(path), "sha256": hashes[name]}
                     for name, path in paths.items()},
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
                        child = subprocess.Popen([str(paths[arm])], stdout=log, stderr=subprocess.STDOUT,
                                                 env=dict(os.environ, QQ_BENCH_ITERATIONS="1"), start_new_session=True)
                        while child.poll() is None:
                            if time.monotonic() - started > 180:
                                failure = "benchmark timeout"
                                break
                            if log.tell() > 1024 * 1024:
                                failure = "benchmark output exceeds 1MiB"
                                break
                            time.sleep(.02)
                except OSError as error:
                    failure = str(error)
                finally:
                    if child is not None:
                        try:
                            if child.poll() is None:
                                try:
                                    os.killpg(child.pid, signal.SIGKILL)
                                except ProcessLookupError:
                                    pass  # The child exited between poll and kill.
                            child.wait(timeout=5)
                        except (OSError, subprocess.TimeoutExpired) as error:
                            failure = f"{failure + '; ' if failure else ''}benchmark cleanup: {error}"
                        run["exit_code"] = child.returncode
                    run["wall_seconds"] = time.monotonic() - started
                    run["pressure_after"] = pressure()
                if not failure and (args.output / name).stat().st_size > 1024 * 1024:
                    failure = "benchmark output exceeds 1MiB"
                try:
                    if failure or run["exit_code"]:
                        raise ValueError(failure or f"benchmark exit {run['exit_code']}")
                    rows = metrics((args.output / name).read_text())
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
    return 0


if __name__ == "__main__":
    sys.exit(main())
