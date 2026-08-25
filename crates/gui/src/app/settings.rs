//! Settings screen state.
//!
//! Settings is a screen rather than a dialog: theme editing is exploratory,
//! and a modal that has to be dismissed to see what it did to the window is
//! the wrong shape for it. It opens over the conversation view and Escape
//! closes it.

use crate::theme::{ThemeSettings, config};

/// A destination in the settings nav.
///
/// Named for the object each one is about, not for the fact that they are
/// settings — the surrounding screen already says that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsSection {
    Account,
    #[default]
    Appearance,
    Notifications,
    AudioVideo,
    Privacy,
    Storage,
    Advanced,
}

impl SettingsSection {
    pub const ALL: [Self; 7] = [
        Self::Account,
        Self::Appearance,
        Self::Notifications,
        Self::AudioVideo,
        Self::Privacy,
        Self::Storage,
        Self::Advanced,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Account => "Account",
            Self::Appearance => "Appearance",
            Self::Notifications => "Notifications",
            Self::AudioVideo => "Audio & video",
            Self::Privacy => "Privacy & keys",
            Self::Storage => "Storage & media",
            Self::Advanced => "Advanced",
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Account => "account",
            Self::Appearance => "appearance",
            Self::Notifications => "notifications",
            Self::AudioVideo => "audio-video",
            Self::Privacy => "privacy",
            Self::Storage => "storage",
            Self::Advanced => "advanced",
        }
    }
}

/// What the Settings screen is showing and editing.
pub struct SettingsState {
    pub section: SettingsSection,
    /// The theme as it would be saved. Edited live so the window shows the
    /// result while it is being chosen, which is the only honest way to pick
    /// a colour.
    pub draft: ThemeSettings,
    /// The theme in force when Settings opened, so `Revert` has something to
    /// go back to without re-reading the file.
    pub original: ThemeSettings,
}

impl SettingsState {
    pub fn new(current: ThemeSettings) -> Self {
        Self {
            section: SettingsSection::default(),
            draft: current.clone(),
            original: current,
        }
    }

    /// Whether the draft departs from what was in force on open.
    pub fn is_dirty(&self) -> bool {
        self.draft.palette != self.original.palette
            || self.draft.preset != self.original.preset
            || self.draft.density != self.original.density
            || self.draft.font_size != self.original.font_size
    }

    /// Where the theme file lives, for the "Reveal" affordance.
    pub fn config_path(&self) -> Option<std::path::PathBuf> {
        config::config_path()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Preset;
    use crate::theme::metrics::Density;

    #[test]
    fn a_fresh_draft_is_clean() {
        assert!(!SettingsState::new(ThemeSettings::default()).is_dirty());
    }

    #[test]
    fn changing_any_dimension_marks_it_dirty() {
        for mutate in [
            (|s: &mut SettingsState| s.draft.density = Density::Compact) as fn(&mut SettingsState),
            |s: &mut SettingsState| s.draft.font_size = 18.0,
            |s: &mut SettingsState| s.draft.preset = Preset::TokyoNightLight,
            |s: &mut SettingsState| s.draft.palette.primary = crate::theme::Rgb::new(0xff00ff),
        ] {
            let mut state = SettingsState::new(ThemeSettings::default());
            mutate(&mut state);
            assert!(state.is_dirty());
        }
    }

    #[test]
    fn section_ids_are_distinct() {
        let mut ids: Vec<&str> = SettingsSection::ALL.iter().map(|s| s.id()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), SettingsSection::ALL.len());
    }
}
