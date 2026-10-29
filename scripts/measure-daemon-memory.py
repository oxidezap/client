#!/usr/bin/env python3
"""Run immutable release test executables in fresh, unprivileged Linux processes."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import math
import signal
import statistics
import subprocess
import tempfile
import time


def wait_for_workload(process, timeout):
    def expired(_signal, _frame):
        raise TimeoutError(f"Workload exceeded {timeout:g} seconds")

    previous = signal.signal(signal.SIGALRM, expired)
    signal.setitimer(signal.ITIMER_REAL, timeout)
    try:
        _, status, usage = os.wait4(process.pid, 0)
        process.returncode = os.waitstatus_to_exitcode(status)
        return usage
    except BaseException:
        # Prevent a stalled workload or an interrupted driver from leaving a child.
        signal.setitimer(signal.ITIMER_REAL, 0)
        if process.returncode is None:
            try:
                process.kill()
            except ProcessLookupError:
                pass
            _, status, _ = os.wait4(process.pid, 0)
            process.returncode = os.waitstatus_to_exitcode(status)
        raise
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, previous)


def run(binary, scenario, mode, timeout):
    env = os.environ.copy()
    env.pop("OXIDEZAP_MEMORY_REPORT", None)
    if mode == "memory":
        env["OXIDEZAP_MEMORY_REPORT"] = "1"
    command = [str(binary), "--exact",
               f"session_bridge::memory_scenario::account_scale_memory_{scenario}",
               "--ignored", "--nocapture", "--test-threads=1"]
    with tempfile.TemporaryFile(mode="w+") as log:
        started = time.monotonic()
        process = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT, env=env)
        usage = wait_for_workload(process, timeout)
        elapsed = time.monotonic() - started
        log.seek(0)
        output = log.read()
    if process.returncode != 0:
        raise RuntimeError(f"{command} failed:\n{output}")
    if "1 passed; 0 failed" not in output:
        raise RuntimeError(f"Workload was not executed exactly once:\n{output}")
    stages = []
    for line in output.splitlines():
        marker = "OXIDEZAP_MEMORY "
        if marker in line:
            stages.append(json.loads(line.split(marker, 1)[1]))
    expected = ["startup", "initialized", "attach", "sync_queued", "drained",
                "detached", "queried", "released"]
    if mode == "memory" and [s["stage"] for s in stages] != expected:
        raise RuntimeError("Missing memory checkpoints; use the instrumented test binary")
    if mode == "timing" and (stages or "OXIDEZAP_ALLOCATOR counting" in output):
        raise RuntimeError("Timing requires a binary built without memory-profiling")
    return {"wall_s": elapsed, "cpu_s": usage.ru_utime + usage.ru_stime,
            "user_s": usage.ru_utime, "system_s": usage.ru_stime,
            "maxrss_kib": usage.ru_maxrss, "stages": stages}


def distribution(values):
    return {"median": statistics.median(values), "min": min(values), "max": max(values)}


def summarize(records):
    result = {}
    for scenario in sorted({r["scenario"] for r in records}):
        result[scenario] = {}
        for variant in sorted({r["variant"] for r in records if r["scenario"] == scenario}):
            selected = [r for r in records if r["scenario"] == scenario and r["variant"] == variant]
            data = {key: distribution([r[key] for r in selected])
                    for key in ["wall_s", "cpu_s", "maxrss_kib"]}
            data["stages"] = {}
            if selected[0]["stages"]:
                for index, first in enumerate(selected[0]["stages"]):
                    samples = [r["stages"][index] for r in selected]
                    stage = {key: distribution([s[key] for s in samples])
                             for key in ["live_bytes", "peak_bytes", "allocations", "requested_bytes"]}
                    stage["process_kib"] = {
                        key: distribution([s["process_kib"][key] for s in samples])
                        for key in first["process_kib"]
                    }
                    data["stages"][first["stage"]] = stage
            result[scenario][variant] = data
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", required=True, type=Path)
    parser.add_argument("--head", type=Path)
    parser.add_argument("--mode", choices=["memory", "timing"], required=True)
    parser.add_argument("--repeats", type=int, default=7)
    parser.add_argument("--warmups", type=int, default=1)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--timeout", type=float, default=120, help="Seconds per workload process")
    args = parser.parse_args()
    if platform.system() != "Linux" or args.repeats < 1 or args.warmups < 0:
        parser.error("Requires Linux, positive repeats and nonnegative warmups")
    if not math.isfinite(args.timeout) or args.timeout <= 0:
        parser.error("--timeout must be finite and positive")
    binaries = {"base": args.base.resolve(strict=True)}
    if args.head:
        binaries["head"] = args.head.resolve(strict=True)
    output = args.output.resolve()
    if any(output == binary or (output.exists() and output.samefile(binary))
           for binary in binaries.values()):
        parser.error("--output must not refer to an input binary")
    scenarios = ["headless", "attached", "read", "groups"]
    records = []
    failure = None
    try:
        for scenario in scenarios:
            for _ in range(args.warmups):
                for binary in binaries.values():
                    run(binary, scenario, args.mode, args.timeout)
            for repeat in range(args.repeats):
                order = list(binaries)
                if repeat % 2:
                    order.reverse()
                for variant in order:
                    sample = run(binaries[variant], scenario, args.mode, args.timeout)
                    records.append({"scenario": scenario, "variant": variant, "repeat": repeat, **sample})
    except (Exception, KeyboardInterrupt) as error:
        failure = f"{type(error).__name__}: {error}"
    model = next((line.split(":", 1)[1].strip() for line in
                  Path("/proc/cpuinfo").read_text().splitlines() if line.startswith("model name")), "unknown")
    result = {"mode": args.mode, "repeats": args.repeats, "warmups": args.warmups,
              "timeout_s": args.timeout, "complete": failure is None, "error": failure,
              "runner": {"machine": platform.machine(), "kernel": platform.release(),
                         "cpu_model": model, "affinity": sorted(os.sched_getaffinity(0))},
              "binaries": {name: {"path": str(path), "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
                           for name, path in binaries.items()},
              "summary": summarize(records), "runs": records}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(f"Wrote {len(records)} isolated runs to {args.output}")
    if failure is not None:
        parser.exit(1, failure + "\n")


if __name__ == "__main__":
    main()
