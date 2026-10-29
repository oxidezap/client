#!/usr/bin/env python3
"""Run paired Linux release attention workloads in fresh processes.

CPU includes process startup. elapsed_ns covers only the asserted state workload.
RSS/PSS are sampled after the workload. Neither they nor peak RSS measure live
heap or distinguish allocator retention. No allocation instrumentation is used.
"""

import argparse
import json
import os
from pathlib import Path
import re
import statistics
import subprocess
import tempfile
import time


def sample(launcher, binary, count, mute_every):
    env = dict(os.environ, ATTENTION_CHATS=str(count), ATTENTION_MUTE_EVERY=str(mute_every))
    with tempfile.TemporaryFile(mode="w+") as output:
        started = time.perf_counter_ns()
        process = subprocess.Popen(
            [launcher, binary, "--ignored", "--exact", "state::store::tests::attention_workload", "--nocapture"],
            env=env, stdout=output, stderr=subprocess.STDOUT,
        )
        process.wait()
        wall_ns = time.perf_counter_ns() - started
        output.seek(0)
        text = output.read()
        if process.returncode:
            raise RuntimeError(text)
    match = re.search(r"elapsed_ns=(\d+)", text)
    if not match:
        raise RuntimeError(f"Workload did not run: {text}")
    memory = {key: int(value) for key, value in re.findall(
        r"attention memory (Rss|Pss|Private_Dirty):\s*(\d+)", text,
    )}
    child = re.search(r"attention child cpu_s=([0-9.]+) peak_rss_kib=(\d+)", text)
    if not child:
        raise RuntimeError(f"Missing child resource usage: {text}")
    return dict(
        elapsed_ns=int(match[1]), process_wall_ns=wall_ns,
        cpu_s=float(child[1]), peak_rss_kib=int(child[2]),
        **memory,
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base", type=Path)
    parser.add_argument("head", type=Path)
    parser.add_argument("--output", type=Path, default=Path("attention-results.json"))
    parser.add_argument("--samples", type=int, default=9)
    args = parser.parse_args()
    if args.samples < 1:
        parser.error("--samples must be positive")
    binaries = [("base", str(args.base.resolve())), ("head", str(args.head.resolve()))]
    launcher_dir = tempfile.TemporaryDirectory(prefix="attention-launcher-")
    launcher = str(Path(launcher_dir.name) / "child")
    subprocess.run(["cc", "-O2", "-Wall", "-Wextra", "-Werror",
                    str(Path(__file__).with_suffix(".c")), "-o", launcher], check=True)
    results = []
    for count in [100, 1000, 10000]:
        for mute_every in [0, 2, 1]:
            for iteration in range(args.samples + 1):
                order = binaries if iteration % 2 == 0 else binaries[::-1]
                for label, binary in order:
                    row = dict(label=label, count=count, mute_every=mute_every,
                               iteration=iteration, **sample(launcher, binary, count, mute_every))
                    if iteration:  # One warmup pair per scenario.
                        results.append(row)
            subset = [row for row in results if row["count"] == count
                      and row["mute_every"] == mute_every]
            metrics = ["elapsed_ns", "cpu_s", "peak_rss_kib", "Rss", "Pss", "Private_Dirty"]
            summary = {
                label: {key: statistics.median(row[key] for row in subset if row["label"] == label)
                        for key in metrics}
                for label, _ in binaries
            }
            print(json.dumps(dict(count=count, mute_every=mute_every, medians=summary)), flush=True)
            args.output.write_text(json.dumps(results, indent=2) + "\n")


if __name__ == "__main__":
    main()
