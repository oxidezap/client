#!/usr/bin/env bash
# AppImage CLI selection (AppRun --cli, or a symlink named oxidezap-cli): argument
# boundaries, stdio and exit status. Runs in seconds (CI: .github/workflows/packaging-lint.yml).
# No build or AppImage mount required; the bundled binaries are stand-ins.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
test_dir="$(mktemp -d)"
trap 'rm -rf "$test_dir"' EXIT
app_dir="$test_dir/OxideZap test.AppDir"
mkdir -p "$app_dir/usr/bin" "$app_dir/usr/share/applications"
cp "$HERE/AppRun" "$app_dir/AppRun"
printf '[Desktop Entry]\nType=Application\nName=OxideZap\nExec=oxidezap\n' \
  >"$app_dir/usr/share/applications/org.oxidezap.client.local.desktop"
cat >"$app_dir/usr/bin/oxidezap" <<'EOF'
#!/bin/sh
printf 'gui\n'
for arg do printf '<%s>\n' "$arg"; done
EOF
cat >"$app_dir/usr/bin/oxidezap-cli" <<'EOF'
#!/bin/sh
case "${1:-}" in
  read-line)
    IFS= read -r request
    printf '%s\n' "$request"
    printf 'cli stderr\n' >&2
    ;;
  --exit-37) exit 37 ;;
  *)
    printf 'cli\n'
    for arg do printf '<%s>\n' "$arg"; done
    ;;
esac
EOF
chmod +x "$app_dir/AppRun" "$app_dir/usr/bin/oxidezap" "$app_dir/usr/bin/oxidezap-cli"
export APPIMAGE="$test_dir/OxideZap test.AppImage"
: >"$APPIMAGE"
export ARGV0="$APPIMAGE"
export XDG_DATA_HOME="$test_dir/data"

fail() { echo "FAIL: $*" >&2; exit 1; }

out="$("$app_dir/AppRun" --cli send 'Some Chat' '' '*.txt' --text 'hello world')"
expected=$'cli\n<send>\n<Some Chat>\n<>\n<*.txt>\n<--text>\n<hello world>'
[[ "$out" = "$expected" ]] || fail "CLI arguments changed: $out"
[[ "$("$app_dir/AppRun" --cli)" = cli ]] || fail "empty CLI invocation"

# The AppImage runtime exposes the original symlink name through ARGV0.
for argv0 in oxidezap-cli ./oxidezap-cli "$test_dir/with spaces/oxidezap-cli"; do
  out="$(ARGV0="$argv0" "$app_dir/AppRun" --version)"
  [[ "$out" = $'cli\n<--version>' ]] || fail "CLI symlink not selected: $argv0"
done
out="$(ARGV0=oxidezap-cli "$app_dir/AppRun" --cli --version)"
[[ "$out" = $'cli\n<--version>' ]] || fail "CLI selector not removed with a symlink"

request='{"request":"ping"}'
out="$(printf '%s\n' "$request" | "$app_dir/AppRun" --cli read-line 2>"$test_dir/stderr")"
[[ "$out" = "$request" ]] || fail "CLI stdin/stdout changed: $out"
[[ "$(cat "$test_dir/stderr")" = 'cli stderr' ]] || fail "CLI stderr lost"
status=0
"$app_dir/AppRun" --cli --exit-37 || status=$?
[[ "$status" = 37 ]] || fail "CLI exit status changed: $status"
[[ ! -e "$XDG_DATA_HOME" ]] || fail "CLI performed desktop integration"

# GUI invocations still forward all arguments; only a leading --cli is a selector.
out="$("$app_dir/AppRun" 'some arg' --cli)"
[[ "$out" = $'gui\n<some arg>\n<--cli>' ]] || fail "GUI arguments changed: $out"
[[ -f "$XDG_DATA_HOME/applications/org.oxidezap.client.local.desktop" ]] || fail "GUI integration lost"
out="$(ARGV0=another-oxidezap-cli "$app_dir/AppRun" status)"
[[ "$out" = $'gui\n<status>' ]] || fail "an unrelated filename selected CLI"

# A broken package must fail visibly, rather than silently opening the GUI.
rm "$app_dir/usr/bin/oxidezap-cli"
status=0
"$app_dir/AppRun" --cli --version >"$test_dir/missing-out" 2>"$test_dir/missing-err" || status=$?
[[ "$status" != 0 ]] || fail "missing CLI succeeded"
[[ ! -s "$test_dir/missing-out" ]] || fail "missing CLI launched the GUI"
[[ -s "$test_dir/missing-err" ]] || fail "missing CLI had no diagnostic"

echo "AppRun CLI selection ok"
