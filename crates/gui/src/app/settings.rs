//! Settings screen state.
//!
//! Settings is a screen rather than a dialog: theme editing is exploratory,
//! and a modal that has to be dismissed to see what it did to the window is
//! the wrong shape for it. It opens over the conversation view and Escape
//! closes it.

use gpui::{Context, WeakEntity};

use crate::app::WhatsAppApp;
use crate::session::StorageUsage;
use crate::theme::{ActiveProductTheme as _, ThemeSettings, config};

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

    /// What Save would write, as text.
    ///
    /// The pane named the file and stopped there, which left no way to see
    /// what a preset actually *is* — or to check what an edit did — without
    /// leaving the app to open the file. This is the same serialization the
    /// writer uses, so what is shown is what would land.
    pub fn draft_json(&self) -> String {
        config::preview(&self.draft)
    }
}

impl WhatsAppApp {
    /// Put an open Settings screen back in step with a theme file that
    /// changed underneath it.
    ///
    /// The pane holds two copies — the draft it is editing and the state it
    /// would revert to — and the heartbeat installs an external edit into the
    /// global without either of them hearing about it. The controls then
    /// describe a palette the window is not showing, and the next density or
    /// preset click writes that whole stale palette back over the edit.
    ///
    /// A clean draft is nobody's work, so it adopts the file. A dirty one is
    /// somebody's, in progress, and is re-applied instead: the person
    /// choosing a colour right now outranks a background write, and the two
    /// agreeing again is what matters either way.
    pub(super) fn adopt_reloaded_theme(&mut self, cx: &mut gpui::App) {
        let Some(settings) = &mut self.settings else {
            return;
        };
        let loaded = cx.product().settings();
        if settings.is_dirty() {
            crate::theme::install(settings.draft.clone(), cx);
            return;
        }
        settings.draft = loaded.clone();
        settings.original = loaded;
    }

    /// What the store and the media cache occupy, as last measured.
    pub fn storage_usage(&self) -> Option<StorageUsage> {
        self.storage_usage
    }

    /// Ask the daemon to measure again.
    ///
    /// The daemon owns both paths, so it is the only process that can answer;
    /// this side holds the last answer and shows it while a new one is on the
    /// way, because a number that blanks every time the pane opens reads as a
    /// failure.
    pub fn refresh_storage_usage(&mut self, cx: &mut Context<Self>) {
        let Some(client) = &self.client else {
            return;
        };
        let waiting = client.storage_usage();
        // Which account asked. The task is detached and the daemon it asked
        // can be replaced while it is still measuring, so the answer has to
        // say whose it is: Settings stays open across a re-pair, and the old
        // account's totals landing under the new one is a number that is
        // simply untrue.
        let epoch = self.account_epoch;
        cx.spawn(async move |entity: WeakEntity<Self>, cx| {
            let Ok(usage) = waiting.await else {
                return;
            };
            let _ = entity.update(cx, |app, cx| {
                if app.account_epoch != epoch {
                    return;
                }
                app.storage_usage = Some(usage);
                cx.notify();
            });
        })
        .detach();
    }

    /// Delete the cached media and re-measure.
    pub fn clear_media_cache(&mut self, cx: &mut Context<Self>) {
        if let Some(client) = &self.client {
            client.clear_media_cache();
        }
        self.refresh_storage_usage(cx);
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
