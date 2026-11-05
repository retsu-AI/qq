import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import random
import signal
import statistics
import subprocess
import sys
import time


def digest(data):
    return hashlib.sha256(data).hexdigest()


def encode(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode()


def save(path, value):
    with path.open("x") as stream:
        json.dump(value, stream, indent=2, allow_nan=False)
        stream.write("\n")


def capture(command, cwd=None):
    result = subprocess.run(command, cwd=cwd, capture_output=True, timeout=60, check=False)
    if result.returncode:
        raise ValueError(f"metadata command failed: {command}: {result.stderr.decode(errors='replace')}")
    return result.stdout


def source_identity(root):
    root = Path(root).resolve()
    revision = capture(["git", "rev-parse", "HEAD"], root).decode().strip()
    status = capture(["git", "status", "--porcelain=v1", "-z", "--untracked-files=all"], root)
    paths = capture(["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"], root)
    submodules = capture(["git", "submodule", "status", "--recursive"], root)
    manifest = hashlib.sha256(submodules)
    for raw in sorted(set(paths.split(b"\0")) - {b""}):
        path = root / os.fsdecode(raw)
        manifest.update(raw + b"\0")
        if path.is_symlink():
            manifest.update(b"symlink\0" + os.fsencode(os.readlink(path)))
        elif path.is_file():
            manifest.update(str(path.stat().st_mode).encode() + b"\0" + bytes.fromhex(digest(path.read_bytes())))
        else:
            manifest.update(b"missing-or-submodule")
    return {"revision": revision, "dirty": bool(status), "status_sha256": digest(status),
            "content_sha256": manifest.hexdigest()}


def schedule(manifest, adapters, samples, warmups, seed):
    rng = random.Random(seed)
    result = []
    arms = [(a["id"], a["id"]) for a in adapters]
    controls = [("control_a", adapters[0]["id"]), ("control_b", adapters[0]["id"])]
    for tier in manifest["tiers"]:
        for workload in manifest["workloads"]:
            for phase, count in [("warmup", warmups), ("sample", samples)]:
                for pair in range(count):
                    for group, entries in [("aa", controls), ("paired", arms)]:
                        entries = entries.copy()
                        rng.shuffle(entries)
                        for arm, adapter in entries:
                            result.append(dict(tier=tier, workload=workload["id"], phase=phase,
                                               pair=pair, group=group, arm=arm, adapter=adapter))
    return result


def validate_receipt(receipt, workload):
    if not isinstance(receipt, dict):
        raise ValueError("receipt must be an object")
    if receipt.get("schema_version") != 1 or receipt.get("status") not in {"ok", "failed", "unsupported"}:
        raise ValueError("unsupported receipt schema or status")
    if receipt["status"] == "unsupported":
        if not receipt.get("unsupported_reason"):
            raise ValueError("unsupported result needs a reason")
        if receipt.get("measurements") or receipt.get("counts") != {"offered": 0, "admitted": 0, "rejected": 0}:
            raise ValueError("unsupported result must not claim measurements or admissions")
        return
    counts = receipt["counts"]
    if not isinstance(counts, dict) or not isinstance(receipt["measurements"], list) or not isinstance(receipt["correctness"], list) or not isinstance(receipt["spend"], dict):
        raise ValueError("invalid receipt field types")
    if any(not isinstance(c, dict) for c in receipt["correctness"]) or any(not isinstance(m, dict) for m in receipt["measurements"]):
        raise ValueError("checks and measurements must be objects")
    values = [counts[k] for k in ["offered", "admitted", "rejected"]]
    if any(v is not None and (type(v) is not int or v < 0) for v in values):
        raise ValueError("counts must be nonnegative integers or unknown null")
    if all(v is not None for v in values) and values[0] != values[1] + values[2]:
        raise ValueError("offered must equal admitted plus rejected")
    if receipt["status"] == "ok":
        if None in values or not receipt["correctness"] or any(c.get("passed") is not True for c in receipt["correctness"]):
            raise ValueError("successful result needs known counts and passing correctness receipts")
        if not receipt["measurements"]:
            raise ValueError("successful workload needs measurements")
        if workload == "paired_coding_task":
            evaluation = receipt.get("evaluation")
            if not evaluation or type(evaluation.get("total")) is not int or evaluation["total"] <= 0:
                raise ValueError("coding task needs independent test evaluation")
            if not 0 <= evaluation["passed"] <= evaluation["total"]:
                raise ValueError("invalid independent test score")
    for metric in receipt["measurements"]:
        if type(metric["value"]) not in {int, float} or not math.isfinite(metric["value"]) or metric["value"] < 0:
            raise ValueError("measurement must be finite and nonnegative")
        if not all(metric.get(k) for k in ["name", "unit", "boundary"]):
            raise ValueError("measurement needs name, unit and exact boundary")
    for key in ["provider_requests", "retries", "compactions", "child_requests", "input_tokens", "output_tokens", "cost_usd"]:
        value = receipt["spend"][key]
        if value is not None and (type(value) not in {int, float} or not math.isfinite(value) or value < 0):
            raise ValueError("spend must be nonnegative or unknown null")


def summary(values):
    ordered = sorted(values)
    n = len(values)
    rank = lambda p: ordered[math.ceil(n * p) - 1]
    median = rank(.5)
    return {"attempt_count": n, "p50": median, "p95": rank(.95) if n >= 20 else None,
            "p99": rank(.99) if n >= 100 else None, "minimum": ordered[0], "maximum": ordered[-1],
            "median_absolute_deviation": statistics.median(abs(v - median) for v in values),
            "standard_deviation": statistics.stdev(values) if n >= 2 else None}


def summarize(attempts):
    buckets = {}
    for attempt in attempts:
        receipt = attempt.get("receipt")
        if attempt.get("error") or not receipt or receipt["status"] != "ok" or attempt["phase"] != "sample":
            continue
        clustered = {}
        for metric in receipt["measurements"]:
            key = tuple(attempt[k] for k in ["group", "arm", "tier", "workload"]) + tuple(metric[k] for k in ["name", "unit", "boundary"])
            clustered.setdefault(key, []).append(metric["value"])
        for key, values in clustered.items():
            buckets.setdefault(key, []).append(statistics.median(values))
    keys = ["group", "arm", "tier", "workload", "metric", "unit", "boundary"]
    return [dict(zip(keys, key), **summary(values)) for key, values in sorted(buckets.items())]


def paired_observations(attempts):
    pairs = {}
    for attempt in attempts:
        if attempt["phase"] != "sample":
            continue
        key = tuple(attempt[k] for k in ["group", "tier", "workload", "pair"])
        pairs.setdefault(key, []).append(attempt)
    observations = []
    for key, entries in pairs.items():
        if len(entries) != 2:
            continue
        left, right = sorted(entries, key=lambda a: a["arm"])
        record = dict(zip(["group", "tier", "workload", "pair"], key), left=left["arm"], right=right["arm"], deltas=[])
        if any(a.get("error") or a.get("receipt", {}).get("status") != "ok" for a in entries):
            record["incomparable_reason"] = "failed or unsupported paired member"
        else:
            def metrics(attempt):
                grouped = {}
                for metric in attempt["receipt"]["measurements"]:
                    metric_key = tuple(metric[k] for k in ["name", "unit", "boundary"])
                    grouped.setdefault(metric_key, []).append(metric["value"])
                return {k: statistics.median(v) for k, v in grouped.items()}
            first, second = metrics(left), metrics(right)
            if set(first) != set(second):
                record["incomparable_reason"] = "metric names, units or boundaries differ"
            else:
                record["deltas"] = [dict(zip(["metric", "unit", "boundary"], k), right_minus_left=second[k]-first[k]) for k in first]
        observations.append(record)
    return observations


def spend_accounting(attempts):
    buckets = {}
    fields = ["provider_requests", "retries", "compactions", "child_requests", "input_tokens", "output_tokens", "cost_usd"]
    for attempt in attempts:
        key = tuple(attempt[k] for k in ["group", "arm", "tier", "workload"])
        bucket = buckets.setdefault(key, {"attempts_including_warmups_and_failures": 0, "known_subtotals": dict.fromkeys(fields, 0), "unknown_attempts": dict.fromkeys(fields, 0)})
        bucket["attempts_including_warmups_and_failures"] += 1
        spend = attempt.get("receipt", {}).get("spend", {}) if attempt.get("receipt_valid", True) else {}
        if not isinstance(spend, dict):
            spend = {}
        for field in fields:
            value = spend.get(field)
            if type(value) in {int, float} and math.isfinite(value) and value >= 0:
                bucket["known_subtotals"][field] += value
            else:
                bucket["unknown_attempts"][field] += 1
    return [dict(zip(["group", "arm", "tier", "workload"], key), **bucket,
                 total_spend_comparable=all(v == 0 for v in bucket["unknown_attempts"].values()) and all(
                     a.get("receipt", {}).get("spend", {}).get("accounting_scope") == "all_attempts_including_retries_compaction_children"
                     for a in attempts if tuple(a[k] for k in ["group", "arm", "tier", "workload"]) == key)) for key, bucket in sorted(buckets.items())]


def host_metadata(machine_class):
    def optional(path):
        try:
            return Path(path).read_text()
        except OSError:
            return None
    return {"machine_class": machine_class, "platform": platform.platform(), "uname": list(platform.uname()),
            "python": sys.version, "cpu_count": os.cpu_count(), "load_average": list(os.getloadavg()) if hasattr(os, "getloadavg") else None,
            "cpuinfo": optional("/proc/cpuinfo"), "meminfo": optional("/proc/meminfo"),
            "mountinfo": optional("/proc/self/mountinfo"), "governor": optional("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor")}


def attempt_run(entry, adapter, manifest, directory, timeout):
    directory.mkdir()
    workspace = directory / "workspace"
    workspace.mkdir()
    for name, content in manifest["filesystem"].items():
        target = workspace / name
        if Path(name).is_absolute() or ".." in Path(name).parts:
            raise ValueError("filesystem fixture escapes workspace")
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content)
    files = dict(manifest["filesystem"])
    if entry["workload"] == "paired_coding_task":
        files["solution.py"] = next(w for w in manifest["workloads"] if w["id"] == entry["workload"])["semantics"]["initial_file"]
        (workspace / "solution.py").write_text(files["solution.py"])
    request = dict(entry, manifest_sha256=digest(encode(manifest)),
                   provider_sha256=digest(encode(manifest["provider"])), filesystem_sha256=digest(encode(files)),
                   semantics=next(w["semantics"] for w in manifest["workloads"] if w["id"] == entry["workload"]))
    save(directory / "request.json", request)
    replacements = dict(entry, output=str(directory / "receipt.json"), request=str(directory / "request.json"), workspace=str(workspace))
    command = [part.format_map(replacements) for part in adapter["command"]]
    env = {key: os.environ[key] for key in ["PATH", "LANG", "LC_ALL", "SYSTEMROOT", "TMPDIR", "LD_LIBRARY_PATH"] if key in os.environ}
    env["PYTHONDONTWRITEBYTECODE"] = "1"
    for key in ["HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME"]:
        home = directory / key.lower()
        home.mkdir()
        env[key] = str(home)
    record = dict(entry, command=command, directory=str(directory), error=None, receipt_valid=False)
    started = time.monotonic_ns()
    process = None
    with (directory / "stdout").open("xb") as stdout, (directory / "stderr").open("xb") as stderr:
        try:
            process = subprocess.Popen(command, cwd=workspace, env=env, stdout=stdout, stderr=stderr, start_new_session=True)
            record["exit_code"] = process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
            record["error"] = "timeout; process group killed and reaped"
        except OSError as error:
            record["error"] = str(error)
        finally:
            if process is not None:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
    record["wall_ns"] = time.monotonic_ns() - started
    try:
        receipt = json.loads((directory / "receipt.json").read_text())
        record["receipt"] = receipt
        validate_receipt(receipt, entry["workload"])
        record["receipt_valid"] = True
        if receipt["status"] == "failed" or record.get("exit_code") != 0:
            record["error"] = record["error"] or "adapter failed; inspect receipt and raw outputs"
    except (OSError, ValueError, KeyError, TypeError) as error:
        record["error"] = record["error"] or f"invalid or missing receipt: {error}"
    record["stdout_sha256"] = digest((directory / "stdout").read_bytes())
    record["stderr_sha256"] = digest((directory / "stderr").read_bytes())
    save(directory / "attempt.json", record)
    return record


def run(args):
    manifest_bytes = args.manifest.read_bytes()
    manifest = json.loads(manifest_bytes)
    config_bytes = args.adapters.read_bytes()
    config = json.loads(config_bytes)
    if manifest["schema_version"] != 1 or config["schema_version"] != 1:
        raise ValueError("unsupported manifest or adapter schema")
    if args.samples < 1 or args.warmups < 0 or args.timeout_seconds <= 0:
        raise ValueError("invalid sample, warmup or timeout count")
    adapters = config["adapters"]
    if not adapters or len({a["id"] for a in adapters}) != len(adapters):
        raise ValueError("adapter ids must be nonempty and unique")
    if {a["id"] for a in adapters} & {"control_a", "control_b"}:
        raise ValueError("control_a/control_b are reserved arm names")
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    metadata = {}
    for adapter in adapters:
        executable = Path(adapter["command"][0])
        if not executable.is_absolute() or not executable.is_file():
            raise ValueError("adapter command must start with an absolute executable")
        identity = source_identity(adapter["source"]["root"])
        if identity["revision"] != adapter["source"]["revision"]:
            raise ValueError("adapter revision differs from pinned revision")
        source = adapter["source"]
        if not source.get("build_command") or not source.get("profile"):
            raise ValueError("source needs explicit build_command and profile")
        build_metadata = {"commands": []}
        for command in adapter.get("build_metadata_commands", []):
            build_metadata["commands"].append({"command": command, "stdout": capture(command, source["root"]).decode(errors="replace")})
        metadata[adapter["id"]] = dict(adapter, observed_source=identity, build_metadata=build_metadata,
            executable_sha256=digest(executable.read_bytes()),
            runtime=capture(adapter["runtime_command"]).decode(errors="replace"))
    plan = schedule(manifest, adapters, args.samples, args.warmups, args.seed)
    report = {"schema_version": 1, "recorded_unix_ns": time.time_ns(), "command": sys.argv,
              "runner_sha256": digest(Path(__file__).read_bytes()), "manifest": manifest,
              "manifest_file_sha256": digest(manifest_bytes), "adapters_file_sha256": digest(config_bytes),
              "seed": args.seed, "samples": args.samples, "warmups": args.warmups,
              "host_before": host_metadata(args.machine_class), "adapters": metadata, "schedule": plan,
              "unsupported_cases": ["true OS-cache-cold startup without controlled host reset", "live-model coding quality and total billed spend not measured by deterministic replay", "subscriber overflow/backpressure not measured by drain-after-terminal fixture"],
              "interpretation": "No winner or numeric improvement targets. Compare only same tier, semantics, boundary, model, filesystem, host and build profile. Unknown spend is not zero.",
              "attempts": []}
    save(output / "campaign.json", report)
    index = {a["id"]: a for a in adapters}
    for number, entry in enumerate(plan):
        attempt = attempt_run(entry, index[entry["adapter"]], manifest, output / f"{number:06}", args.timeout_seconds)
        report["attempts"].append(attempt)
    report["source_after"] = {a["id"]: source_identity(a["source"]["root"]) for a in adapters}
    report["source_unchanged"] = all(report["source_after"][key] == value["observed_source"] for key, value in metadata.items())
    report["executable_unchanged"] = all(digest(Path(a["command"][0]).read_bytes()) == metadata[a["id"]]["executable_sha256"] for a in adapters)
    report["host_after"] = host_metadata(args.machine_class)
    report["summaries"] = summarize(report["attempts"])
    report["paired_observations"] = paired_observations(report["attempts"])
    report["spend_accounting"] = spend_accounting(report["attempts"])
    report["failures"] = sum(bool(a["error"]) for a in report["attempts"])
    report["unsupported"] = [dict(tier=a["tier"], workload=a["workload"], adapter=a["adapter"], reason=a["receipt"]["unsupported_reason"])
                               for a in report["attempts"] if not a["error"] and a.get("receipt", {}).get("status") == "unsupported"]
    report["complete"] = report["failures"] == 0 and report["source_unchanged"] and report["executable_unchanged"]
    save(output / "report.json", report)
    print(f"{output / 'report.json'}: {len(plan)} attempts, {report['failures']} failures, {len(report['unsupported'])} unsupported; no winner claimed")
    return 0 if report["complete"] else 1


def main():
    parser = argparse.ArgumentParser(description="Seeded equal-semantics paired baseline with retained A/A controls")
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--adapters", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--samples", type=int, default=5)
    parser.add_argument("--warmups", type=int, default=1)
    parser.add_argument("--seed", type=int, default=42)
    parser.add_argument("--timeout-seconds", type=float, default=60)
    parser.add_argument("--machine-class", required=True)
    args = parser.parse_args()
    try:
        return run(args)
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        print(str(error), file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
