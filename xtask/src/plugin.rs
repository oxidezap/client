//! Building a plugin, without the flags that would make it not one.
//!
//! The root `.cargo/config.toml` gives every `wasm32-unknown-unknown` build
//! under the tree the web front end's flags — `+atomics`, `--shared-memory`,
//! `--import-memory` — because in this repository that target *is* the web
//! front end. Cargo joins a target's `rustflags` from every config file it
//! finds walking up from the current directory, so a plugin built in its own
//! directory inherits them: the compiler warns that `atomics` is unstable,
//! the linker gives the module a shared, imported memory, and the daemon
//! refuses it at the first byte of the memory section. Nothing a config file
//! *under* the examples can say takes the flags back, since arrays are joined
//! rather than replaced; only the environment replaces them, and `RUSTFLAGS`
//! is what does.
//!
//! So this is the one place that knows to clear it. The READMEs, the building
//! guide, the host's own test and CI all say `cargo xtask plugin build <dir>`
//! rather than each carrying a copy of `RUSTFLAGS=` that one of them would
//! eventually lose — which is how the README came to say the bare command and
//! the guide the right one, with the daemon's answer the only thing telling
//! the two apart.

use std::path::{Path, PathBuf};

use crate::util::{Result, Run, env_or};
use crate::{err, say};

/// Build the plugin in `dir` for `wasm32-unknown-unknown` and answer where
/// the module landed.
///
/// A release build, because a plugin's `[profile.release]` is what makes it
/// kilobytes rather than hundreds of them, and because the path the host's
/// ignored test reads is the release one. Printed on standard output and
/// nothing else is, so `cp "$(cargo xtask plugin build examples/autoreply)"
/// <folder>` is a copy of the right file — cargo's own output goes to the
/// error stream, where it always did.
pub fn build(dir: &Path) -> Result<PathBuf> {
    let manifest = dir.join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest)
        .map_err(|e| err!("{} is not a plugin directory: {e}", dir.display()))?;
    let name =
        package_name(&text).ok_or_else(|| err!("{} names no package", manifest.display()))?;

    // The cargo this task was started by, so a `cargo +nightly xtask` builds
    // the plugin with the same toolchain, and a plain one stays on stable —
    // which is what a plugin wants: nothing in it needs nightly.
    Run::new(env_or("CARGO", "cargo"))
        .args(["build", "--release", "--target", TARGET])
        .arg("--manifest-path")
        .arg(&manifest)
        // Set to nothing rather than removed. An environment variable takes
        // precedence over every config file's `rustflags` and *replaces* them,
        // and cargo treats an empty one as set — so this is what leaves the
        // root config's wasm flags out. Removed, they would come back.
        .env("RUSTFLAGS", "")
        // Its encoded twin outranks `RUSTFLAGS` itself, so one inherited from
        // an outer cargo — this task runs under one — would put the flags
        // straight back.
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .run()?;

    let module = target_dir(dir)
        .join(TARGET)
        .join("release")
        .join(format!("{name}.wasm"));
    if !module.is_file() {
        return Err(err!(
            "the build finished but {} is not there; is `crate-type = [\"cdylib\"]` set?",
            module.display()
        ));
    }
    say!("{}", module.display());
    Ok(module)
}

const TARGET: &str = "wasm32-unknown-unknown";

/// Where cargo put the build: `CARGO_TARGET_DIR` if somebody set one, and
/// the directory's own `target/` otherwise.
///
/// Read from the environment rather than asked of cargo, because the answer
/// is only used to find one file and `cargo metadata` would pull the whole
/// graph for it. A relative `CARGO_TARGET_DIR` is relative to where cargo
/// ran, which is where this process runs.
fn target_dir(dir: &Path) -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| dir.join("target"))
}

/// The `name` under `[package]`, which is the plugin's id and the module's
/// file name.
///
/// A line reader rather than a TOML parser, because this crate takes no
/// dependencies and a manifest's package name is one quoted string on one
/// line. Anything stranger — a name on a continuation line, a `[package]`
/// table written inline — is not something either example does, and a
/// manifest that fools this is answered by the build finishing and the file
/// not being there, which `build` says.
fn package_name(manifest: &str) -> Option<String> {
    let mut in_package = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "name" {
            continue;
        }
        let value = value.split('#').next().unwrap_or("").trim();
        return value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .map(str::to_owned);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::repo_root;

    #[test]
    fn the_name_comes_from_the_package_table_and_nowhere_else() {
        let manifest = r#"
[package]
# This name is the plugin's id.
name = "autoreply" # trailing
version = "0.1.0"

[dependencies]
name = "not-this-one"
"#;
        assert_eq!(package_name(manifest).as_deref(), Some("autoreply"));
    }

    #[test]
    fn a_manifest_without_a_package_names_nothing() {
        assert_eq!(package_name("[workspace]\nmembers = []\n"), None);
        assert_eq!(package_name("[package]\nversion = \"1\"\n"), None);
    }

    /// The file's name is the plugin's id, and the READMEs say which file to
    /// copy. An example whose directory and package disagree would build a
    /// module under one name while every sentence about it used the other.
    #[test]
    fn each_example_is_named_after_its_directory() {
        let examples = repo_root().join("examples");
        let mut seen = 0;
        for entry in std::fs::read_dir(&examples).expect("examples/ is beside this crate") {
            let dir = entry.expect("readable").path();
            let Ok(manifest) = std::fs::read_to_string(dir.join("Cargo.toml")) else {
                continue;
            };
            let expected = dir.file_name().and_then(|n| n.to_str()).map(str::to_owned);
            assert_eq!(
                package_name(&manifest),
                expected,
                "{} should be named after its directory",
                dir.display()
            );
            seen += 1;
        }
        assert!(seen >= 2, "both examples were checked");
    }
}
