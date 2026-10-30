#!/usr/bin/env python3
"""Synthetic same-runner framing comparison. No account data or network access.

Run: python3 crates/ipc/benches/framing_memory.py
Requires cargo and cached serde_json dependencies. The owned path retains the
same full-frame String allocation as main; the callback path parses in place.
Timing and allocator instrumentation use separate builds. Output is JSON.
"""
import json
import os
from pathlib import Path
import statistics
import resource
import subprocess
import tempfile

SOURCE = r'''
use oxidezap_ipc::{framing, DaemonMessage};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::hint::black_box;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static CALLS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);
struct Counting;
fn add(n: usize) {
    if cfg!(feature = "count") {
        CALLS.fetch_add(1, Relaxed);
        BYTES.fetch_add(n, Relaxed);
        PEAK.fetch_max(LIVE.fetch_add(n, Relaxed) + n, Relaxed);
    }
}
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() { add(l.size()); }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        if cfg!(feature = "count") { LIVE.fetch_sub(l.size(), Relaxed); }
        unsafe { System.dealloc(p, l); }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        let p = unsafe { System.realloc(p, l, n) };
        if !p.is_null() && cfg!(feature = "count") {
            LIVE.fetch_sub(l.size(), Relaxed);
            add(n);
        }
        p
    }
}
#[global_allocator]
static ALLOC: Counting = Counting;
fn parse(line: &str) -> Result<DaemonMessage, serde_json::Error> {
    serde_json::from_str(line.trim_end())
}
fn main() {
    let args: Vec<_> = std::env::args().collect();
    let borrowed = args[1] == "borrowed";
    let size: usize = args[2].parse().unwrap();
    let fragment: usize = args[3].parse().unwrap();
    let iterations: usize = args[4].parse().unwrap();
    let warm = args[5] == "warm";
    let malformed = args[6] == "malformed";
    let message = DaemonMessage::Messages {
        id: 1,
        jid: "synthetic@s.whatsapp.net".into(),
        messages: vec![oxidezap_core::ChatMessage::new_incoming(
            "synthetic-id".into(), "synthetic@s.whatsapp.net".into(), "x".repeat(size),
        )],
        next: None,
    };
    let mut frame = serde_json::to_vec(&message).unwrap();
    frame.push(b'\n');
    drop(message);
    if malformed { frame[0] = b'!'; }
    let mut reader = std::io::BufReader::with_capacity(fragment, frame.as_slice());
    let mut buf = Vec::new();
    // Warm both reader and connection buffer without a retained decoded value.
    if warm { let _ = framing::read_frame(&mut reader, &mut buf, frame.len()); }
    let start_live = LIVE.load(Relaxed);
    PEAK.store(start_live, Relaxed);
    CALLS.store(0, Relaxed);
    BYTES.store(0, Relaxed);
    let start = std::time::Instant::now();
    for _ in 0..iterations {
        *reader.get_mut() = frame.as_slice();
        if !warm { buf = Vec::new(); }
        let malformed_result = if borrowed {
            match framing::read_frame_with(&mut reader, &mut buf, frame.len(), |line| black_box(parse(black_box(line))).is_err()).unwrap() {
                Some(framing::FrameRead::Line(value)) => value,
                other => panic!("unexpected outcome {other:?}"),
            }
        } else {
            match framing::read_frame(&mut reader, &mut buf, frame.len()).unwrap() {
                Some(framing::FrameRead::Line(line)) => black_box(parse(black_box(&line))).is_err(),
                other => panic!("unexpected outcome {other:?}"),
            }
        };
        assert_eq!(black_box(malformed_result), malformed);
    }
    let ns = start.elapsed().as_nanos();
    let calls = CALLS.load(Relaxed);
    let allocated = BYTES.load(Relaxed);
    let peak = PEAK.load(Relaxed) - start_live;
    println!("{{\"ns\":{ns},\"calls\":{calls},\"allocated\":{allocated},\"peak_extra\":{peak},\"capacity\":{}}}", buf.capacity());
}
'''


def main():
    workspace = Path(__file__).resolve().parents[3]
    with tempfile.TemporaryDirectory(prefix="ipc-framing-") as directory:
        root = Path(directory)
        (root / "src").mkdir()
        (root / "Cargo.lock").write_text((workspace / "Cargo.lock").read_text())
        (root / "Cargo.toml").write_text('[package]\nname="framing-profile"\nversion="0.0.0"\nedition="2024"\n[features]\ncount=[]\n[dependencies]\nserde_json="=1.0.154"\n' + f'oxidezap-ipc={{path="{workspace / "crates/ipc"}"}}\noxidezap-core={{path="{workspace / "crates/core"}"}}\n')
        manifest = root / "Cargo.toml"
        with manifest.open("a") as output:
            output.write(f'\n[patch.crates-io]\nphonenumber={{path="{workspace / "vendor/phonenumber"}"}}\n')
        (root / "src/main.rs").write_text(SOURCE)
        cargo = os.environ.get("CARGO", "cargo")
        results = {}
        cases = [("small", 64, 8192, 1000000, "warm", "valid"),
                 ("large", 262144, 8192, 250, "warm", "valid"),
                 ("very_large", 4194304, 8192, 16, "warm", "valid"),
                 ("fragmented", 262144, 31, 100, "warm", "valid"),
                 ("bytewise", 4096, 1, 500, "warm", "valid"),
                 ("cold_large", 262144, 8192, 250, "cold", "valid"),
                 ("malformed", 262144, 8192, 250, "warm", "malformed")]
        for counted in [False, True]:
            command = [cargo, "build", "--offline", "--release", "--manifest-path", str(root / "Cargo.toml")]
            if counted:
                command += ["--features", "count"]
            subprocess.run(command, check=True)
            binary = Path(os.environ.get("CARGO_TARGET_DIR", root / "target")) / "release/framing-profile"
            for name, size, fragment, iterations, warmth, valid in cases:
                entries = {mode: [] for mode in ["owned", "borrowed"]}
                for repeat in range(1 if counted else 11):
                    for mode in (["owned", "borrowed"] if repeat % 2 == 0 else ["borrowed", "owned"]):
                        before = resource.getrusage(resource.RUSAGE_CHILDREN)
                        value = json.loads(subprocess.check_output([str(binary), mode, str(size), str(fragment), str(iterations), warmth, valid]))
                        after = resource.getrusage(resource.RUSAGE_CHILDREN)
                        value["cpu_ns"] = round((after.ru_utime + after.ru_stime - before.ru_utime - before.ru_stime) * 1e9)
                        entries[mode].append(value)
                result = results.setdefault(name, {"iterations": iterations})
                for mode, runs in entries.items():
                    if counted:
                        result[mode].update({k: v for k, v in runs[0].items() if k not in ("ns", "cpu_ns")})
                    else:
                        result[mode] = {"median_ns": statistics.median(r["ns"] for r in runs), "samples_ns": [r["ns"] for r in runs], "median_cpu_ns": statistics.median(r["cpu_ns"] for r in runs), "samples_cpu_ns": [r["cpu_ns"] for r in runs]}
        print(json.dumps(results, indent=2))


if __name__ == "__main__":
    main()
