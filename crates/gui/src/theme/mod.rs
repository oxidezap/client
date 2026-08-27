//! Product theme.
//!
//! The palette is registered once into gpui-component's `Theme` global, so
//! every surface — ours and the library's own controls — reads the same tokens
//! through `cx.theme()`. Rendering code must not name a colour directly: a
//! literal in a component is invisible to theme switching and drifts from the
//! rest of the UI the moment either side changes.
//!
//! A handful of roles have no equivalent in gpui-component's `ThemeColor`:
//! the ink for monospace metadata, the two message-bubble colours that encode
//! authorship, and the hues assigned to people. Those live in
//! [`ProductTheme`], reached through [`ActiveProductTheme::product`], and are
//! tokens in exactly the same sense — not an excuse for a literal.
//!
//! ```ignore
//! let palette = &cx.product().palette;
//! div().bg(palette.hsla(palette.message_sent))
//! ```
//!
//! Values come from `~/.config/oxidezap/theme.json` layered over a named
//! preset; see [`config`]. Loading cannot fail, so a hand-edited file can
//! never leave the window unreadable.

mod apply;
pub mod config;
pub mod metrics;
pub mod palette;

use std::path::PathBuf;
use std::time::SystemTime;

use gpui::{App, Global, Hsla, Pixels, Size, rgb};

pub use config::ThemeSettings;
pub use metrics::Metrics;
pub use palette::{Palette, Preset, Rgb};

/// The product tokens that gpui-component's `Theme` has no field for, plus the
/// resolved scale and the provenance Settings needs to explain itself.
pub struct ProductTheme {
    pub palette: Palette,
    /// The scale in force: the base font the user asked for, fitted to the
    /// window. Everything drawn reads this one.
    pub metrics: Metrics,
    /// The base font as it is written in the file, before the window had a
    /// say. Settings shows and saves this — a scale the window imposed is not
    /// a preference to write back.
    base_font_size: f32,
    /// What the window's size did to that base. Kept so a theme reload — a
    /// palette edit, a font-size step — does not silently restore a design
    /// scale the screen has no room for.
    fit: f32,
    pub preset: Preset,
    /// Whatever the config file asked for that could not be honoured. Empty
    /// when the file is absent or fully applied.
    pub problems: Vec<String>,
    /// Where the file lives, or `None` when there is no config directory to
    /// look in.
    pub path: Option<PathBuf>,
    /// Last-modified stamp the current palette was read from, so an edit made
    /// outside the app can be noticed without re-parsing every tick.
    pub loaded_at: Option<SystemTime>,
}

impl Global for ProductTheme {}

impl ProductTheme {
    fn from_settings(settings: ThemeSettings, fit: f32) -> Self {
        let path = config::config_path();
        let loaded_at = path.as_deref().and_then(config::modified_at);
        Self {
            metrics: Metrics::new(settings.font_size * fit, settings.density),
            base_font_size: settings.font_size,
            fit,
            palette: settings.palette,
            preset: settings.preset,
            problems: settings.problems,
            path,
            loaded_at,
        }
    }

    /// The settings that produced this theme, for the Settings editor to show
    /// and write back.
    pub fn settings(&self) -> ThemeSettings {
        ThemeSettings {
            preset: self.preset,
            palette: self.palette,
            density: self.metrics.density(),
            font_size: self.base_font_size,
            problems: self.problems.clone(),
        }
    }

    /// A palette colour as gpui's colour type.
    pub fn hsla(&self, colour: Rgb) -> Hsla {
        rgb(colour.0).into()
    }

    /// The hue standing for an identity — a group sender, an avatar, a
    /// participant in a call grid.
    pub fn speaker(&self, identity: &str) -> Hsla {
        self.hsla(self.palette.speaker(identity))
    }
}

/// Reading the product tokens off any context that can reach globals.
///
/// Deliberately mirrors gpui-component's `ActiveTheme` so a component reads
/// `cx.theme()` for the shared roles and `cx.product()` for ours, without
/// having to know which is which beyond the name.
pub trait ActiveProductTheme {
    fn product(&self) -> &ProductTheme;
}

impl ActiveProductTheme for App {
    fn product(&self) -> &ProductTheme {
        self.global::<ProductTheme>()
    }
}

/// Load `theme.json` and install it. Call once during startup, after
/// `gpui_component::init`.
pub fn init(cx: &mut App) {
    install(config::load(), cx);
}

/// Install a resolved theme, replacing whatever is active.
///
/// Used by startup, by the Settings editor, and by [`reload_if_changed`]. The
/// caller refreshes the window: the base font is the rem reference, so frames
/// already laid out against the previous one are stale.
pub fn install(settings: ThemeSettings, cx: &mut App) {
    for problem in &settings.problems {
        log::warn!("theme.json: {problem}");
    }
    // Whatever the window last imposed, carried across. A palette edit is not
    // news about the screen's size, and reinstalling at full scale would put
    // a handheld back into a design it cannot fit until the next resize —
    // which, on a device whose window never resizes, is never.
    let fit = cx
        .try_global::<ProductTheme>()
        .map_or(1.0, |theme| theme.fit);
    apply::apply(&settings, fit, cx);
    cx.set_global(ProductTheme::from_settings(settings, fit));
}

/// Fold a window's size into the design scale.
///
/// The one place a viewport reaches the theme. Called from the root's render
/// pass, because that is where the window's size is a fact rather than an
/// event, and it answers whether anything moved so the caller can refresh a
/// window whose frames were laid out against the previous scale.
pub fn fit_to_viewport(viewport: Size<Pixels>, cx: &mut App) -> bool {
    let fit = metrics::viewport_fit(viewport);
    let current = cx.global::<ProductTheme>();
    if (current.fit - fit).abs() < f32::EPSILON {
        return false;
    }
    let settings = current.settings();
    install_fitted(settings, fit, cx);
    true
}

fn install_fitted(settings: ThemeSettings, fit: f32, cx: &mut App) {
    apply::apply(&settings, fit, cx);
    cx.set_global(ProductTheme::from_settings(settings, fit));
}

/// Whether there is a `theme.json` worth polling at all.
///
/// No file means nothing can change, and the poll can stop.
pub fn watches_a_file(cx: &App) -> bool {
    cx.global::<ProductTheme>().path.is_some()
}

/// Reinstall the theme if the file changed on disk since it was last read.
///
/// Polling rather than watching is deliberate: a watcher would be a new
/// dependency and a platform surface of its own for a file that changes when a
/// person saves it in an editor. The check is a `stat`, and it only reparses
/// when the stamp actually moves.
///
/// Returns whether the theme was replaced, so the caller knows to refresh.
pub fn reload_if_changed(cx: &mut App) -> bool {
    let Some(path) = cx.global::<ProductTheme>().path.clone() else {
        return false;
    };
    let modified = config::modified_at(&path);
    if modified == cx.global::<ProductTheme>().loaded_at {
        return false;
    }
    install(config::load_from(&path), cx);
    true
}
