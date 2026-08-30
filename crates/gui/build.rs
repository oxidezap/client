//! Two things the build knows and the source cannot: which revision this is,
//! and — for the target that has nowhere to fetch them from — gpui-component's
//! icons.
//!
//! The revision is what a bug report is worth reading with. A nightly moves
//! every push and the version string does not move with it, so "oxidezap
//! 0.1.0" names a hundred different builds; the short hash beside it names
//! one, and the window makes it a link to that commit.
//!
//! On the desktop `gpui-component-assets` embeds its own icons and this does
//! nothing. Its web implementation does not embed them — it downloads each
//! one from an endpoint on first use, which keeps a wasm bundle small at the
//! cost of the page needing somewhere to download from. A build published as
//! a static export has no such place, and a window whose icons arrive one
//! round trip at a time is a window that draws empty buttons first.
//!
//! So for wasm the icons are copied into `OUT_DIR` and embedded beside our
//! own. `gpui-component-assets` advertises where they are through the `links`
//! mechanism — `DEP_GPUI_COMPONENT_DEFAULT_ICONS_ICONS_DIR` — so nothing here
//! has to guess at a path inside a cargo checkout.

use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    revision();

    let is_wasm = std::env::var("CARGO_CFG_TARGET_FAMILY")
        .is_ok_and(|families| families.split(',').any(|family| family == "wasm"));
    // The directory is still created on the desktop, empty: `rust-embed`
    // resolves its folder at macro-expansion time and fails the build on one
    // that does not exist, even where nothing embeds from it.
    let destination = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"))
        .join("component-icons")
        .join("icons");
    let _ = std::fs::create_dir_all(&destination);
    if !is_wasm {
        return;
    }

    let Some(source) = std::env::var_os("DEP_GPUI_COMPONENT_DEFAULT_ICONS_ICONS_DIR") else {
        panic!(
            "gpui-component-assets did not advertise its icon directory; \
             the web build has no icons to embed"
        );
    };
    let source = PathBuf::from(source);
    println!("cargo:rerun-if-changed={}", source.display());

    let mut copied = 0usize;
    let entries =
        std::fs::read_dir(&source).unwrap_or_else(|e| panic!("reading {}: {e}", source.display()));
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "svg")
            && let Some(name) = path.file_name()
        {
            copy(&path, &destination.join(name));
            copied += 1;
        }
    }

    assert!(
        copied > 0,
        "no icons found in {} — the web build would draw empty buttons",
        source.display()
    );
}

/// Tell the window which commit it was built from.
///
/// Three answers, in order, and the last of them is silence rather than a
/// guess: a source archive has no `.git` and no build environment, and a
/// window saying `unknown` there would be a worse line than no line at all —
/// `render_versions` draws nothing when this is unset.
///
///   1. `OXIDEZAP_REV`, which is how a release build says it. CI has the
///      revision in the environment and often builds from an export rather
///      than a repository, so asking git there would answer nothing.
///   2. `git rev-parse`, which is every ordinary build from a checkout.
///   3. Nothing.
///
/// Truncated to seven, matching what git itself abbreviates to and what a
/// release title carries — and long enough that GitHub resolves it, which is
/// what makes the link work.
fn revision() {
    println!("cargo:rerun-if-env-changed=OXIDEZAP_REV");

    let from_env = std::env::var("OXIDEZAP_REV")
        .ok()
        .filter(|rev| !rev.trim().is_empty());
    let Some(rev) = from_env.or_else(git_revision) else {
        return;
    };
    let short: String = rev
        .trim()
        .chars()
        .filter(char::is_ascii_hexdigit)
        .take(7)
        .collect();
    if !short.is_empty() {
        println!("cargo:rustc-env=OXIDEZAP_REV={short}");
    }
}

/// The checkout's own answer, and what to watch so it stays current.
///
/// `rerun-if-changed` on `.git/HEAD` and on the file it names: without them
/// the revision is baked in at the first build and every later one reports
/// the commit that happened to be checked out that day. `git rev-parse
/// --git-path` rather than joining paths onto `.git`, because a worktree's
/// `.git` is a file and its HEAD is somewhere else entirely.
fn git_revision() -> Option<String> {
    let dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR")?);

    // HEAD moves when the checkout does; the ref it names moves when a
    // commit lands on the branch already checked out. A detached HEAD has no
    // second file, and holds the revision itself.
    //
    // Only files that are *there*: cargo reads a missing `rerun-if-changed`
    // path as changed, so naming a ref that lives in `packed-refs` rather
    // than in a file of its own would recompile this crate on every build
    // for the rest of the checkout's life.
    let mut watch = vec!["HEAD".to_owned()];
    watch.extend(git(&dir, &["symbolic-ref", "--quiet", "HEAD"]));
    for path in watch {
        if let Some(resolved) = git(&dir, &["rev-parse", "--git-path", &path]) {
            let resolved = dir.join(resolved);
            if resolved.exists() {
                println!("cargo:rerun-if-changed={}", resolved.display());
            }
        }
    }

    git(&dir, &["rev-parse", "--short=7", "HEAD"])
}

/// Run git in the crate's directory, or answer `None`.
///
/// Every way this fails means the same thing — no revision to report — so
/// they are one arm: git absent, this not being a repository, a repository
/// git refuses to read.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim().to_owned();
    (!text.is_empty()).then_some(text)
}

fn copy(from: &Path, to: &Path) {
    std::fs::copy(from, to)
        .unwrap_or_else(|e| panic!("copying {} to {}: {e}", from.display(), to.display()));
}
