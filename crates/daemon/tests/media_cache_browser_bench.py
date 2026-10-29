#!/usr/bin/env python3
"""Paired release wasm measurements in a real browser, with ordinary wasm memory.

Requires Python 3.11+, the wasm32 target, wasm-bindgen-test-runner matching Cargo.lock, and
matching browser/WebDriver binaries. Set CHROMEDRIVER and, if needed,
WASM_BINDGEN_TEST_WEBDRIVER_JSON as for the project's browser tests, then run:
  RUSTFLAGS='--cfg web_sys_unstable_apis' python3 crates/daemon/tests/media_cache_browser_bench.py BASE_REF

This uses the actual cache sources with the same pure dependency shims as the
native probe. It measures synthetic fill/clear cycles in one browser with
alternating execution order after warmup. It does not measure the whole page,
shared-memory production builds, networking, image decode, CPU time or heap.
The temporary crate reuses the normal Cargo target/cache; no workspace manifest
or production dependency changes are made.
"""

import os
from pathlib import Path
import subprocess
import sys
import tempfile
import tomllib

import media_cache_bench as native

PROBE = r'''
#[path="base_cache.rs"] mod base;
#[path="head_cache.rs"] mod head;
#[cfg(test)] mod probe {
 use super::*;
 use wasm_bindgen::prelude::*;
 use wasm_bindgen_test::*;
 wasm_bindgen_test_configure!(run_in_browser);
 #[wasm_bindgen(inline_js="export function cache_probe_now() { return performance.now(); } export function cache_probe_yield() { return new Promise(resolve => setTimeout(resolve, 0)); }")]
 extern "C" { fn cache_probe_now() -> f64; fn cache_probe_yield() -> js_sys::Promise; }
 fn sample(version: &str, keys: &[String], rounds: usize) -> f64 {
  let start=cache_probe_now();
  for _ in 0..rounds {
   match version {
    "base" => {
     base::delete_for("",Wipe::Everything).unwrap();
     for key in keys {std::hint::black_box(base::put_owned_evictable(key,vec![7;1024]).unwrap());}
     assert_eq!(base::cache_usage(),(1024*keys.len().min(4096)as u64,keys.len().min(4096)as u64));
    },
    _ => {
     head::delete_for("",Wipe::Everything).unwrap();
     for key in keys {std::hint::black_box(head::put_owned_evictable(key,vec![7;1024]).unwrap());}
     assert_eq!(head::cache_usage(),(1024*keys.len().min(4096)as u64,keys.len().min(4096)as u64));
    }
   }
  }
  (cache_probe_now()-start)/rounds as f64
 }
 #[wasm_bindgen_test]
 async fn paired_release_measurements() {
  for (count, rounds) in [(256,200),(1024,30),(4096,5),(4200,5)] {
   let keys:Vec<String>=(0..count).map(|i|format!("a1-d-{i:032x}")).collect();
   // Warm both implementations before collecting paired samples.
   sample("base",&keys,1); sample("head",&keys,1);
   for repeat in 0..11 {
    let (b,h)=if repeat%2==0 {(sample("base",&keys,rounds),sample("head",&keys,rounds))}
              else {let h=sample("head",&keys,rounds);(sample("base",&keys,rounds),h)};
    console_log!("MEDIA_PAIR count={} repeat={} rounds={} base_ms={} head_ms={}",count,repeat,rounds,b,h);
    // Yield outside the timed samples so WebDriver can collect progress.
    wasm_bindgen_futures::JsFuture::from(cache_probe_yield()).await.unwrap();
   }
  }
 }
}
'''


if __name__ == "__main__":
    base = sys.argv[1]
    definitions = native.definitions(base)
    if definitions != native.definitions(None):
        raise RuntimeError("Probe requires identical key predicates, wipe rules and budget constants")
    packages = tomllib.loads((native.ROOT / "Cargo.lock").read_text())["package"]
    versions = {p["name"]: p["version"] for p in packages}
    with tempfile.TemporaryDirectory(prefix="media-cache-browser-") as scratch:
        directory = Path(scratch)
        for name, ref in [("base", base), ("head", None)]:
            source = native.read(ref, native.MEDIA + "web.rs").split("#[cfg(test)]")[0]
            (directory / (name + "_cache.rs")).write_text(source)
        manifest = '[package]\nname = "media-cache-browser-bench"\nversion = "0.0.0"\nedition = "2024"\n'
        manifest += '[lib]\npath = "lib.rs"\ncrate-type = ["cdylib"]\n[dev-dependencies]\n'
        for name in ["wasm-bindgen", "wasm-bindgen-futures", "wasm-bindgen-test", "js-sys"]:
            manifest += f'{name} = "={versions[name]}"\n'
        (directory / "Cargo.toml").write_text(manifest)
        (directory / "lib.rs").write_text(definitions + "\n" + PROBE)
        env = os.environ.copy()
        env.setdefault("CARGO_TARGET_DIR", str(native.ROOT / "target"))
        env.setdefault("CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER", "wasm-bindgen-test-runner")
        env.setdefault("WASM_BINDGEN_TEST_TIMEOUT", "300")
        subprocess.run(["cargo", "test", "--manifest-path", str(directory / "Cargo.toml"),
                        "--target", "wasm32-unknown-unknown", "--release", "--", "--nocapture"],
                       env=env, check=True)
