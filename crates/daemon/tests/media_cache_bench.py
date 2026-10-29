#!/usr/bin/env python3
"""Linux native algorithm benchmark, not a browser or whole-daemon benchmark.

Run with Python 3 and rustc on PATH:
  python3 crates/daemon/tests/media_cache_bench.py BASE_REF > results.json

Compiles the actual web cache module at BASE_REF and in the working tree with
rustc -O. Pure key predicates, wipe rules and the budget constant are extracted
from each tree. Dependency names are shims for those extracted definitions;
logging only constructs its format arguments. No Cargo manifests are changed.
Each row contains a pair in alternating execution order. CPU and peak RSS are
per child process; wall time covers repeated fill/clear cycles, including Vec
allocation, cache mutation and eviction. Payloads are synthetic 1 KiB entries.
This does not measure JS, wasm code generation, decoding, networking, live heap,
or the page's frame time. RSS is not a live-heap measurement.
"""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[3]
MEDIA = "crates/daemon/src/media/"


def read(ref, path):
    if ref is None:
        return (ROOT / path).read_text()
    return subprocess.check_output(["git", "show", f"{ref}:{path}"], cwd=ROOT, text=True)


def item(source, marker):
    start = source.index(marker)
    end = source.index("{", start) + 1
    depth = 1
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[start:end]


def definitions(ref):
    transport = read(ref, "crates/ipc/src/transport.rs")
    media = read(ref, MEDIA + "mod.rs")
    helpers = "\n".join(item(transport, marker) for marker in [
        "pub fn account_prefix_of", "pub fn key_local_name", "pub fn is_staged_key",
        "pub fn is_global_staged_key", "pub fn is_account_staged_key", "fn account_staged_local",
    ])
    helpers += "\n#[derive(Debug, Clone, Copy, PartialEq, Eq)]\n"
    helpers += "\n".join(item(media, marker) for marker in [
        "pub enum Wipe", "impl Wipe", "pub(super) fn is_disposable",
        "pub(super) fn is_staged_upload", "pub(super) fn is_in_progress",
    ])
    helpers = helpers.replace("pub(super)", "pub")
    budget = next(line for line in read(ref, "crates/core/src/media_budget.rs").splitlines()
                  if line.startswith("pub const WEB_MEDIA_BUDGET_BYTES:"))
    return r'''#![allow(dead_code)]
extern crate self as anyhow;
extern crate self as oxidezap_core;
extern crate self as oxidezap_ipc;
extern crate self as log;
pub type Result<T> = std::result::Result<T, String>;
#[macro_export] macro_rules! info {
    ($($arg:tt)*) => { std::hint::black_box(format_args!($($arg)*)); }
}
#[macro_export] macro_rules! warn {
    ($($arg:tt)*) => { std::hint::black_box(format_args!($($arg)*)); }
}
const STAGED_PREFIX: &str = "u-";
const IN_PROGRESS_PREFIX: &str = "w-";
''' + budget + "\n" + helpers


def build(ref, directory, name):
    module = directory / (name + "_module.rs")
    module.write_text(read(ref, MEDIA + "web.rs"))
    source = directory / (name + ".rs")
    source.write_text(definitions(ref) + f'\n#[path="{module}"] mod cache;\n' + r'''
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let count: usize = args[1].parse().unwrap();
    let rounds: usize = args[2].parse().unwrap();
    let keys: Vec<String> = (0..count).map(|i| format!("a1-d-{i:032x}")).collect();
    let start = std::time::Instant::now();
    for _ in 0..rounds {
        cache::delete_for("", Wipe::Everything).unwrap();
        for key in &keys {
            std::hint::black_box(cache::put_owned_evictable(key, vec![7; 1024]).unwrap());
        }
        assert_eq!(cache::cache_usage(), (1024 * count.min(4096) as u64, count.min(4096) as u64));
    }
    println!("{}", start.elapsed().as_nanos());
}
''')
    binary = directory / name
    subprocess.run(["rustc", "--edition=2024", "-C", "opt-level=3", str(source),
                    "-o", str(binary)], check=True)
    return binary


def sample(binary, count, rounds):
    proc = subprocess.Popen([str(binary), str(count), str(rounds)], stdout=subprocess.PIPE)
    _, status, usage = os.wait4(proc.pid, 0)
    proc.returncode = os.waitstatus_to_exitcode(status)
    if proc.returncode:
        raise RuntimeError(f"{binary.name} exited with {proc.returncode}")
    return {"wall_ns": int(proc.stdout.read()), "cpu_s": usage.ru_utime + usage.ru_stime,
            "maxrss_kib": usage.ru_maxrss}


if __name__ == "__main__":
    base = sys.argv[1]
    rows = []
    with tempfile.TemporaryDirectory(prefix="media-cache-") as scratch:
        binaries = {"base": build(base, Path(scratch), "base"),
                    "head": build(None, Path(scratch), "head")}
        for count, rounds in [(256, 1000), (1024, 80), (4096, 10), (4200, 10)]:
            for repeat in range(11):
                row = {"count": count, "rounds": rounds, "repeat": repeat}
                for name in (["base", "head"] if repeat % 2 == 0 else ["head", "base"]):
                    row[name] = sample(binaries[name], count, rounds)
                rows.append(row)
    print(json.dumps(rows, indent=2))
