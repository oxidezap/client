# shellcheck shell=bash
# Shared setup for the Linux packaging scripts. Source it: `. "$(dirname "$0")/../env.sh"`.
#
# Exports:
#   ROOT              workspace root
#   LABEL             the version as it appears in file names: what the release workflows call
#                     the version (`nightly`, or a tag such as `v0.2.0`), else VERSION
#   VERSION           the package version: [workspace.package] version from Cargo.toml, or
#                     OXIDEZAP_VERSION without its leading `v`. A name that is not a version
#                     (`nightly`, `pr-12`) becomes <Cargo version>-<name>.<UTC timestamp>, so
#                     successive nightlies upgrade each other and sort below the release they
#                     lead up to.
#   CHANNEL           the release an AppImage looks for its updates in: `nightly` or `latest`
#   DIST              output directory for the packages (default: $ROOT/dist/release)
#   BUILD_DATE        UTC build date, YYYY-MM-DD (override: OXIDEZAP_BUILD_DATE)
#   CARGO_TARGET_DIR  cargo's target dir (default: $ROOT/target)

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export ROOT

# The version lives in one place: `[workspace.package] version` in the root Cargo.toml.
workspace_version() {
  awk '
    /^\[/ { in_pkg = ($0 == "[workspace.package]") ; next }
    in_pkg && $1 == "version" { gsub(/[" ]/, "", $3); print $3; exit }
  ' "$ROOT/Cargo.toml"
}

# The same check build.yml applies before a version becomes a file name.
case "${OXIDEZAP_VERSION:-}" in
  *[!A-Za-z0-9._-]* | -* | .*)
    echo "error: OXIDEZAP_VERSION is not a plain name: $OXIDEZAP_VERSION" >&2
    exit 1
    ;;
esac
export BUILD_DATE="${OXIDEZAP_BUILD_DATE:-$(date -u +%Y-%m-%d)}"
CHANNEL=latest
case "${OXIDEZAP_VERSION:-}" in
  "") VERSION="$(workspace_version)"; LABEL="$VERSION" ;;
  v[0-9]* | [0-9]*) LABEL="$OXIDEZAP_VERSION"; VERSION="${OXIDEZAP_VERSION#v}" ;;
  *)
    # `nightly`, or `pr-<number>` from release-check.yml: not a version, so a pre-release of
    # the Cargo.toml one. Dots and dashes are what semver allows in a pre-release.
    LABEL="$OXIDEZAP_VERSION"
    VERSION="$(workspace_version)-$(printf '%s' "$OXIDEZAP_VERSION" | tr -c 'A-Za-z0-9.\n' '.').$(date -u +%Y%m%d%H%M)"
    if [[ "$LABEL" = nightly ]]; then CHANNEL=nightly; fi
    ;;
esac
if [[ -z "$VERSION" || "$VERSION" = -* ]]; then
  echo "error: could not read [workspace.package] version from $ROOT/Cargo.toml" >&2
  exit 1
fi
export VERSION LABEL CHANNEL

DIST="${DIST:-$ROOT/dist/release}"
mkdir -p "$DIST"
export DIST

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"

# Emit a GitHub Actions warning (plain stderr outside Actions).
warn() {
  if [[ -n "${GITHUB_ACTIONS:-}" ]]; then echo "::warning::$*"; else echo "warning: $*" >&2; fi
}

# Copy the licence and readme files into a package's doc directory. The emoji font is compiled
# into the window, so its licence ships with every package, as it does in the release archives.
copy_docs() {
  local dest="$1" f
  for f in README.md LICENSE crates/gui/assets/fonts/noto-color-emoji/NotoColorEmoji-OFL.txt; do
    if [[ -f "$ROOT/$f" ]]; then cp "$ROOT/$f" "$dest/"; fi
  done
}

# Portable SHA-256 of a file (prints just the hash).
sha256() {
  if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}
