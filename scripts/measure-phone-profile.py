#!/usr/bin/env python3
"""Alternate fresh base/head probes; keep heap and counter-disabled runs separate."""
import argparse
import csv
import re
import statistics
import subprocess
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("launcher", type=Path)
parser.add_argument("base", type=Path)
parser.add_argument("head", type=Path)
parser.add_argument("--samples", type=int, default=9)
parser.add_argument("--output", type=Path, required=True)
args = parser.parse_args()
if args.samples < 1:
    parser.error("--samples must be positive")
rows = []
for mode in ["heap", "timing"]:
    for case in ["left", "format"]:
        for pair in range(-1, args.samples):  # Discard one warmup pair.
            for rev in (["base", "head"] if pair % 2 else ["head", "base"]):
                command = [str(args.launcher.resolve()), str(getattr(args, rev).resolve())]
                if case == "left":
                    command.append("left")
                if mode == "timing":
                    command.append("--timing")
                process = subprocess.run(command, text=True, capture_output=True, check=True)
                values = {k: int(v) for k, v in re.findall(r"(\w+)=(\d+)", process.stderr)}
                for line in process.stdout.splitlines():
                    tag = line.split()[0]
                    values.update({tag + "_" + k: int(v) for k, v in re.findall(r"(\w+)=(\d+)", line)})
                if values.get("cold_n") != 1 or values.get("warm_n") != 10000:
                    raise RuntimeError(f"Unexpected probe output: {process.stdout}")
                if pair >= 0:
                    rows.append(dict(mode=mode, case=case, pair=pair, rev=rev, **values))
with args.output.open("w", newline="") as output:
    writer = csv.DictWriter(output, fieldnames=rows[0].keys(), lineterminator="\n")
    writer.writeheader()
    writer.writerows(rows)
for mode in ["heap", "timing"]:
    for case in ["left", "format"]:
        for rev in ["base", "head"]:
            selected = [r for r in rows if (r["mode"], r["case"], r["rev"]) == (mode, case, rev)]
            medians = {k: statistics.median(r[k] for r in selected) for k in selected[0] if k not in ["mode", "case", "pair", "rev"]}
            print(mode, case, rev, medians)
