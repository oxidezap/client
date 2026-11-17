#!/usr/bin/env bash
# Repackage OxideZap's FHS tarball from package.sh as a single-file Flatpak bundle:
#
#   $DIST/oxidezap-<label>-linux-<arch>.flatpak
#
# BROKEN: the daemon's tray icon cannot register inside the sandbox, so once the window is
# closed nothing shows or controls the session left running (details and the fix in
# packaging/linux/flatpak/io.github.oxidezap.client.yml). Not part of the release; kept to work
# on the sandbox support.
#
# Usage: packaging/linux/flatpak-bundle.sh [--no-test] [TARBALL]
#
# TARBALL defaults to $DIST/oxidezap-<label>-linux-<arch>-fhs.tar.gz (package.sh --formats tar).
# The bundle is built for the host architecture (x86_64 or aarch64), which must match the
# tarball's. Needs flatpak and flatpak-builder; the freedesktop runtime and SDK named in the
# manifest are installed per-user from Flathub. Unless --no-test, the bundle is then installed
# per-user and `oxidezap-cli --help` is run inside the sandbox as a smoke test.
set -euo pipefail
umask 022
# shellcheck source=../env.sh
. "$(dirname "${BASH_SOURCE[0]}")/../env.sh"
HERE="$ROOT/packaging/linux"
APP_ID=io.github.oxidezap.client
FLATHUB=https://dl.flathub.org/repo/flathub.flatpakrepo

TEST=1
TARBALL=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --no-test) TEST=0; shift ;;
    -h | --help) sed -n '2,17p' "$0"; exit 0 ;;
    -*) echo "unknown argument: $1" >&2; exit 2 ;;
    *) TARBALL="$1"; shift ;;
  esac
done

ARCH="$(uname -m)"
case "$ARCH" in
  x86_64) ;;
  aarch64 | arm64) ARCH=aarch64 ;;
  *) echo "unsupported architecture $ARCH" >&2; exit 2 ;;
esac
BASENAME="oxidezap-$LABEL-linux-$ARCH"
TARBALL="${TARBALL:-$DIST/$BASENAME-fhs.tar.gz}"
[[ -f "$TARBALL" ]] || { echo "error: $TARBALL not found (run packaging/linux/package.sh --formats tar first)" >&2; exit 1; }
for tool in flatpak flatpak-builder; do
  command -v "$tool" >/dev/null || { echo "error: $tool not found (apt install flatpak flatpak-builder)" >&2; exit 1; }
done

warn "the Flatpak is known broken: the daemon's tray icon cannot register inside the sandbox"
echo "==> OxideZap $VERSION Flatpak bundle for $ARCH from $(basename "$TARBALL")"

WORK="$CARGO_TARGET_DIR/flatpak-bundle"
rm -rf "$WORK"
mkdir -p "$WORK/stage"
tar -xzf "$TARBALL" -C "$WORK/stage" --strip-components=1
for b in oxidezap oxidezapd oxidezap-cli; do
  [[ -x "$WORK/stage/bin/$b" ]] || { echo "error: $TARBALL has no bin/$b" >&2; exit 1; }
done
cp "$HERE/flatpak/$APP_ID.yml" "$WORK/$APP_ID.yml"

flatpak remote-add --user --if-not-exists flathub "$FLATHUB"
# --disable-rofiles-fuse: no FUSE in containers and CI runners.
flatpak-builder --user --install-deps-from=flathub --disable-rofiles-fuse --force-clean \
  --default-branch=stable --repo="$WORK/repo" --state-dir="$WORK/state" "$WORK/build" "$WORK/$APP_ID.yml"

OUT="$DIST/$BASENAME.flatpak"
# --runtime-repo lets `flatpak install` fetch the freedesktop runtime from Flathub if missing.
flatpak build-bundle --runtime-repo="$FLATHUB" "$WORK/repo" "$OUT" "$APP_ID" stable
echo "wrote $OUT"

if [[ "$TEST" = 1 ]]; then
  flatpak install --user -y --noninteractive --reinstall "$OUT"
  flatpak info --user "$APP_ID"
  flatpak run --command=oxidezap-cli "$APP_ID" --help >/dev/null
  flatpak run --command=sh "$APP_ID" -c 'test -x /app/bin/oxidezapd && ls /app/share/applications /app/share/metainfo /app/share/icons/hicolor/256x256/apps'
fi
echo "==> done"
ls -lh "$OUT"
