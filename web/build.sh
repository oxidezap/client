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

# `${TRUNK_ACTION:-build}` so `TRUNK_ACTION=serve ./build.sh` runs the dev
# server through exactly the same environment the published bundle is built
# with — a serve that differs from the build is a difference nobody sees
# until deploy.
exec trunk "${TRUNK_ACTION:-build}" ${TRUNK_PROFILE---release} \
    --public-url "$PUBLIC_URL"
