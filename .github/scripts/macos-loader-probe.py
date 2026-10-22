"""Identify loader inputs for a fixed, credential-free macOS diagnostic command."""
import json
import subprocess
import tempfile
from pathlib import Path

with tempfile.TemporaryDirectory(prefix="qq-loader-probe-") as directory:
    base = ["/bin", "/usr", "/System/Library", "/System/Cryptexes/OS", directory]
    candidates = [
        ("baseline", []),
        ("root-directory", []),
        ("app-cryptex", ["/System/Cryptexes/App"]),
        ("preboot-cryptexes", ["/System/Volumes/Preboot/Cryptexes"]),
        ("preboot", ["/System/Volumes/Preboot"]),
        ("system", ["/System"]),
        ("boot-database", ["/private/var/db"]),
    ]
    for label, extra in candidates:
        profile = "(version 1)(deny default)(allow process-exec process-fork)(allow sysctl-read)"
        if label == "root-directory":
            profile += '(allow file-read-data (require-all (literal "/") (vnode-type DIRECTORY)))'
        roots = set()
        for root in base + extra:
            roots.add(root)
            roots.add(str(Path(root).resolve()))
        for root in roots:
            profile += "(allow file-read* file-map-executable (subpath " + json.dumps(root) + "))"
            for parent in Path(root).parents:
                profile += "(allow file-read-metadata (literal " + json.dumps(str(parent)) + "))"
        try:
            result = subprocess.run(
                ["/usr/bin/sandbox-exec", "-p", profile, "/bin/sh", "-c", "printf loader-ready"],
                cwd=directory,
                env={"PATH": "/usr/bin:/bin", "HOME": directory},
                capture_output=True, text=True, timeout=5,
            )
            print(label, result.returncode, repr(result.stdout), repr(result.stderr), flush=True)
        except subprocess.TimeoutExpired:
            print(label, "timed out", flush=True)
