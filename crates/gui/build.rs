//! Bring gpui-component's icon set into the build, for the target that has
//! nowhere to fetch it from.
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

fn copy(from: &Path, to: &Path) {
    std::fs::copy(from, to)
        .unwrap_or_else(|e| panic!("copying {} to {}: {e}", from.display(), to.display()));
}
