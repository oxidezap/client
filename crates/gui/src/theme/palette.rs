//! The product palette and its built-in presets.
//!
//! A [`Palette`] is the complete set of colour roles the product draws with,
//! held as plain sRGB values so it can be read from and written back to
//! `theme.json`. Nothing here decides *where* a colour is used — that is
//! [`super::apply`]'s job, which projects a palette onto gpui-component's
//! `Theme` plus the product-only roles in [`super::ProductTheme`].
//!
//! Presets are the fallback floor: a palette is always a preset with zero or
//! more overrides on top, so a missing or malformed key can never leave a role
//! undefined.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// An sRGB colour as it appears in `theme.json`.
///
/// Kept as a `u32` rather than an `Hsla` so a round trip through the config
/// file returns the same string the user typed. Conversion to gpui's `Hsla`
/// happens once, at apply time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u32);

impl Rgb {
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// Blend towards `other` by `t` in `0.0..=1.0`, per channel.
    ///
    /// Used to derive the handful of roles that are a fixed relationship to
    /// another role rather than an independent design decision (a hover state,
    /// a translucent-looking tint over a known background). Deriving them keeps
    /// a custom `theme.json` coherent: overriding `primary` moves its hover
    /// with it instead of leaving a stale hand-picked value behind.
    pub fn mix(self, other: Self, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        let channel = |shift: u32| {
            let a = ((self.0 >> shift) & 0xff) as f32;
            let b = ((other.0 >> shift) & 0xff) as f32;
            (a + (b - a) * t).round().clamp(0.0, 255.0) as u32
        };
        Self((channel(16) << 16) | (channel(8) << 8) | channel(0))
    }

    /// Relative luminance per WCAG 2.1, used to pick a readable foreground.
    pub fn luminance(self) -> f32 {
        let channel = |shift: u32| {
            let c = (((self.0 >> shift) & 0xff) as f32) / 255.0;
            if c <= 0.039_28 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(16) + 0.7152 * channel(8) + 0.0722 * channel(0)
    }
}

impl fmt::Display for Rgb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{:06x}", self.0 & 0x00ff_ffff)
    }
}

/// Rejects anything that is not `#rgb` or `#rrggbb`, so a typo falls back to
/// the preset value instead of silently rendering black.
impl FromStr for Rgb {
    type Err = ParseColorError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let hex = s.strip_prefix('#').ok_or(ParseColorError)?;
        let value = u32::from_str_radix(hex, 16).map_err(|_| ParseColorError)?;
        match hex.len() {
            // #rgb expands each nibble, matching CSS.
            3 => {
                let expand = |nibble: u32| (nibble << 4) | nibble;
                Ok(Self(
                    (expand((value >> 8) & 0xf) << 16)
                        | (expand((value >> 4) & 0xf) << 8)
                        | expand(value & 0xf),
                ))
            }
            6 => Ok(Self(value)),
            _ => Err(ParseColorError),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseColorError;

impl fmt::Display for ParseColorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("expected a colour like \"#7aa2f7\" or \"#7af\"")
    }
}

impl std::error::Error for ParseColorError {}

impl Serialize for Rgb {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Rgb {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

/// Every colour role the product draws with.
///
/// Roles are named for what they mean, not where they sit: `elevated` is "a
/// surface above a card", not "the popover background", so the same value can
/// serve a popover, a menu and a title bar without three keys drifting apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Palette {
    /// Deepest surface: the conversation pane.
    pub background: Rgb,
    /// Panel surface: sidebar, headers, composer chrome.
    pub sidebar: Rgb,
    /// Card surface sitting on a panel: search field, call card, chips.
    pub secondary: Rgb,
    /// Elevated surface: popovers, menus, title bar, incoming bubbles.
    pub elevated: Rgb,

    /// Body text.
    pub foreground: Rgb,
    /// Secondary text that must still be comfortably readable.
    pub muted_foreground: Rgb,
    /// Metadata and monospace detail — timestamps, counters, hints.
    pub subtle_foreground: Rgb,
    /// Dimmest ink: shortcut chrome, drag handles, disabled glyphs.
    pub faint_foreground: Rgb,

    pub border: Rgb,
    /// Row hover.
    pub list_hover: Rgb,
    /// Selected row.
    pub list_active: Rgb,

    /// Principal action and selection emphasis.
    pub primary: Rgb,
    /// Keyboard focus ring. Deliberately distinct from `primary` so focus
    /// never reads as selection.
    pub ring: Rgb,
    pub danger: Rgb,
    pub warning: Rgb,
    pub success: Rgb,
    pub info: Rgb,

    /// Outgoing bubble.
    pub message_sent: Rgb,
    /// Incoming bubble.
    pub message_received: Rgb,

    /// Hues assigned to people — group senders, avatars, active speakers.
    ///
    /// Order is part of the contract: an identity maps to an index, so
    /// reordering repaints everyone. Extending the end is safe.
    pub speakers: [Rgb; 7],
}

impl Palette {
    /// The foreground that reads on `surface`, chosen by contrast rather than
    /// assumed — a light preset needs dark ink on `primary` where a dark one
    /// needs the reverse.
    pub fn on(&self, surface: Rgb) -> Rgb {
        if surface.luminance() > 0.4 {
            self.background
        } else {
            self.foreground
        }
    }

    /// The stable hue for an identity, so the same person keeps their colour
    /// across the member list, their bubbles and the call grid.
    pub fn speaker(&self, identity: &str) -> Rgb {
        // FNV-1a over the raw bytes: order-sensitive and stable across runs,
        // which `DefaultHasher` is explicitly not.
        let hash = identity.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
        });
        self.speakers[(hash % self.speakers.len() as u64) as usize]
    }
}

/// A named starting point a `theme.json` can `extends`.
///
/// The shared prefix is the themes' actual published names, not stutter:
/// shortening them to `Night`/`Storm`/`Light` would go ambiguous the moment a
/// preset from another family lands, and the strings in `id()` have to stay
/// `tokyo-night*` regardless because they are what users write in the file.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Preset {
    #[default]
    TokyoNight,
    TokyoNightStorm,
    TokyoNightLight,
}

impl Preset {
    /// Every preset, in the order Settings offers them.
    pub const ALL: [Self; 3] = [
        Self::TokyoNight,
        Self::TokyoNightStorm,
        Self::TokyoNightLight,
    ];

    /// The value written to `extends`.
    pub fn id(self) -> &'static str {
        match self {
            Self::TokyoNight => "tokyo-night",
            Self::TokyoNightStorm => "tokyo-night-storm",
            Self::TokyoNightLight => "tokyo-night-light",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::TokyoNight => "Tokyo Night",
            Self::TokyoNightStorm => "Tokyo Night Storm",
            Self::TokyoNightLight => "Tokyo Night Light",
        }
    }

    /// Whether the preset wants gpui-component's dark base theme underneath.
    pub fn is_dark(self) -> bool {
        !matches!(self, Self::TokyoNightLight)
    }

    pub fn palette(self) -> Palette {
        match self {
            Self::TokyoNight => TOKYO_NIGHT,
            Self::TokyoNightStorm => TOKYO_NIGHT_STORM,
            Self::TokyoNightLight => TOKYO_NIGHT_LIGHT,
        }
    }
}

impl FromStr for Preset {
    type Err = UnknownPresetError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|preset| preset.id() == s)
            .ok_or_else(|| UnknownPresetError(s.to_string()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownPresetError(pub String);

impl fmt::Display for UnknownPresetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown preset \"{}\"; known presets are ", self.0)?;
        for (i, preset) in Preset::ALL.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "\"{}\"", preset.id())?;
        }
        Ok(())
    }
}

impl std::error::Error for UnknownPresetError {}

const fn rgb(value: u32) -> Rgb {
    Rgb::new(value)
}

/// Tokyo Night, the product default.
pub const TOKYO_NIGHT: Palette = Palette {
    background: rgb(0x16161e),
    sidebar: rgb(0x1a1b26),
    secondary: rgb(0x1f2335),
    elevated: rgb(0x24283b),

    foreground: rgb(0xc0caf5),
    muted_foreground: rgb(0x7f87ac),
    subtle_foreground: rgb(0x565f89),
    faint_foreground: rgb(0x414868),

    border: rgb(0x2f334d),
    list_hover: rgb(0x232741),
    list_active: rgb(0x292e42),

    primary: rgb(0x73daca),
    ring: rgb(0x7aa2f7),
    danger: rgb(0xf7768e),
    warning: rgb(0xff9e64),
    success: rgb(0x9ece6a),
    info: rgb(0x7dcfff),

    message_sent: rgb(0x2b4d4a),
    message_received: rgb(0x24283b),

    speakers: [
        rgb(0x7aa2f7),
        rgb(0xbb9af7),
        rgb(0x7dcfff),
        rgb(0x9ece6a),
        rgb(0xff9e64),
        rgb(0xf7768e),
        rgb(0x73daca),
    ],
};

/// Tokyo Night Storm: the same hues on a lifted, bluer ground.
pub const TOKYO_NIGHT_STORM: Palette = Palette {
    background: rgb(0x1f2335),
    sidebar: rgb(0x24283b),
    secondary: rgb(0x292e42),
    elevated: rgb(0x2f344d),

    foreground: rgb(0xc0caf5),
    muted_foreground: rgb(0x8b93b8),
    subtle_foreground: rgb(0x636da3),
    faint_foreground: rgb(0x4a5279),

    border: rgb(0x3b4261),
    list_hover: rgb(0x2c3149),
    list_active: rgb(0x343a54),

    ..TOKYO_NIGHT
};

/// Tokyo Night Light (day): the same roles inverted, for bright rooms.
pub const TOKYO_NIGHT_LIGHT: Palette = Palette {
    background: rgb(0xe1e2e7),
    sidebar: rgb(0xd0d5e3),
    secondary: rgb(0xe9e9ec),
    elevated: rgb(0xf2f3f7),

    foreground: rgb(0x3760bf),
    muted_foreground: rgb(0x6172b0),
    subtle_foreground: rgb(0x848cb5),
    faint_foreground: rgb(0xa1a6c5),

    border: rgb(0xc4c8da),
    list_hover: rgb(0xd6dae6),
    list_active: rgb(0xc8cddd),

    primary: rgb(0x118c74),
    ring: rgb(0x2e7de9),
    danger: rgb(0xc64343),
    warning: rgb(0x8c6c3e),
    success: rgb(0x587539),
    info: rgb(0x07879d),

    message_sent: rgb(0xbfe6dc),
    message_received: rgb(0xf2f3f7),

    speakers: [
        rgb(0x2e7de9),
        rgb(0x9854f1),
        rgb(0x07879d),
        rgb(0x587539),
        rgb(0x8c6c3e),
        rgb(0xc64343),
        rgb(0x118c74),
    ],
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_hex_forms() {
        assert_eq!("#7aa2f7".parse::<Rgb>().unwrap(), Rgb(0x7aa2f7));
        assert_eq!("#7af".parse::<Rgb>().unwrap(), Rgb(0x77aaff));
    }

    #[test]
    fn rejects_malformed_colours() {
        // Each of these would otherwise resolve to a plausible-looking black.
        for bad in ["7aa2f7", "#7aa2f", "#gggggg", "", "#"] {
            assert!(bad.parse::<Rgb>().is_err(), "{bad} should not parse");
        }
    }

    #[test]
    fn colour_round_trips_through_its_string_form() {
        let colour = Rgb(0x2b4d4a);
        assert_eq!(colour.to_string().parse::<Rgb>().unwrap(), colour);
    }

    #[test]
    fn mix_reaches_both_endpoints() {
        let a = Rgb(0x000000);
        let b = Rgb(0xffffff);
        assert_eq!(a.mix(b, 0.0), a);
        assert_eq!(a.mix(b, 1.0), b);
        assert_eq!(a.mix(b, 0.5), Rgb(0x808080));
    }

    #[test]
    fn readable_ink_flips_with_surface_luminance() {
        let dark = TOKYO_NIGHT;
        // Teal `primary` is light enough to need the dark ground as ink.
        assert_eq!(dark.on(dark.primary), dark.background);
        assert_eq!(dark.on(dark.background), dark.foreground);
    }

    #[test]
    fn speaker_hue_is_stable_for_an_identity() {
        let palette = TOKYO_NIGHT;
        let jid = "5521999999999@s.whatsapp.net";
        assert_eq!(palette.speaker(jid), palette.speaker(jid));
    }

    #[test]
    fn every_preset_id_round_trips() {
        for preset in Preset::ALL {
            assert_eq!(preset.id().parse::<Preset>().unwrap(), preset);
        }
    }

    #[test]
    fn unknown_preset_is_rejected() {
        assert!("dracula".parse::<Preset>().is_err());
    }
}
