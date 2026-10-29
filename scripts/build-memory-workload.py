#!/usr/bin/env python3
"""Build and preserve an immutable release test binary for memory comparisons."""

import argparse
import json
from pathlib import Path
import shutil
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--instrument", action="store_true")
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    destination = args.output.resolve()
    if destination.exists():
        parser.error(f"Refusing to overwrite existing measurement binary: {destination}")
    command = ["cargo", "test", "-p", "oxidezap-daemon", "--release", "--lib",
               "--no-run", "--message-format=json"]
    if args.instrument:
        command += ["--features", "memory-profiling"]
    built = subprocess.run(command, stdout=subprocess.PIPE, text=True, check=True)
    artifacts = [json.loads(line) for line in built.stdout.splitlines() if line.startswith("{")]
    executables = [a["executable"] for a in artifacts
                   if a.get("reason") == "compiler-artifact" and a.get("executable")
                   and a.get("target", {}).get("name") == "oxidezap_daemon"
                   and a.get("profile", {}).get("test")]
    if len(executables) != 1:
        raise RuntimeError(f"Expected one daemon test binary, got {executables}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(executables[0], destination)
    metadata = {"command": command, "instrumented": args.instrument,
                "revision": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
                "worktree": subprocess.check_output(["git", "status", "--porcelain"], text=True),
                "compiler": subprocess.check_output(["rustc", "-vV"], text=True),
                "cargo": subprocess.check_output(["cargo", "--version"], text=True).strip()}
    destination.with_suffix(destination.suffix + ".build.json").write_text(json.dumps(metadata, indent=2) + "\n")
    print(destination)


if __name__ == "__main__":
    main()
