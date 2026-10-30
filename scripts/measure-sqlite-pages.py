#!/usr/bin/env python3
"""Paired Linux release cursor workloads. Setup is excluded by the Rust test.

CPU ticks come from /proc/self/stat around the query loop, without allocator
instrumentation. This runner makes no RSS or live-heap claims.
"""
import argparse
import csv
import json
import os
from pathlib import Path
import re
import statistics
import subprocess
import sys


def sample(binary, case, direction, args):
    env = dict(os.environ, PAGING_ROWS=str(args.rows), PAGING_CASE=case,
               PAGING_DIRECTION=direction, PAGING_ITERATIONS=str(args.iterations))
    result = subprocess.run(
        [str(binary), "--ignored", "--exact", "queries::tests::cursor_seek_workload", "--nocapture"],
        env=env, text=True, capture_output=True, check=True,
    )
    match = re.search(r"elapsed_ns=(\d+) cpu_ticks=Some\((\d+)\)", result.stdout)
    if not match:
        raise RuntimeError(result.stdout + result.stderr)
    return dict(elapsed_ns=int(match[1]), cpu_ticks=int(match[2]),
                sqlite_version=re.search(r"sqlite_version=([0-9.]+)", result.stdout)[1])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base", type=Path)
    parser.add_argument("head", type=Path)
    parser.add_argument("--rows", type=int, default=200000)
    parser.add_argument("--iterations", type=int, default=256)
    parser.add_argument("--samples", type=int, default=7)
    parser.add_argument("--output", type=Path, default=Path("sqlite-pages.csv"))
    args = parser.parse_args()
    if sys.platform != "linux" or args.rows < 200 or min(args.iterations, args.samples) < 1:
        parser.error("requires Linux, at least 200 rows, and positive iterations/samples")
    binaries = [("base", args.base.resolve()), ("head", args.head.resolve())]
    results = []
    print(json.dumps(dict(cpu_ticks_per_second=os.sysconf("SC_CLK_TCK"))), flush=True)
    for case in ["head", "middle", "deep", "ties", "aliases"]:
        for direction in ["backward", "forward"]:
            for iteration in range(args.samples + 1):
                order = binaries if iteration % 2 == 0 else binaries[::-1]
                for label, binary in order:
                    row = dict(label=label, case=case, direction=direction,
                               iteration=iteration, rows=args.rows, queries=args.iterations,
                               **sample(binary, case, direction, args))
                    if iteration:
                        results.append(row)
            subset = [r for r in results if r["case"] == case and r["direction"] == direction]
            medians = {label: {key: statistics.median(r[key] for r in subset if r["label"] == label)
                               for key in ["elapsed_ns", "cpu_ticks"]}
                       for label, _ in binaries}
            print(json.dumps(dict(case=case, direction=direction, medians=medians)), flush=True)
            with args.output.open("w") as output:
                writer = csv.DictWriter(output, fieldnames=list(results[0]))
                writer.writeheader()
                writer.writerows(results)


if __name__ == "__main__":
    main()
