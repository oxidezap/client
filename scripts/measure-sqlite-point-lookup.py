#!/usr/bin/env python3
"""Paired pooled point lookup workloads on Linux release test binaries.

Use binaries built from the same harness and release flags on one runner.
Each process seeds synthetic data, warms its pool, then measures only reads. Contact includes present and absent rows; lid reaches the
nested mapping lookup through reactions_for; pn is an unchanged control.
"""
import argparse
import csv
import json
import os
from pathlib import Path
import re
import statistics
import subprocess


def sample(binary, case, allocations, iterations):
    env = dict(os.environ, POINT_CASE=case,
               POINT_ALLOC=str(allocations), POINT_ITERATIONS=str(iterations))
    result = subprocess.run(
        [str(binary), "--ignored", "--exact", "point_lookup_workload", "--nocapture"],
        env=env, text=True, capture_output=True, check=True,
    )
    # libtest can prefix this line with its progress text on a pinned CPU.
    match = re.search(r"\bPOINT (.+)$", result.stdout, re.MULTILINE)
    if not match:
        raise RuntimeError(result.stdout + result.stderr)
    return {key: int(value) for key, value in re.findall(r"(\w+)=(\d+)", match[1])}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base", type=Path)
    parser.add_argument("head", type=Path)
    parser.add_argument("--iterations", type=int, default=64)
    parser.add_argument("--samples", type=int, default=7)
    parser.add_argument("--cases", nargs="+", choices=["contact", "lid", "pn"],
                        default=["contact", "lid", "pn"])
    parser.add_argument("--modes", nargs="+", type=int, choices=[0, 1], default=[0, 1])
    parser.add_argument("--cpu", type=int, help="pin this runner and its children to an allowed Linux CPU")
    parser.add_argument("--output", type=Path, default=Path("sqlite-point-lookup.csv"))
    args = parser.parse_args()
    if min(args.iterations, args.samples) < 1:
        parser.error("iterations and samples must be positive")
    if args.cpu is not None:
        if args.cpu not in os.sched_getaffinity(0):
            parser.error("cpu is outside this process's allowed affinity")
        os.sched_setaffinity(0, {args.cpu})
    binaries = [("base", args.base.resolve()), ("head", args.head.resolve())]
    rows = []
    for case in args.cases:
        for allocations in args.modes:
            for pair in range(args.samples + 1):
                order = binaries if pair % 2 == 0 else binaries[::-1]
                observed = []
                for label, binary in order:
                    result = sample(binary, case, allocations, args.iterations)
                    observed.append(tuple(result[key] for key in ["rows", "checksum", "quotes", "reactions"]))
                    row = dict(label=label, case=case, allocations=allocations,
                               pair=pair, iterations=args.iterations, **result)
                    if pair:
                        rows.append(row)
                if observed[0] != observed[1]:
                    raise RuntimeError(f"base/head result mismatch: {case}, {observed}")
            print(json.dumps(dict(case=case, allocations=allocations, completed=True)), flush=True)
    with args.output.open("w", newline="") as output:
        writer = csv.DictWriter(output, fieldnames=list(rows[0]), lineterminator="\n")
        writer.writeheader()
        writer.writerows(rows)
    for case in args.cases:
        timed = [row for row in rows if row["case"] == case and row["allocations"] == 0]
        if not timed:
            continue
        base = {row["pair"]: row for row in timed if row["label"] == "base"}
        head = {row["pair"]: row for row in timed if row["label"] == "head"}
        print(json.dumps(dict(case=case, **{
            metric + "_paired_ratio_median": statistics.median(
                head[pair][metric] / base[pair][metric] for pair in base)
            for metric in ["elapsed_ns", "cpu_ns"]
        })), flush=True)


if __name__ == "__main__":
    main()
