#!/usr/bin/env python3
"""Assert read-only CLI commands do not create DiskDrift state.

Usage: scripts/read_only_audit.py --config PATH --db PATH [--binary PATH]
"""

import argparse
import hashlib
import os
import pathlib
import subprocess
import sys


def state(path: pathlib.Path) -> dict[pathlib.Path, tuple[int, int, str]]:
    if not path.exists():
        return {}
    result = {}
    for entry in path.rglob("*"):
        stat = entry.stat()
        digest = ""
        if entry.is_file():
            digest = hashlib.sha256(entry.read_bytes()).hexdigest()
        result[entry.relative_to(path)] = (stat.st_mode, stat.st_size, digest)
    return result


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--config", required=True, type=pathlib.Path)
    parser.add_argument("--db", required=True, type=pathlib.Path)
    parser.add_argument("--binary", default="target/debug/diskdrift", type=pathlib.Path)
    args = parser.parse_args()
    args.config = args.config.resolve()
    args.db = args.db.resolve()
    args.binary = args.binary.resolve()

    root = args.db.parent
    root.mkdir(parents=True, exist_ok=True)
    args.config.parent.mkdir(parents=True, exist_ok=True)
    home = root / "home"
    home.mkdir(parents=True, exist_ok=True)
    if not args.config.exists():
        args.config.write_text('{"schemaVersion":1}', encoding="utf-8")
    before = state(root)
    env = os.environ.copy()
    env["HOME"] = str(home)
    env["USERPROFILE"] = str(home)
    for command in (["info"], ["usage"], ["doctor", "--quick", "--budget", "1"]):
        completed = subprocess.run(
            [str(args.binary), "--config", str(args.config), "--db", str(args.db), *command],
            cwd=pathlib.Path(__file__).resolve().parents[1],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        if completed.returncode != 0:
            sys.stderr.write(completed.stderr)
            return completed.returncode
    after = state(root)
    changed = sorted(str(path) for path in set(before) | set(after) if before.get(path) != after.get(path))
    if changed:
        sys.stderr.write("read-only commands changed state: " + ", ".join(changed) + "\n")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
