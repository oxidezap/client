#!/usr/bin/env sh
# Build the web front end into `web/dist`.
#
# Three things are not defaults and all three are required:
#
#   nightly      `-Z build-std` is nightly-only, and gpui's own head uses
#                unstable library features besides.
#   build-std    the standard library has to be rebuilt with the atomics
#                target feature on; the prebuilt one is not, and linking
#                against it produces a module with no working threads.
#   public-url   a project page is served from a subdirectory, so the
#                generated glue has to be told where it is loading from.
#
# The link flags themselves are in /.cargo/config.toml, under the wasm target,
# so an ordinary `cargo build --target wasm32-unknown-unknown` gets them too.
set -eu

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$here"

: "${PUBLIC_URL:=/}"
: "${PROFILE:=--release}"

exec trunk build $PROFILE \
    --public-url "$PUBLIC_URL" \
    -- -Z build-std=std,panic_abort
