#!/bin/sh
# Register the extracted archive for this user. Keep its three binaries together.
set -eu
case "$0" in
    */*) script_dir=${0%/*}; script_dir=${script_dir:-/} ;;
    *) script_dir=. ;;
esac
# The delimiter preserves newline bytes that belong to a directory name.
archive_dir=$(CDPATH= cd -P -- "$script_dir" && printf '%s.' "$PWD")
archive_dir=${archive_dir%.}
case "$archive_dir" in
    *=*) printf 'Archive path cannot contain an equals sign\n' >&2; exit 1 ;;
    *'
'* | *"$(printf '\r')"*) printf 'Archive path cannot contain a newline\n' >&2; exit 1 ;;
esac
for binary in oxidezap oxidezapd oxidezap-cli; do
    if [ ! -x "$archive_dir/$binary" ]; then
        printf 'Missing executable beside this installer: %s\n' "$binary" >&2
        exit 1
    fi
done
app_id=org.oxidezap.client.local
data_dir=${XDG_DATA_HOME:-"$HOME/.local/share"}
case "$data_dir" in
    /*) ;;
    *) printf 'XDG_DATA_HOME must be an absolute path\n' >&2; exit 1 ;;
esac
# Desktop entries have two escaping layers: entry values, then Exec quoting.
# Percent is escaped separately because it introduces a desktop field code.
exec_path=$(printf '%s' "$archive_dir/oxidezap" | sed \
    -e 's/\\/\\\\\\\\/g' -e 's/"/\\\\"/g' -e 's/`/\\\\`/g' -e 's/\$/\\\\$/g' -e 's/%/%%/g')

mkdir -p "$data_dir/applications" "$data_dir/icons/hicolor/scalable/apps"
cp "$archive_dir/$app_id.svg" "$data_dir/icons/hicolor/scalable/apps/$app_id.svg"
{
    printf '[Desktop Entry]\nType=Application\nName=OxideZap\nComment=WhatsApp client\n'
    printf 'Exec="%s"\nIcon=%s\nStartupWMClass=%s\n' "$exec_path" "$app_id" "$app_id"
    printf 'Terminal=false\nCategories=Network;InstantMessaging;\nStartupNotify=true\n'
} > "$data_dir/applications/$app_id.desktop"
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$data_dir/applications" || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache --force --ignore-theme-index "$data_dir/icons/hicolor" >/dev/null 2>&1 || true
fi
printf 'Installed the OxideZap launcher. Keep this archive at %s\n' "$archive_dir"
