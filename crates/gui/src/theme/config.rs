//! `theme.json`: the palette as data rather than code.
//!
//! The file is a set of overrides on top of a named preset, never a whole
//! palette. That is what makes it safe to hand-edit: a key the user deletes,
//! misspells or fills with nonsense falls back to the preset instead of
//! leaving a role undefined, so no edit can produce an unreadable window.
//!
//! Loading therefore never fails. [`load`] always returns a usable
//! [`ThemeSettings`]; everything it could not honour comes back alongside it in
//! [`ThemeSettings::problems`] for Settings to show.

use crate::platform::prefs;
use std::collections::BTreeMap;

pub use prefs::Revision;

use serde::{Deserialize, Serialize};

use super::metrics::Density;
use super::palette::{Palette, Preset, Rgb};

/// Base font size in logical pixels, which `Root` turns into the window's
/// `rem` — so this is the application's zoom control, not just body type.
pub const DEFAULT_FONT_SIZE: f32 = 16.0;
/// Bounds on the base font. Below the floor the 1px hairlines and focus rings
/// stop resolving; above the ceiling a 1200px window can no longer show both
/// panes.
pub const MIN_FONT_SIZE: f32 = 11.0;
pub const MAX_FONT_SIZE: f32 = 24.0;

/// The file exactly as it is written on disk.
///
/// Every field is optional so that a two-line file is as valid as a complete
/// one. `deny_unknown_fields` is deliberate at this level: a misspelled
/// *section* is worth reporting, where a misspelled colour inside `colors` is
/// handled per key so one typo cannot discard the rest.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThemeFile {
    /// Preset to start from. Omitted means the product default.
    pub extends: Option<String>,
    /// Colour roles to override, by the names in [`PALETTE_KEYS`].
    pub colors: BTreeMap<String, String>,
    /// Message bubble colours, which carry authorship rather than a semantic
    /// role and so are named separately from `colors`.
    pub brand: BTreeMap<String, String>,
    /// `compact` or `comfortable`.
    pub density: Option<String>,
    /// Base font size in pixels; the whole rem scale follows it.
    pub font_size: Option<f32>,
}

/// A resolved, always-valid theme.
#[derive(Debug, Clone)]
pub struct ThemeSettings {
    pub preset: Preset,
    pub palette: Palette,
    pub density: Density,
    pub font_size: f32,
    /// What the file asked for that could not be honoured, in the order the
    /// keys appear. Empty means the file was fully applied — or absent.
    pub problems: Vec<String>,
}

impl Default for ThemeSettings {
    fn default() -> Self {
        let preset = Preset::default();
        Self {
            preset,
            palette: preset.palette(),
            density: Density::default(),
            font_size: DEFAULT_FONT_SIZE,
            problems: Vec::new(),
        }
    }
}

impl ThemeSettings {
    /// Resolve a parsed file into a palette, reporting whatever it could not
    /// apply. Pure, so the Settings editor can validate a draft the user has
    /// not saved yet.
    pub fn resolve(file: &ThemeFile) -> Self {
        let mut problems = Vec::new();

        let preset = match file.extends.as_deref() {
            None => Preset::default(),
            Some(name) => match name.parse::<Preset>() {
                Ok(preset) => preset,
                Err(err) => {
                    problems.push(err.to_string());
                    Preset::default()
                }
            },
        };

        let mut palette = preset.palette();
        apply_overrides(
            &mut palette,
            &file.colors,
            PALETTE_KEYS,
            "colors",
            &mut problems,
        );
        apply_overrides(
            &mut palette,
            &file.brand,
            BRAND_KEYS,
            "brand",
            &mut problems,
        );

        let density = match file.density.as_deref() {
            None => Density::default(),
            Some(name) => match name.parse::<Density>() {
                Ok(density) => density,
                Err(err) => {
                    problems.push(err.to_string());
                    Density::default()
                }
            },
        };

        let font_size = match file.font_size {
            None => DEFAULT_FONT_SIZE,
            Some(size) if (MIN_FONT_SIZE..=MAX_FONT_SIZE).contains(&size) => size,
            Some(size) => {
                problems.push(format!(
                    "font_size {size} is outside {MIN_FONT_SIZE}–{MAX_FONT_SIZE}; using {DEFAULT_FONT_SIZE}"
                ));
                DEFAULT_FONT_SIZE
            }
        };

        Self {
            preset,
            palette,
            density,
            font_size,
            problems,
        }
    }

    /// The file that would reproduce these settings, written as overrides on
    /// the preset so only real departures from it are recorded.
    pub fn to_file(&self) -> ThemeFile {
        let base = self.preset.palette();
        let diff = |keys: &[PaletteKey]| -> BTreeMap<String, String> {
            keys.iter()
                .filter_map(|key| {
                    let value = (key.get)(&self.palette);
                    (value != (key.get)(&base)).then(|| (key.name.to_string(), value.to_string()))
                })
                .collect()
        };

        ThemeFile {
            extends: Some(self.preset.id().to_string()),
            colors: diff(PALETTE_KEYS),
            brand: diff(BRAND_KEYS),
            density: Some(self.density.id().to_string()),
            font_size: Some(self.font_size),
        }
    }
}

/// One writable colour role, so the key list, the override pass and the
/// read-back in [`ThemeSettings::to_file`] all derive from one table and
/// cannot drift.
pub struct PaletteKey {
    pub name: &'static str,
    get: fn(&Palette) -> Rgb,
    set: fn(&mut Palette, Rgb),
}

macro_rules! palette_keys {
    ($($name:literal => $field:ident),* $(,)?) => {
        &[$(PaletteKey {
            name: $name,
            get: |palette| palette.$field,
            set: |palette, value| palette.$field = value,
        }),*]
    };
}

/// Semantic roles, in the order Settings lists them.
pub const PALETTE_KEYS: &[PaletteKey] = palette_keys![
    "background" => background,
    "sidebar" => sidebar,
    "secondary" => secondary,
    "elevated" => elevated,
    "foreground" => foreground,
    "muted_foreground" => muted_foreground,
    "subtle_foreground" => subtle_foreground,
    "faint_foreground" => faint_foreground,
    "border" => border,
    "list_hover" => list_hover,
    "list_active" => list_active,
    "primary" => primary,
    "ring" => ring,
    "danger" => danger,
    "warning" => warning,
    "success" => success,
    "info" => info,
];

/// Authorship colours, addressed under `brand` rather than `colors`.
pub const BRAND_KEYS: &[PaletteKey] = palette_keys![
    "message_sent" => message_sent,
    "message_received" => message_received,
];

fn apply_overrides(
    palette: &mut Palette,
    overrides: &BTreeMap<String, String>,
    keys: &[PaletteKey],
    section: &str,
    problems: &mut Vec<String>,
) {
    for (name, raw) in overrides {
        let Some(key) = keys.iter().find(|key| key.name == name) else {
            problems.push(format!("{section}.{name} is not a known colour role"));
            continue;
        };
        match raw.parse::<Rgb>() {
            Ok(colour) => (key.set)(palette, colour),
            Err(err) => problems.push(format!("{section}.{name}: {err}")),
        }
    }
}

/// Where the theme document is kept, in words a person can act on.
///
/// `None` means there is nowhere to keep one, which is not an error: the
/// product default applies and Settings reports the theme as unavailable
/// rather than offering to edit something that cannot exist.
///
/// A description rather than a path, because on one of the two platforms it
/// is not a path — see [`crate::platform::prefs`].
pub fn config_location() -> Option<String> {
    prefs::location()
}

/// Read and resolve the theme document. A missing one is the default; an
/// unreadable or malformed one is the default plus a problem to show.
pub fn load() -> ThemeSettings {
    match prefs::read() {
        Ok(Some(raw)) => parse(&raw),
        Ok(None) => ThemeSettings::default(),
        Err(problem) => ThemeSettings {
            problems: vec![problem],
            ..ThemeSettings::default()
        },
    }
}

/// Resolve theme text, so the Settings editor validates exactly what `load`
/// would do with the same bytes.
pub fn parse(raw: &str) -> ThemeSettings {
    match serde_json::from_str::<ThemeFile>(raw) {
        Ok(file) => ThemeSettings::resolve(&file),
        Err(err) => ThemeSettings {
            problems: vec![format!("line {}: {}", err.line(), err)],
            ..ThemeSettings::default()
        },
    }
}

/// Write settings back, wherever they are kept.
///
/// Returns where they landed, for the message that says so.
///
/// # Errors
///
/// Nowhere to keep them, or keeping them failed.
pub fn save(settings: &ThemeSettings) -> Result<String, String> {
    let mut json = serde_json::to_string_pretty(&settings.to_file())
        .map_err(|err| format!("this theme cannot be written: {err}"))?;
    json.push('\n');
    prefs::write(&json)?;
    Ok(prefs::location().unwrap_or_else(|| "the theme store".to_string()))
}

/// The theme as it would be written, for showing rather than saving.
///
/// Shares [`save_to`]'s serialization deliberately: a preview built any other
/// way is a second thing to keep in step, and the whole point of showing it is
/// that it is the truth.
pub fn preview(settings: &ThemeSettings) -> String {
    serde_json::to_string_pretty(&settings.to_file())
        .unwrap_or_else(|err| format!("// this theme cannot be written: {err}"))
}

/// The document's current version, used to notice an edit made outside the
/// app. Compared, never interpreted — see [`prefs::Revision`].
pub fn revision() -> Option<prefs::Revision> {
    prefs::revision()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::palette::TOKYO_NIGHT;

    fn resolve(raw: &str) -> ThemeSettings {
        parse(raw)
    }

    /// Absence resolves to the product default with nothing to report, which
    /// is what makes a first run on a machine with no theme document quiet.
    /// Asserted against the empty document rather than a missing one, because
    /// the store is what decides absence now and a test may not have one.
    #[test]
    fn nothing_configured_is_the_product_default() {
        let settings = ThemeSettings::default();
        assert_eq!(settings.preset, Preset::TokyoNight);
        assert_eq!(settings.palette, TOKYO_NIGHT);
        assert!(settings.problems.is_empty(), "absence is not a problem");
    }

    #[test]
    fn applies_the_documented_example() {
        let settings = resolve(
            r##"{
                "extends": "tokyo-night",
                "colors": { "primary": "#73daca", "ring": "#7aa2f7" },
                "brand": { "message_sent": "#2b4d4a" },
                "density": "comfortable"
            }"##,
        );
        assert!(settings.problems.is_empty(), "{:?}", settings.problems);
        assert_eq!(settings.palette.primary, Rgb(0x73daca));
        assert_eq!(settings.palette.message_sent, Rgb(0x2b4d4a));
        assert_eq!(settings.density, Density::Comfortable);
    }

    #[test]
    fn missing_keys_fall_back_to_the_preset() {
        let settings = resolve(r##"{ "extends": "tokyo-night-storm" }"##);
        assert_eq!(settings.palette, Preset::TokyoNightStorm.palette());
    }

    #[test]
    fn a_bad_colour_does_not_discard_its_neighbours() {
        let settings =
            resolve(r##"{ "colors": { "primary": "not a colour", "danger": "#ff0000" } }"##);
        assert_eq!(settings.palette.danger, Rgb(0xff0000));
        assert_eq!(
            settings.palette.primary, TOKYO_NIGHT.primary,
            "the unparseable key falls back rather than blanking the role"
        );
        assert_eq!(settings.problems.len(), 1);
    }

    #[test]
    fn unknown_role_is_reported_not_fatal() {
        let settings = resolve(r##"{ "colors": { "bakcground": "#000000" } }"##);
        assert_eq!(settings.palette, TOKYO_NIGHT);
        assert!(settings.problems[0].contains("bakcground"));
    }

    #[test]
    fn malformed_json_still_yields_a_usable_theme() {
        let settings = resolve("{ this is not json");
        assert_eq!(settings.palette, TOKYO_NIGHT);
        assert_eq!(settings.problems.len(), 1);
    }

    #[test]
    fn unknown_preset_falls_back_and_is_reported() {
        let settings = resolve(r##"{ "extends": "dracula" }"##);
        assert_eq!(settings.preset, Preset::TokyoNight);
        assert!(settings.problems[0].contains("dracula"));
    }

    #[test]
    fn font_size_outside_its_bounds_is_refused() {
        let settings = resolve(r##"{ "font_size": 96 }"##);
        assert_eq!(settings.font_size, DEFAULT_FONT_SIZE);
        assert_eq!(settings.problems.len(), 1);
    }

    #[test]
    fn settings_round_trip_through_the_file() {
        let mut settings = ThemeSettings {
            preset: Preset::TokyoNightStorm,
            palette: Preset::TokyoNightStorm.palette(),
            density: Density::Compact,
            font_size: 18.0,
            problems: Vec::new(),
        };
        settings.palette.primary = Rgb(0xff00ff);

        let json = serde_json::to_string(&settings.to_file()).unwrap();
        let reloaded = parse(&json);

        assert!(reloaded.problems.is_empty(), "{:?}", reloaded.problems);
        assert_eq!(reloaded.preset, settings.preset);
        assert_eq!(reloaded.palette, settings.palette);
        assert_eq!(reloaded.density, settings.density);
        assert_eq!(reloaded.font_size, settings.font_size);
    }

    #[test]
    fn only_real_departures_from_the_preset_are_written() {
        let settings = ThemeSettings {
            preset: Preset::TokyoNight,
            palette: TOKYO_NIGHT,
            ..ThemeSettings::default()
        };
        let file = settings.to_file();
        assert!(
            file.colors.is_empty() && file.brand.is_empty(),
            "an untouched preset should not be spelled out key by key"
        );
    }

    #[test]
    fn every_key_in_the_table_is_writable() {
        // Guards the macro: a `get`/`set` pair pointing at different fields
        // would leave the role unchanged here.
        for key in PALETTE_KEYS.iter().chain(BRAND_KEYS) {
            let mut palette = TOKYO_NIGHT;
            (key.set)(&mut palette, Rgb(0x123456));
            assert_eq!((key.get)(&palette), Rgb(0x123456), "{}", key.name);
        }
    }
}
