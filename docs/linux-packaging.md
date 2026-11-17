# Linux packages

Besides the release archive, every Linux build in `build.yml` produces
`oxidezap-<version>-linux-<arch>.{AppImage,AppImage.zsync,deb,rpm}` through
`packaging/linux/package.sh`, and the nightly and tagged releases attach them
next to the archives. `packaging-lint.yml` checks the scripts and metadata in
seconds; `release-check.yml` builds the packages on every pull request.

## Why

The archive runs anywhere its glibc floor allows, but it is a directory the
user has to keep in place, register with `install-desktop.sh` and replace by
hand on every update. A deb or rpm puts the three binaries in `/usr/bin`, beside
each other as the window and CLI require, with a menu entry, icons and AppStream
metadata, and updates through the package manager (as a download: there is no
package repository). The AppImage is one file that runs in place, registers its
own menu entry and icons on first start (`packaging/linux/AppRun`) and updates by
delta through the `.zsync` published next to it: a nightly AppImage follows the
`nightly` release, a tagged one `latest`.

The packaging is the one the Craft apps (PhotoCraft) ship: one script stages an
FHS tree, nfpm builds the deb and rpm, appimagetool and the type2 runtime are
downloaded at pinned SHA-256 digests. It builds on the same Ubuntu 22.04
runners the archives do, and `package.sh` reads the glibc floor from the
binaries (2.35 for the nightly of 2026-11-16) into the deb's and rpm's
dependency on libc.

## Decisions

- **Desktop id `org.oxidezap.client.local`**, the window's app id, so Wayland
  pairs the window with the launcher. `install-desktop.sh` writes the same file;
  when the AppImage finds such an entry it keeps its `Exec` as a user
  customization rather than taking the launcher over.
- **AppStream and Flatpak id `io.github.oxidezap.client`**: reverse-DNS on a
  name the project owns (the GitHub organization).
- **Package version.** A tag's version is the package version. A name that is
  not a version (`nightly`, `pr-<n>`) becomes a pre-release of the
  `Cargo.toml` version with a UTC timestamp (`0.1.0~nightly.202611171200` in a
  deb), so nightlies upgrade each other and sort below the release they lead
  up to.
- **Dependencies** are the libraries the binaries link (`objdump -p`) plus the
  ones they `dlopen` (Wayland, Vulkan or EGL); `packaging/linux/nfpm.yaml` lists
  them with the reasoning. The rpm names sonames, so one package resolves on
  Fedora and openSUSE.
- **Maintainer** is the project's creator as the first commit records him.
- **The AppImage runs the CLI too**: `--cli <args>`, or a symlink to it named
  `oxidezap-cli`, without desktop integration. A daemon started from the
  AppImage inherits the runtime's keep-alive pipe, so the mount stays until the
  daemon exits rather than the window or CLI that started it. The deb and rpm
  carry the CLI's bash, zsh and fish completions, which it prints itself.
- **No self-update to suppress.** The client has no self-install or update
  mechanism; `install-desktop.sh` belongs to the archive and is not shipped in
  the packages.

## Flatpak: built, broken, not released

`packaging/linux/flatpak-bundle.sh` repackages the binaries as a Flatpak
bundle with `packaging/linux/flatpak/io.github.oxidezap.client.yml`. It is
not attached to releases, because the daemon's tray icon cannot register in the
sandbox: ksni owns a D-Bus name `org.kde.StatusNotifierItem-<pid>-<n>` first,
and the sandbox's bus proxy does not let an app own it. The daemon logs `no tray
presence` and keeps running without one, and the tray is the only control the
daemon has once the window is closed. Granting `--own-name=org.kde.*` makes the
icon register, but lets the app own any KDE name, the tray watcher's included,
and the exact name cannot be granted because its pid is the sandbox's. The fix
belongs in `crates/daemon/src/tray/linux.rs`: `disable_dbus_name(true)` when
running sandboxed, as ksni documents for Flatpak. The window's Wayland app id should
follow `$FLATPAK_ID` too, or the launcher and window do not pair on Wayland.
The manifest repackages the release binaries rather than compiling, because
flatpak-builder builds offline and the workspace has git dependencies.
