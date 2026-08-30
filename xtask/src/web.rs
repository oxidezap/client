//! Build the web front end into `web/dist`.
//!
//! Three things are not defaults and all three are required:
//!
//!   nightly      `build-std` is nightly-only, and gpui's own head uses
//!                unstable library features besides.
//!   build-std    the standard library has to be rebuilt with the atomics
//!                target feature on; the prebuilt one is not, and linking
//!                against it produces a module with no working threads.
//!   public-url   a project page is served from a subdirectory, so the
//!                generated glue has to be told where it is loading from.
//!
//! The first two are passed as environment rather than as flags: trunk has no
//! way to forward arguments to cargo, and `[unstable]` in a config file would
//! apply to the native build too — which is meant to stay on stable. Cargo
//! reads the same setting from `CARGO_UNSTABLE_BUILD_STD`, and here it reaches
//! only the child rather than this process and everything after it.
//!
//! The link flags themselves are in /.cargo/config.toml, under the wasm
//! target, so an ordinary `cargo build --target wasm32-unknown-unknown` gets
//! them too.

use crate::util::{Result, Run, env_or, repo_root};

pub fn build() -> Result<()> {
    // What this build is *for*, which is the one thing here that is a choice.
    //
    //   release  (default)  the bundle a visitor downloads
    //   debug               the same bundle, with its symbols
    //
    // `WEB_PROFILE=debug cargo xtask web build` is what to reach for when a
    // page is misbehaving and a profile or a panic trace has to name
    // something. It is the *same* build — `[profile.web-debug]` inherits `web`
    // — with `strip` off, so the name section survives into the module and
    // DevTools has Rust functions to show rather than indices. A build you are
    // diagnosing should be the build that misbehaved.
    //
    // Both are release builds in cargo's sense; an unoptimized gpui is
    // unusable, which is why `[profile.dev.package.gpui]` exists at all.
    //
    // `[profile.web]` and `[profile.web-debug]` in the workspace manifest are
    // where the per-crate decisions and their measurements live. Selected here
    // rather than being a `cfg`, because cargo has no per-target profiles and
    // the desktop build must keep the one it was calibrated for.
    //
    // Through trunk's own flag, which is the only way in: `--config` on a
    // cargo command line does reach the per-package overrides and trunk cannot
    // forward one, while the `CARGO_PROFILE_RELEASE_PACKAGE_<NAME>_OPT_LEVEL`
    // environment form, which would have needed neither, is silently ignored —
    // measured, byte for byte the same as not setting it.
    let cargo_profile = match env_or("WEB_PROFILE", "release").as_str() {
        "debug" => "web-debug",
        _ => "web",
    };

    // `TRUNK_ACTION=serve` runs the dev server through exactly the same
    // environment the published bundle is built with — a serve that differs
    // from the build is a difference nobody sees until deploy.
    let action = env_or("TRUNK_ACTION", "build");

    // Overridable, and split on whitespace so more than one flag fits; set it
    // to a single space to pass none.
    let profile_flags: Vec<String> = std::env::var("TRUNK_PROFILE")
        .map(|v| v.split_whitespace().map(str::to_string).collect())
        .unwrap_or_else(|_| vec!["--release".to_string()]);

    Run::new("trunk")
        .arg(&action)
        .args(&profile_flags)
        .args(["--cargo-profile", cargo_profile])
        .args(["--public-url", &env_or("PUBLIC_URL", "/")])
        // Trunk finds `Trunk.toml` and `index.html` beside it.
        .current_dir(repo_root().join("web"))
        // Named rather than exported, so nothing else this process goes on to
        // do is silently a nightly build.
        .env("RUSTUP_TOOLCHAIN", env_or("RUSTUP_TOOLCHAIN", "nightly"))
        .env("CARGO_UNSTABLE_BUILD_STD", "std,panic_abort")
        // The standard library, which `build-std` is already paying to rebuild
        // and was rebuilding at the defaults. `optimize_for_size` is what
        // `opt-level = "s"` is for `core` and `alloc`, which are 20% of the
        // shipped code section between them and are reached by no profile
        // setting here: build-std compiles them as its own units.
        //
        // Its louder sibling `panic_immediate_abort` is *not* here, and not by
        // choice: it stopped being a build-std feature and became a panic
        // strategy (`panic = "immediate-abort"`), which cargo will only accept
        // behind `cargo-features` in the workspace manifest — a key stable
        // cargo rejects outright, so declaring it would move the desktop build
        // to nightly cargo to save bytes in the web one.
        //
        // Set for both profiles, because the diagnostic build differs from the
        // shipped one only in what it keeps: one that size-optimized its
        // standard library differently would not be the build being diagnosed.
        .env("CARGO_UNSTABLE_BUILD_STD_FEATURES", "optimize_for_size")
        .run()
}
