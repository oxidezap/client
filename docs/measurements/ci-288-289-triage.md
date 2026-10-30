# CI failures shared by the profiling PRs

Checkpoint for #288 and #289, against main `33108ce`, core `af2e018c`.
These are baseline issues, independent of the SQL and phone-metadata patches.
No production dependencies, fuel limits, test deadlines or assertions change.

## Web compilation failure

The [#288 web job](https://github.com/oxidezap/client/actions/runs/40998832174/job/123878204949)
and [#289 web job](https://github.com/oxidezap/client/actions/runs/41000719222/job/123884222100)
fail before launching a browser. Both report the same errors:

```text
error[E0433]: cannot find `session` in the crate root
  avatar/web.rs:36: crate::session::media::decode_avatar(&bytes)
error[E0425]: cannot find type `DecodePriority` in module `super`
  avatar/web.rs:32: _priority: super::DecodePriority
error[E0433]: cannot find module or crate `gpui` in this scope
  avatar/web.rs:34: Arc<gpui::RenderImage>
```

`daemon::media::avatar_web_tests` imports the actual GUI storage source by
path so it can exercise Cache Storage and Web Locks without linking GPUI.
#282 added a GUI decode function to that file. The daemon has neither those
GUI modules/types nor a GPUI dependency. Main already has both the import
and the GUI function; neither profiling PR changes them. A clean baseline
compilation reproduced all three errors with the same library-test target:

```sh
RUSTFLAGS='--cfg web_sys_unstable_apis' \
  cargo test -p oxidezap-daemon --lib --target wasm32-unknown-unknown --no-run
```

The fix moves only the GUI decode function into `avatar/web_platform.rs`.
The GUI selects that adapter as `imp`; it reexports the existing `web`
storage implementation. The daemon still imports `web.rs`, unchanged at its
call site. Storage implementation bytes are identical after removing the
GUI function. No browser tests, data-isolation checks or storage logic are
removed or stubbed, and the daemon gains no GUI dependency.

## Native fuel test flake

The [#288 Check job](https://github.com/oxidezap/client/actions/runs/40998832174/job/123878205132)
passed workspace Clippy and then failed at:

```text
tests::a_plugin_that_loops_forever_runs_out_of_fuel_and_is_stopped
crates/plugin-host/src/tests.rs:110:13:
timed out waiting for the plugin to be stopped
```

Nextest reported failure after 11.004 seconds. The adjacent self-waking
plugin test took 21.454 seconds. The failed fixture is a bare infinite
branch, draining the real 50,000,000-unit per-call fuel budget one branch
at a time in the debug interpreter. Its publication poll has a ten-second
wall-clock deadline. Both plugin-host sources and its dependency graph are
unchanged by #288; chat-store is not a plugin-host dependency.

The unchanged fixture passed isolated on #288 in 8.44 seconds and on main
in 8.47 seconds. Pinning the main test and one synthetic busy Python process
to the same CPU reproduced the exact timeout; the process finished after
16.75 seconds. The competing process was terminated in a `finally` block.
This establishes a load-sensitive baseline flake, not a SQL regression.

Reproduction uses the test executable reported by Cargo:

```sh
cargo test -p oxidezap-plugin-host --all-features --lib \
  tests::a_plugin_that_loops_forever_runs_out_of_fuel_and_is_stopped -- --exact
# To reproduce contention, pin this executable and one CPU-busy process
# to the same allowed CPU; always terminate that process after the test.
```

The fixture now inserts 64 constant/drop pairs inside the same infinite
loop. Wasmi 2.0 meters each constant operator before folding the pair away;
`engine/translator/func/fuel.rs` and `engine/costs.rs` in the locked wasmi
source define this behavior. The loop consumes the same production fuel
budget with fewer interpreter dispatches. Its unconditional branch,
ten-second deadline, stopped-state assertion and fuel-error assertion all
remain. A separate bounded 32-unit probe retains the original bare-branch
case and asserts the precise OutOfFuel trap, so adding other billed work
cannot hide a free-branch regression. No runtime sandbox setting or budget changes.

With the same CPU contention, the adjusted fixture passed five consecutive
runs in 0.60–0.68 seconds. Isolated, it passed in 0.25 seconds. The full
plugin-host suite passed, 136 tests and one existing ignored test. Clippy
for all plugin-host targets/features passed with warnings denied.

## Browser validation

The existing daemon and session library tests were built and run with the
CI flags, stable Rust 1.99.1 and wasm-bindgen-test-runner 0.2.129. A matching
Chromium/ChromeDriver 156.0.8078.73 pair was unpacked into `/tmp` for the
synthetic browser runs; no installed browser or user profile was modified.

```sh
RUSTFLAGS='--cfg web_sys_unstable_apis' \
CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner \
  cargo test -p oxidezap-daemon -p oxidezap-session --lib \
    --target wasm32-unknown-unknown
```

All 16 daemon tests and 32 session tests passed, including the real avatar
Cache Storage recovery/account-isolation tests. Existing unrelated WASM
warnings remain. The production GUI also needs its normal Pages build;
Windows, macOS and the remaining CI jobs remain the PR's checks. No merge
is performed as part of this investigation.
