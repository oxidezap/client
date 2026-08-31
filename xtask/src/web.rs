//! Build the web front end into `web/dist`.
//!
//! Three things are not defaults and all three are required:
//!
//!   nightly      `build-std` is nightly-only, and gpui's own head uses
//!                unstable library features besides.
//!   build-std    the standard library has to be rebuilt with the atomics
//!                target feature on; the prebuilt one is not, and linking
//!                against it produces a module with no working threads.
//!   public-url   a project page is served from a subdirectory, so the
//!                generated glue has to be told where it is loading from.
//!
//! The first two are passed as environment rather than as flags: trunk has no
//! way to forward arguments to cargo, and `[unstable]` in a config file would
//! apply to the native build too — which is meant to stay on stable. Cargo
//! reads the same setting from `CARGO_UNSTABLE_BUILD_STD`, and here it reaches
//! only the child rather than this process and everything after it.
//!
//! The link flags themselves are in /.cargo/config.toml, under the wasm
//! target, so an ordinary `cargo build --target wasm32-unknown-unknown` gets
//! them too.

use std::fs;
use std::path::{Path, PathBuf};

use crate::sourcemap;
use crate::util::{Result, Run, env_or, repo_root};
use crate::{err, say};

pub fn build() -> Result<()> {
    // What this build is *for*, which is the one thing here that is a choice.
    //
    //   release  (default)  the bundle a visitor downloads
    //   debug               the same bundle, with its symbols
    //   dwarf               the same bundle, with its source lines
    //
    // `WEB_PROFILE=debug cargo xtask web build` is what to reach for when a
    // page is misbehaving and a profile or a panic trace has to name
    // something. It is the *same* build — `[profile.web-debug]` inherits `web`
    // — with `strip` off, so the name section survives into the module and
    // DevTools has Rust functions to show rather than indices. A build you are
    // diagnosing should be the build that misbehaved.
    //
    // Both are release builds in cargo's sense; an unoptimized gpui is
    // unusable, which is why `[profile.dev.package.gpui]` exists at all.
    //
    // `[profile.web]` and `[profile.web-debug]` in the workspace manifest are
    // where the per-crate decisions and their measurements live. Selected here
    // rather than being a `cfg`, because cargo has no per-target profiles and
    // the desktop build must keep the one it was calibrated for.
    //
    // Through trunk's own flag, which is the only way in: `--config` on a
    // cargo command line does reach the per-package overrides and trunk cannot
    // forward one, while the `CARGO_PROFILE_RELEASE_PACKAGE_<NAME>_OPT_LEVEL`
    // environment form, which would have needed neither, is silently ignored —
    // measured, byte for byte the same as not setting it.
    //
    // `dwarf` is the third and it is a different question: names answer "which
    // function", and only the line table answers "which line". It is a
    // separate profile rather than more of the debug one because the cost is
    // in another order of magnitude — DWARF is several times the module — and
    // because it is the only build here that must not be run through wasm-opt.
    // See `dwarf_index` below for why that is not a flag.
    let (cargo_profile, dwarf) = match env_or("WEB_PROFILE", "release").as_str() {
        "debug" => ("web-debug", false),
        "dwarf" => ("web-dwarf", true),
        _ => ("web", false),
    };

    // `TRUNK_ACTION=serve` runs the dev server through exactly the same
    // environment the published bundle is built with — a serve that differs
    // from the build is a difference nobody sees until deploy.
    let action = env_or("TRUNK_ACTION", "build");

    // Overridable, and split on whitespace so more than one flag fits; set it
    // to a single space to pass none.
    let profile_flags: Vec<String> = std::env::var("TRUNK_PROFILE")
        .map(|v| v.split_whitespace().map(str::to_string).collect())
        .unwrap_or_else(|_| vec!["--release".to_string()]);

    let web = repo_root().join("web");
    let dist = web.join("dist");

    // The target `index.html`, which is the shipped one unless this build
    // needs different attributes on the module's own link — trunk reads those
    // from the document and there is no flag for either of them.
    let index = if dwarf {
        Some(write_dwarf_index(&web)?)
    } else {
        None
    };

    let mut trunk = Run::new("trunk")
        .arg(&action)
        .args(&profile_flags)
        .args(["--cargo-profile", cargo_profile])
        .args(["--public-url", &env_or("PUBLIC_URL", "/")]);
    if let Some(index) = &index {
        // Positional, because it is where trunk takes the document to drive
        // the build from and `Trunk.toml`'s own `target` is what it overrides.
        trunk = trunk.arg(index);
    }
    trunk
        // Trunk finds `Trunk.toml` and `index.html` beside it.
        .current_dir(&web)
        // Named rather than exported, so nothing else this process goes on to
        // do is silently a nightly build.
        .env("RUSTUP_TOOLCHAIN", env_or("RUSTUP_TOOLCHAIN", "nightly"))
        .env("CARGO_UNSTABLE_BUILD_STD", "std,panic_abort")
        // The standard library, which `build-std` is already paying to rebuild
        // and was rebuilding at the defaults. `optimize_for_size` is what
        // `opt-level = "s"` is for `core` and `alloc`, which are 20% of the
        // shipped code section between them and are reached by no profile
        // setting here: build-std compiles them as its own units.
        //
        // Its louder sibling `panic_immediate_abort` is *not* here, and not by
        // choice: it stopped being a build-std feature and became a panic
        // strategy (`panic = "immediate-abort"`), which cargo will only accept
        // behind `cargo-features` in the workspace manifest — a key stable
        // cargo rejects outright, so declaring it would move the desktop build
        // to nightly cargo to save bytes in the web one.
        //
        // Set for both profiles, because the diagnostic build differs from the
        // shipped one only in what it keeps: one that size-optimized its
        // standard library differently would not be the build being diagnosed.
        .env("CARGO_UNSTABLE_BUILD_STD_FEATURES", "optimize_for_size")
        .run()?;

    // A serve never finishes, so anything after the run is about a build.
    if dwarf && action == "build" {
        settle_index(&dist)?;
        map(&dist)?;
    }
    Ok(())
}

/// Write the source map for the module in `dist`, and point the module at it.
///
/// Its own task as well as the tail of a `dwarf` build, because the build is
/// the expensive half: a map that came out wrong is worth another minute
/// rather than another hour.
pub fn map(dist: &Path) -> Result<()> {
    let wasm = module_in(dist)?;
    let summary = sourcemap::write(&wasm, &repo_root())?;
    say!(
        "{} -> {} ({} rows over {} sources, {} of them embedded, {:.1} MiB)",
        wasm.file_name().unwrap_or_default().to_string_lossy(),
        summary
            .map
            .file_name()
            .unwrap_or_default()
            .to_string_lossy(),
        summary.rows,
        summary.sources,
        summary.embedded,
        summary.bytes as f64 / (1024.0 * 1024.0),
    );
    Ok(())
}

/// The one `*_bg.wasm` trunk emitted. Named by a content hash, so it is found
/// rather than known — and more than one of them is a directory holding two
/// builds, which is a question rather than a thing to guess at.
fn module_in(dist: &Path) -> Result<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(dist)
        .map_err(|e| err!("could not read {}: {e}", dist.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with("_bg.wasm"))
        })
        .collect();
    found.sort();
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => Err(err!("no *_bg.wasm in {}", dist.display())),
        n => Err(err!(
            "{n} modules in {}; it holds more than one build",
            dist.display()
        )),
    }
}

/// The name trunk gives the output document is the name of the one it was
/// given, so a build driven from `index-dwarf.html` produces a directory whose
/// entry point is not `index.html`. Nothing else in this repository — the
/// bundle check, a static server, the service worker — knows that name, so it
/// is put back here rather than becoming a second thing to remember.
fn settle_index(dist: &Path) -> Result<()> {
    let generated = dist.join(DWARF_INDEX);
    if !generated.is_file() {
        return Ok(());
    }
    let index = dist.join("index.html");
    fs::rename(&generated, &index).map_err(|e| {
        err!(
            "could not rename {} to index.html: {e}",
            generated.display()
        )
    })
}

const DWARF_INDEX: &str = "index-dwarf.html";

/// The shipped document with the two attributes this build needs, written
/// beside it.
///
/// Both are properties of the module's own `<link>` and trunk reads them from
/// the document, so there is no command line that expresses either — which
/// leaves deriving a second document from the first, generated per build so it
/// cannot drift from the one it is derived from. It is gitignored for the same
/// reason.
fn write_dwarf_index(web: &Path) -> Result<PathBuf> {
    let source = web.join("index.html");
    let html = fs::read_to_string(&source)
        .map_err(|e| err!("could not read {}: {e}", source.display()))?;
    let target = web.join(DWARF_INDEX);
    fs::write(&target, dwarf_index(&html)?)
        .map_err(|e| err!("could not write {}: {e}", target.display()))?;
    Ok(target)
}

/// What that derivation is.
///
/// `data-wasm-opt="0"` turns wasm-opt off, and it is the half that is not
/// negotiable. wasm-opt moves code, and DWARF describes where code *is*: it
/// will rewrite the line table it is asked to keep, but only for the
/// transformations it knows how to follow, and `-Oz` is a pipeline of the
/// ones it does not. What comes out is a module with a line table that still
/// parses and no longer describes it, which is the one failure worth designing
/// against here — a debugger confidently naming the wrong line is worse than a
/// debugger naming nothing, because nothing about it looks broken.
///
/// `data-keep-debug` is wasm-bindgen's, and is the same sentence one step
/// earlier: it rewrites the module too, and throws the debug sections away
/// unless told not to.
fn dwarf_index(html: &str) -> Result<String> {
    let opt = "data-wasm-opt=\"z\"";
    if !html.contains(opt) {
        return Err(err!(
            "web/index.html no longer carries {opt}, which is what this build              has to turn off. `dwarf_index` in xtask/src/web.rs is what has to              learn the new spelling."
        ));
    }
    let generated = html.replace(opt, "data-wasm-opt=\"0\"\n      data-keep-debug");
    Ok(format!(
        "<!-- Generated by `WEB_PROFILE=dwarf cargo xtask web build` from \n\
         index.html beside it. Edit that one; this is rewritten per build. -->\n\
         {generated}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rewrite is a string replacement against a file it does not own, so
    /// what this really asserts is that the file still says what the
    /// replacement is looking for — the failure it exists to catch is
    /// `index.html` being reformatted and this quietly producing an optimized
    /// module with a line table describing some other module.
    #[test]
    fn the_shipped_document_is_one_the_dwarf_build_can_derive_from() {
        let html = fs::read_to_string(repo_root().join("web").join("index.html"))
            .expect("web/index.html is beside this crate");
        let generated = dwarf_index(&html).expect("the attribute is still there");
        assert!(generated.contains("data-wasm-opt=\"0\""));
        assert!(generated.contains("data-keep-debug"));
        assert!(!generated.contains("data-wasm-opt=\"z\""));
    }

    #[test]
    fn a_document_without_the_attribute_is_refused_rather_than_passed_through() {
        assert!(dwarf_index("<link data-trunk rel=\"rust\" />").is_err());
    }
}
