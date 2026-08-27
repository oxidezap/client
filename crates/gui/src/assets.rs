//! Custom asset source that combines gpui-component-assets with our custom icons

use anyhow::anyhow;
use gpui::{AssetSource, Result, SharedString};
use rust_embed::RustEmbed;
use std::borrow::Cow;

/// Our custom icons embedded at compile time
#[derive(RustEmbed)]
#[folder = "assets"]
#[include = "icons/**/*.svg"]
pub struct CustomIcons;

/// gpui-component's own icon set, embedded rather than fetched.
///
/// Only on the web, and only because its own asset source downloads them
/// there — one request per icon, from an endpoint a static export does not
/// have. `build.rs` copies them into `OUT_DIR` from where the crate says they
/// are. On the desktop the fallback below is that crate's embedded copy and
/// this is not built.
#[cfg(target_family = "wasm")]
#[derive(RustEmbed)]
#[folder = "$OUT_DIR/component-icons"]
#[include = "icons/**/*.svg"]
struct ComponentIcons;

/// Combined asset source that first checks our custom icons,
/// then falls back to gpui-component's set.
pub struct Assets;

/// The set to fall back to, however this build carries it.
#[cfg(not(target_family = "wasm"))]
fn fallback_load(path: &str) -> Result<Option<Cow<'static, [u8]>>> {
    gpui_component_assets::Assets.load(path)
}

#[cfg(target_family = "wasm")]
fn fallback_load(path: &str) -> Result<Option<Cow<'static, [u8]>>> {
    Ok(ComponentIcons::get(path).map(|file| file.data))
}

#[cfg(not(target_family = "wasm"))]
fn fallback_list(path: &str) -> Result<Vec<SharedString>> {
    gpui_component_assets::Assets.list(path)
}

#[cfg(target_family = "wasm")]
fn fallback_list(path: &str) -> Result<Vec<SharedString>> {
    Ok(ComponentIcons::iter()
        .filter_map(|p| p.starts_with(path).then(|| p.into()))
        .collect())
}

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path.is_empty() {
            return Ok(None);
        }

        // First try our custom icons
        if let Some(data) = CustomIcons::get(path) {
            return Ok(Some(data.data));
        }

        // Fall back to gpui-component's set
        fallback_load(path).map_err(|e| anyhow!("could not find asset at path \"{path}\": {e}"))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        // Combine both lists
        let mut items: Vec<SharedString> = CustomIcons::iter()
            .filter_map(|p| p.starts_with(path).then(|| p.into()))
            .collect();

        if let Ok(component_items) = fallback_list(path) {
            for item in component_items {
                if !items.contains(&item) {
                    items.push(item);
                }
            }
        }

        Ok(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `#[folder]`/`#[include]` pair is resolved at build time, so a
    /// renamed or moved icon fails here rather than as a blank button.
    #[test]
    fn every_embedded_icon_loads() {
        let icons: Vec<String> = CustomIcons::iter().map(|p| p.to_string()).collect();
        assert!(!icons.is_empty(), "no icons were embedded at all");

        for name in &icons {
            let loaded = Assets
                .load(name)
                .unwrap_or_else(|e| panic!("{name} failed to load: {e}"));
            let bytes = loaded.unwrap_or_else(|| panic!("{name} resolved to nothing"));
            assert!(
                bytes.starts_with(b"<svg") || bytes.starts_with(b"<?xml"),
                "{name} does not look like an SVG"
            );
        }
    }

    /// `list` is what gpui asks for icon lookup; a prefix that matches nothing
    /// is a silently missing icon rather than an error.
    #[test]
    fn list_returns_our_icons_under_their_prefix() {
        let listed = Assets.list("icons/").expect("list");
        for name in CustomIcons::iter() {
            assert!(
                listed.iter().any(|p| p.as_ref() == name.as_ref()),
                "{name} missing from list"
            );
        }
    }
}
