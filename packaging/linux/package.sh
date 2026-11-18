#!/usr/bin/env bash
# Build and package OxideZap for Linux (<arch> is x86_64 or aarch64, <label> the version as the
# release names it: `nightly`, a tag such as v0.2.0, or the Cargo.toml version locally):
#
#   $DIST/oxidezap-<label>-linux-<arch>.AppImage        any distro with glibc >= the build host's
#   $DIST/oxidezap-<label>-linux-<arch>.AppImage.zsync  delta updates (needs zsyncmake)
#   $DIST/oxidezap-<label>-linux-<arch>.deb             Debian, Ubuntu, Mint, Pop!_OS, ...
#   $DIST/oxidezap-<label>-linux-<arch>.rpm             Fedora, openSUSE, RHEL, ...
#   $DIST/oxidezap-<label>-linux-<arch>-fhs.tar.gz      FHS-style tree (bin/, share/), the
#                                                       Flatpak bundle's input; not the release
#                                                       archive, which build.yml writes
#
# Usage: packaging/linux/package.sh [--skip-build] [--formats "appimage deb rpm tar"]
#
# Needs: cargo and the Linux build dependencies from the README (plus clang for bindgen); nfpm
# for deb/rpm (https://nfpm.goreleaser.com); objdump (binutils). AppImage tooling is downloaded
# at verified pins into $CARGO_TARGET_DIR. Build on an old distro (CI: Ubuntu 22.04, glibc 2.35)
# so the binaries run on newer ones. Optional: desktop-file-validate, appstreamcli, zsyncmake
# (the zsync package) for the AppImage's .zsync. --skip-build packages the three binaries in
# $CARGO_TARGET_DIR/[$CARGO_BUILD_TARGET/]release as they are. OXIDEZAP_VERSION sets the
# version (packaging/env.sh), OXIDEZAP_MAINTAINER the deb/rpm maintainer.
set -euo pipefail
# Packaged files and directories get 644/755 whatever the caller's umask.
umask 022
# shellcheck source=../env.sh
. "$(dirname "${BASH_SOURCE[0]}")/../env.sh"
HERE="$ROOT/packaging/linux"
# shellcheck source=verified-download.sh
. "$HERE/verified-download.sh"
# The window's app id, the .desktop file and the icon name (APP_ID in
# crates/gui/src/platform/window_identity.rs).
DESKTOP_ID=org.oxidezap.client.local
# The AppStream component id (packaging/linux/org.oxidezap.client.local.metainfo.xml.in).
COMPONENT_ID=io.github.oxidezap.client
# All three ship together: the window and the CLI start the daemon beside themselves.
BINARIES=(oxidezap oxidezapd oxidezap-cli)

SKIP_BUILD=0
FORMATS="appimage deb rpm tar"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --skip-build) SKIP_BUILD=1; shift ;;
    --formats) FORMATS="$2"; shift 2 ;;
    -h | --help) sed -n '2,22p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

ARCH="$(uname -m)"
case "$ARCH" in
  x86_64) DEB_ARCH=amd64 ;;
  aarch64 | arm64) ARCH=aarch64; DEB_ARCH=arm64 ;;
  *) echo "unsupported architecture $ARCH" >&2; exit 2 ;;
esac
# The project's creator, as the first commit records him.
export OXIDEZAP_MAINTAINER="${OXIDEZAP_MAINTAINER:-João Lucas <jlucaso@hotmail.com>}"
BASENAME="oxidezap-$LABEL-linux-$ARCH"

echo "==> OxideZap $VERSION ($LABEL) for Linux $ARCH ($FORMATS)"

if [[ "$SKIP_BUILD" = 0 ]]; then
  (cd "$ROOT" && cargo build --release --locked --bin oxidezap --bin oxidezapd --bin oxidezap-cli)
fi
# cargo puts a --target (or CARGO_BUILD_TARGET) build one directory deeper.
BIN="$CARGO_TARGET_DIR/${CARGO_BUILD_TARGET:+$CARGO_BUILD_TARGET/}release"
for b in "${BINARIES[@]}"; do
  [[ -x "$BIN/$b" ]] || { echo "error: $BIN/$b not found (build it or drop --skip-build)" >&2; exit 1; }
done
WORK="$CARGO_TARGET_DIR/linux-package"
STAGE="$WORK/root"
rm -rf "$WORK"

# ---- stage an FHS tree (shared by every format) -------------------------------------------------
for b in "${BINARIES[@]}"; do
  install -Dm755 "$BIN/$b" "$STAGE/usr/bin/$b"
  strip "$STAGE/usr/bin/$b" 2>/dev/null || true
done
install -Dm644 "$HERE/$DESKTOP_ID.desktop" "$STAGE/usr/share/applications/$DESKTOP_ID.desktop"
mkdir -p "$STAGE/usr/share/metainfo"
sed -e "s/@VERSION@/$VERSION/g" -e "s/@DATE@/$BUILD_DATE/g" \
  "$HERE/$DESKTOP_ID.metainfo.xml.in" >"$STAGE/usr/share/metainfo/$COMPONENT_ID.metainfo.xml"
mkdir -p "$STAGE/usr/share/icons"
cp -R "$HERE/hicolor" "$STAGE/usr/share/icons/"
mkdir -p "$STAGE/usr/share/doc/oxidezap"
copy_docs "$STAGE/usr/share/doc/oxidezap"

if command -v desktop-file-validate >/dev/null; then
  desktop-file-validate "$STAGE/usr/share/applications/$DESKTOP_ID.desktop"
fi
if command -v appstreamcli >/dev/null; then
  appstreamcli validate --no-net --explain "$STAGE/usr/share/metainfo/$COMPONENT_ID.metainfo.xml"
fi

# The newest glibc symbol version any of the binaries needs: the deb/rpm depend on it, and it is
# the oldest distro the AppImage and tarball run on.
GLIBC_MIN="$(for b in "${BINARIES[@]}"; do objdump -T "$STAGE/usr/bin/$b"; done |
  grep -o 'GLIBC_[0-9][0-9.]*' | sed 's/^GLIBC_//' | sort -uV | tail -n 1)"
[[ -n "$GLIBC_MIN" ]] || { echo "error: could not read the glibc version from $STAGE/usr/bin" >&2; exit 1; }
echo "needs glibc >= $GLIBC_MIN"

# The CLI is the one binary that runs without a display or a session: a broken link or a missing
# library shows here rather than on a user's machine.
"$STAGE/usr/bin/oxidezap-cli" --help >/dev/null
# Its shell completions, which it prints itself and which call it back for each word.
CLI="$STAGE/usr/bin/oxidezap-cli"
mkdir -p "$STAGE/usr/share/bash-completion/completions" "$STAGE/usr/share/zsh/site-functions" \
  "$STAGE/usr/share/fish/vendor_completions.d"
"$CLI" completion --shell bash >"$STAGE/usr/share/bash-completion/completions/oxidezap-cli"
"$CLI" completion --shell zsh >"$STAGE/usr/share/zsh/site-functions/_oxidezap-cli"
"$CLI" completion --shell fish >"$STAGE/usr/share/fish/vendor_completions.d/oxidezap-cli.fish"

has() { case " $FORMATS " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }

# ---- -fhs.tar.gz --------------------------------------------------------------------------------
if has tar; then
  mkdir -p "$WORK/tar"
  cp -R "$STAGE/usr" "$WORK/tar/$BASENAME"
  tar -C "$WORK/tar" -czf "$DIST/$BASENAME-fhs.tar.gz" "$BASENAME"
  echo "wrote $DIST/$BASENAME-fhs.tar.gz"
fi

# ---- .deb / .rpm --------------------------------------------------------------------------------
if has deb || has rpm; then
  command -v nfpm >/dev/null || { echo "error: nfpm not found (https://nfpm.goreleaser.com/install/)" >&2; exit 1; }
  export NFPM_ARCH="$DEB_ARCH"
  # nfpm expands env vars in fields like `version` and `arch`, but not in `contents[].src` or
  # the dependency lists.
  sed -e "s|\${STAGE}|$STAGE|g" -e "s|\${GLIBC_MIN}|$GLIBC_MIN|g" "$HERE/nfpm.yaml" >"$WORK/nfpm.yaml"
  for fmt in deb rpm; do
    if has "$fmt"; then (cd "$ROOT" && nfpm package -f "$WORK/nfpm.yaml" -p "$fmt" -t "$DIST/$BASENAME.$fmt"); fi
  done
fi

# ---- AppImage -----------------------------------------------------------------------------------
if has appimage; then
  APPDIR="$WORK/OxideZap.AppDir"
  cp -R "$STAGE" "$APPDIR"
  mkdir -p "$APPDIR/usr/lib"
  # Wayland is loaded dynamically. GPU loaders and drivers come from the host.
  {
    for b in "${BINARIES[@]}"; do
      ldd "$APPDIR/usr/bin/$b"
    done
    for lib in libwayland-client.so libwayland-egl.so; do
      path="$(cc -print-file-name="$lib")"
      [[ -f "$path" ]] || { echo "error: $lib not found" >&2; exit 1; }
      soname="$(objdump -p "$path" | awk '$1 == "SONAME" { print $2 }')"
      printf '%s => %s\n' "$soname" "$path"
      ldd "$path"
    done
  } >"$WORK/appimage-dependencies"
  while read -r soname arrow path _; do
    [[ "$arrow" = '=>' ]] || continue
    case "$soname" in
      libc.so.* | libm.so.* | libdl.so.* | libpthread.so.* | librt.so.* | libresolv.so.* | ld-linux*) continue ;;
    esac
    [[ -f "$path" ]] || { echo "error: $soname not found" >&2; exit 1; }
    install -m644 "$path" "$APPDIR/usr/lib/$soname"
  done <"$WORK/appimage-dependencies"
  # AppRun is a script, not a symlink: it installs the .desktop entry and icons into
  # $XDG_DATA_HOME before launching, so Wayland compositors can resolve the window's
  # app_id to our icon instead of the generic Wayland logo (opt-out env in the script
  # header), and `--cli` runs oxidezap-cli. Covered by apprun-test.sh and apprun-cli-test.sh.
  install -Dm755 "$HERE/AppRun" "$APPDIR/AppRun"
  cp "$HERE/$DESKTOP_ID.desktop" "$APPDIR/$DESKTOP_ID.desktop"
  cp "$HERE/hicolor/256x256/apps/$DESKTOP_ID.png" "$APPDIR/$DESKTOP_ID.png"
  ln -s "$DESKTOP_ID.png" "$APPDIR/.DirIcon"

  # Digests from the official GitHub release asset metadata:
  # https://github.com/AppImage/appimagetool/releases/tag/1.9.1
  # https://github.com/AppImage/type2-runtime/releases/tag/20251108
  # Keep both pins together:
  # appimagetool otherwise downloads the mutable latest type2 runtime itself.
  TOOL_VERSION=1.9.1
  RUNTIME_VERSION=20251108
  case "$ARCH" in
    x86_64)
      TOOL_SHA256=ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0
      RUNTIME_SHA256=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d
      ;;
    aarch64)
      TOOL_SHA256=f0837e7448a0c1e4e650a93bb3e85802546e60654ef287576f46c71c126a9158
      RUNTIME_SHA256=00cbdfcf917cc6c0ff6d3347d59e0ca1f7f45a6df1a428a0d6d8a78664d87444
      ;;
  esac
  mkdir -p "$CARGO_TARGET_DIR"
  TOOL="${APPIMAGETOOL:-$CARGO_TARGET_DIR/appimagetool-$TOOL_VERSION-$ARCH.AppImage}"
  if [[ -n "${APPIMAGETOOL:-}" ]]; then
    verify_appimagetool_override "$TOOL" "$TOOL_SHA256" "$TOOL_VERSION" "$ARCH"
  else
    download_verified \
      "https://github.com/AppImage/appimagetool/releases/download/$TOOL_VERSION/appimagetool-$ARCH.AppImage" \
      "$TOOL" "$TOOL_SHA256" true
  fi
  RUNTIME="$CARGO_TARGET_DIR/type2-runtime-$RUNTIME_VERSION-$ARCH"
  download_verified \
    "https://github.com/AppImage/type2-runtime/releases/download/$RUNTIME_VERSION/runtime-$ARCH" \
    "$RUNTIME" "$RUNTIME_SHA256"
  # Absolute, because appimagetool runs in $DIST below (CARGO_TARGET_DIR or APPIMAGETOOL may be
  # relative).
  OUT="$(cd "$DIST" && pwd)/$BASENAME.AppImage"
  APPDIR="$(cd "$APPDIR" && pwd)"
  TOOL="$(cd "$(dirname "$TOOL")" && pwd)/$(basename "$TOOL")"
  RUNTIME="$(cd "$(dirname "$RUNTIME")" && pwd)/$(basename "$RUNTIME")"
  # A .zsync left by an earlier run would hide a missing zsyncmake and describe another file.
  rm -f "$OUT.zsync"
  # Update information: AppImageUpdate, AppImageLauncher and the like read it from the file and
  # fetch only the blocks that changed in a newer release, through the .zsync published next to
  # each AppImage on GitHub Releases. A nightly follows the rolling `nightly` release (a
  # pre-release, which `latest` skips); anything else follows `latest`, the newest release that
  # is not a pre-release. A fork's builds point at its own releases through GITHUB_REPOSITORY.
  REPO="${GITHUB_REPOSITORY:-oxidezap/client}"
  UPDATE_INFO="gh-releases-zsync|${REPO%%/*}|${REPO#*/}|$CHANNEL|oxidezap-*-linux-$ARCH.AppImage.zsync"
  # Extract-and-run: works without FUSE (containers, CI). The output embeds the static runtime,
  # so users don't need libfuse2 either. With zsyncmake on the host appimagetool also writes the
  # .zsync, into its working directory, hence the cd.
  (cd "$DIST" && ARCH="$ARCH" APPIMAGE_EXTRACT_AND_RUN=1 "$TOOL" --no-appstream \
    --runtime-file "$RUNTIME" -u "$UPDATE_INFO" "$APPDIR" "$OUT")
  echo "wrote $OUT"
  if [[ -s "$OUT.zsync" ]]; then
    echo "wrote $OUT.zsync"
  else
    warn "zsyncmake not found, so $OUT.zsync was not written; AppImage delta updates need it"
  fi
fi

echo "==> done"
ls -lh "$DIST"
