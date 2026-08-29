#!/usr/bin/env sh
# Build the web front end into `web/dist`.
#
# Three things are not defaults and all three are required:
#
#   nightly      `build-std` is nightly-only, and gpui's own head uses
#                unstable library features besides.
#   build-std    the standard library has to be rebuilt with the atomics
#                target feature on; the prebuilt one is not, and linking
#                against it produces a module with no working threads.
#   public-url   a project page is served from a subdirectory, so the
#                generated glue has to be told where it is loading from.
#
# The first two are passed as environment rather than as flags: trunk has no
# way to forward arguments to cargo, and `[unstable]` in a config file would
# apply to the native build too — which is meant to stay on stable. Cargo
# reads the same setting from `CARGO_UNSTABLE_BUILD_STD`, and that reaches
# only this process.
#
# The link flags themselves are in /.cargo/config.toml, under the wasm target,
# so an ordinary `cargo build --target wasm32-unknown-unknown` gets them too.
set -eu

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$here"

: "${PUBLIC_URL:=/}"

RUSTUP_TOOLCHAIN=${RUSTUP_TOOLCHAIN:-nightly}
CARGO_UNSTABLE_BUILD_STD=std,panic_abort
export RUSTUP_TOOLCHAIN CARGO_UNSTABLE_BUILD_STD

# What this build is *for*, which is the one thing here that is a choice.
#
#   release  (default)  the bundle a visitor downloads
#   debug               the same bundle with its diagnostics intact
#
# `WEB_PROFILE=debug ./build.sh` is what to reach for when a page is
# misbehaving: it builds the ordinary `release` profile with the standard
# library at its defaults, which is the tree the desktop is built from. Both
# are release builds in cargo's sense — an unoptimized gpui is unusable, which
# is why `[profile.dev.package.gpui]` exists at all.
WEB_PROFILE=${WEB_PROFILE:-release}

if [ "$WEB_PROFILE" = release ]; then
    # `[profile.web]` in the workspace manifest is where the per-crate
    # decisions and their measurements live. Selected here rather than being a
    # `cfg`, because cargo has no per-target profiles and the desktop build
    # must keep the one it was calibrated for.
    #
    # Through trunk's own flag: it is the only way in. `--config` on a cargo
    # command line does reach the per-package overrides, and trunk cannot
    # forward one; the `CARGO_PROFILE_RELEASE_PACKAGE_<NAME>_OPT_LEVEL`
    # environment form, which would have needed neither, is silently ignored
    # — measured, at the same byte for byte as not setting it.
    CARGO_PROFILE=web
    # The standard library, which `build-std` is already paying to rebuild and
    # was rebuilding at the defaults. `optimize_for_size` is what `opt-level =
    # "s"` is for `core` and `alloc`, which are 20% of the shipped code
    # section between them and are not reached by any profile setting here:
    # build-std compiles them as its own units.
    #
    # Its louder sibling `panic_immediate_abort` is *not* here, and not by
    # choice: it stopped being a build-std feature and became a panic strategy
    # (`panic = "immediate-abort"`), which cargo will only accept behind
    # `cargo-features` in the workspace manifest — a key stable cargo rejects
    # outright, so declaring it would move the desktop build to nightly cargo
    # to save bytes in the web one.
    CARGO_UNSTABLE_BUILD_STD_FEATURES=optimize_for_size
    export CARGO_UNSTABLE_BUILD_STD_FEATURES
else
    CARGO_PROFILE=release
fi

# `${TRUNK_ACTION:-build}` so `TRUNK_ACTION=serve ./build.sh` runs the dev
# server through exactly the same environment the published bundle is built
# with — a serve that differs from the build is a difference nobody sees
# until deploy.
exec trunk "${TRUNK_ACTION:-build}" ${TRUNK_PROFILE---release} \
    --cargo-profile "$CARGO_PROFILE" \
    --public-url "$PUBLIC_URL"
