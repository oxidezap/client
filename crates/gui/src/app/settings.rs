//! Settings screen state.
//!
//! Settings is a screen rather than a dialog: theme editing is exploratory,
//! and a modal that has to be dismissed to see what it did to the window is
//! the wrong shape for it. It opens over the conversation view and Escape
//! closes it.

use gpui::{App, Context, WeakEntity, Window};

use oxidezap_core::LogLevel;

use crate::app::WhatsAppApp;
use crate::app::notices::Tone;
use crate::session::StorageUsage;
use crate::theme::{ActiveProductTheme as _, ThemeSettings, config};
use log::{error, info};

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
    Plugins,
    Advanced,
}

impl SettingsSection {
    pub const ALL: [Self; 8] = [
        Self::Account,
        Self::Appearance,
        Self::Notifications,
        Self::AudioVideo,
        Self::Privacy,
        Self::Storage,
        Self::Plugins,
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
            Self::Plugins => "Plugins",
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
            Self::Plugins => "plugins",
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

    /// Where the theme document is kept, for the pane that names it.
    pub fn config_location(&self) -> Option<String> {
        config::config_location()
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

/// The Settings screen, and the answers it is the only thing that shows.
///
/// An entity because Settings is a screen: while it is up the conversation
/// view is not, and everything here — the theme being edited, the totals, the
/// log level somebody chose — is about that screen and nothing else. Its
/// methods take a `Context<Settings>`, so editing a palette cannot mark a
/// conversation as having moved.
///
/// What stays on the window is everything that has to *ask* the daemon: it
/// owns both the store and the media cache, so it is the only process that
/// can measure either, and the session is the window's.
pub(super) struct Settings {
    /// The screen, when it is open. `None` is the conversation view.
    open: Option<SettingsState>,
    /// What this account occupies on disk, as the daemon last measured it.
    storage: Option<StorageUsage>,
    /// The log level somebody chose in this front end, if they chose one.
    ///
    /// Kept so a reconnection can say it again: an ask made while the daemon
    /// was unreachable reached nobody, and one made before it restarted is
    /// one it may not have read. `None` is nobody having asked, which is not
    /// the same as `info` and must not be sent as one — a fresh window at the
    /// default must not quiet a daemon another window put at `debug`.
    ///
    /// It starts from the store where the store is this front end's own — a
    /// page's `localStorage`, which no daemon can open, so a choice made
    /// there is one only this side can carry across a reload. It does not on
    /// a desktop, where the stored answer is the daemon's own file and the
    /// daemon read it before this window existed.
    ///
    /// And never where this run was given a level from outside. `?log=` wins
    /// over the stored choice for the run it was given for, which is the
    /// whole of the precedence — so seeding from the store there would send
    /// the stored level at the first connection and, in the tab holding the
    /// account, hand it to a daemon sharing this process's own logging
    /// state: `?log=off` beside a stored `debug` would turn itself back on.
    log_level_asked: Option<LogLevel>,
    /// Which account the answers still in flight belong to.
    ///
    /// A measurement asked of one daemon can land after the window has been
    /// handed to another, and this screen survives the change — so the answer
    /// has to say whose it is. Bumped by [`Self::forget`]; an answer whose
    /// epoch no longer matches is dropped rather than displayed.
    account_epoch: usize,
}

impl Settings {
    pub(super) fn new() -> Self {
        Self {
            open: None,
            storage: None,
            log_level_asked: (crate::platform::log_store::is_ours()
                && oxidezap_logging::forced().is_none())
            .then(oxidezap_logging::stored)
            .flatten(),
            account_epoch: 0,
        }
    }

    pub(super) fn state(&self) -> Option<&SettingsState> {
        self.open.as_ref()
    }

    pub(super) fn is_open(&self) -> bool {
        self.open.is_some()
    }

    pub(super) fn show(&mut self, state: SettingsState, cx: &mut Context<Self>) {
        self.open = Some(state);
        cx.notify();
    }

    pub(super) fn close(&mut self, cx: &mut Context<Self>) -> bool {
        if self.open.take().is_some() {
            cx.notify();
            return true;
        }
        false
    }

    /// Move to another pane, and say which one it landed on.
    ///
    /// `None` when nothing moved — either there is no screen, or it was
    /// already there. The answer is what decides whether anything is
    /// re-measured, and asking again for a click that changed nothing is two
    /// directory reads in another process for no reason.
    pub(super) fn set_section(
        &mut self,
        section: SettingsSection,
        cx: &mut Context<Self>,
    ) -> Option<SettingsSection> {
        let open = self.open.as_mut()?;
        if open.section == section {
            return None;
        }
        open.section = section;
        cx.notify();
        Some(section)
    }

    /// What the store and the media cache occupy, as last measured.
    pub(super) fn storage(&self) -> Option<StorageUsage> {
        self.storage
    }

    /// Which account a measurement in flight has to still belong to.
    pub(super) fn epoch(&self) -> usize {
        self.account_epoch
    }

    /// Take a measurement, unless it is the previous account's.
    ///
    /// Settings stays open across a re-pair, and the old account's totals
    /// landing under the new one is a number that is simply untrue.
    pub(super) fn measured(&mut self, usage: StorageUsage, epoch: usize, cx: &mut Context<Self>) {
        if self.take_measurement(usage, epoch) {
            cx.notify();
        }
    }

    /// The half of [`Self::measured`] with the decision in it: whether this
    /// answer is about the account on screen, and therefore whether it is
    /// shown at all.
    fn take_measurement(&mut self, usage: StorageUsage, epoch: usize) -> bool {
        if self.account_epoch != epoch {
            return false;
        }
        self.storage = Some(usage);
        true
    }

    pub(super) fn log_level_asked(&self) -> Option<LogLevel> {
        self.log_level_asked
    }

    pub(super) fn remember_log_level(&mut self, level: LogLevel) {
        self.log_level_asked = Some(level);
    }

    /// What the *old* account occupied, and the query that is still measuring
    /// it.
    ///
    /// Settings survives the reset, so a completion landing after it would
    /// show the previous account's database and media under the new one; the
    /// epoch is what the detached task checks.
    pub(super) fn forget(&mut self, cx: &mut Context<Self>) {
        self.depart();
        cx.notify();
    }

    /// The half of [`Self::forget`] with no window in it. The epoch bump is
    /// the disowning: a measurement already in flight cannot be stopped, so
    /// it asks on the way back whether the account it was asked for is still
    /// the one on screen.
    fn depart(&mut self) {
        self.storage = None;
        self.account_epoch = self.account_epoch.wrapping_add(1);
    }

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
    pub(super) fn adopt_reloaded_theme(&mut self, cx: &mut Context<Self>) {
        let Some(settings) = &mut self.open else {
            return;
        };
        let loaded = cx.product().settings();
        if settings.is_dirty() {
            let draft = settings.draft.clone();
            crate::theme::install(draft, cx);
            return;
        }
        settings.draft = loaded.clone();
        settings.original = loaded;
        cx.notify();
    }

    /// Switch preset, taking its palette wholesale.
    ///
    /// Picking a preset means wanting that preset, so any per-key overrides
    /// go with it — keeping them would make the card lie about what is
    /// selected.
    pub(super) fn set_preset(
        &mut self,
        preset: crate::theme::Preset,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(settings) = &mut self.open {
            settings.draft.preset = preset;
            settings.draft.palette = preset.palette();
        }
        self.apply_draft(window, cx);
    }

    pub(super) fn set_density(
        &mut self,
        density: crate::theme::metrics::Density,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(settings) = &mut self.open {
            settings.draft.density = density;
        }
        self.apply_draft(window, cx);
    }

    /// Nudge the base font, which scales the whole interface.
    pub(super) fn step_font_size(
        &mut self,
        delta: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::theme::config::{MAX_FONT_SIZE, MIN_FONT_SIZE};
        let Some(settings) = &mut self.open else {
            return;
        };
        let next = (settings.draft.font_size + delta).clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
        if (next - settings.draft.font_size).abs() < f32::EPSILON {
            return;
        }
        settings.draft.font_size = next;
        self.apply_draft(window, cx);
    }

    /// Install the draft theme so the window shows the change while it is
    /// being chosen.
    pub(super) fn apply_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(settings) = &self.open else {
            return;
        };
        let draft = settings.draft.clone();
        crate::theme::install(draft, cx);
        // The base font is the rem reference, so frames already laid out
        // against the old one are stale.
        window.refresh();
        cx.notify();
    }

    /// Put back the theme that was in force when Settings opened.
    pub(super) fn revert(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(settings) = &mut self.open else {
            return;
        };
        settings.draft = settings.original.clone();
        self.apply_draft(window, cx);
    }

    /// Write the draft to `theme.json`.
    ///
    /// Reports the outcome in Settings rather than only to the log: a save
    /// that failed silently is indistinguishable from one that worked.
    pub(super) fn save(&mut self, cx: &mut Context<Self>) {
        let Some(settings) = &mut self.open else {
            return;
        };
        match crate::theme::config::save(&settings.draft) {
            Ok(location) => {
                info!("Wrote theme to {location}");
                // Cleared *before* the copy, or `original` keeps the warnings
                // the save just made untrue: reverting in the same session, or
                // closing and reopening Settings, resurrected complaints about
                // a file that is now valid.
                settings.draft.problems.clear();
                settings.original = settings.draft.clone();
            }
            Err(err) => {
                error!("Could not save the theme: {err}");
                settings.draft.problems = vec![format!("could not save: {err}")];
            }
        }
        cx.notify();
    }

    /// Re-read `theme.json` from disk, discarding the draft.
    pub(super) fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let loaded = crate::theme::config::load();
        crate::theme::install(loaded.clone(), cx);
        if let Some(settings) = &mut self.open {
            settings.draft = loaded.clone();
            settings.original = loaded;
        }
        window.refresh();
        cx.notify();
    }
}

impl WhatsAppApp {
    /// The Settings screen, when it is open.
    pub fn settings<'a>(&self, cx: &'a App) -> Option<&'a SettingsState> {
        self.settings.read(cx).state()
    }

    /// Whether a Settings screen is up, for `sync_overlay_focus` and for the
    /// render pass that draws it in place of the conversation.
    pub(super) fn showing_settings(&self, cx: &App) -> bool {
        self.settings.read(cx).is_open()
    }

    /// Put an open Settings screen back in step with a theme file that
    /// changed underneath it. See [`Settings::adopt_reloaded_theme`].
    pub(super) fn adopt_reloaded_theme(&mut self, cx: &mut gpui::App) {
        self.settings
            .update(cx, |settings, cx| settings.adopt_reloaded_theme(cx));
    }

    /// What the store and the media cache occupy, as last measured.
    pub fn storage_usage(&self, cx: &App) -> Option<StorageUsage> {
        self.settings.read(cx).storage()
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
        let settings = self.settings.clone();
        let epoch = settings.read(cx).epoch();
        cx.spawn(async move |_: WeakEntity<Self>, cx| {
            let Ok(usage) = waiting.await else {
                return;
            };
            settings.update(cx, |settings, cx| settings.measured(usage, epoch, cx));
        })
        .detach();
    }

    /// Change how much is logged, here and in the daemon, now and next time.
    ///
    /// Three things, and each is a different process or a different day.
    /// This window applies it to itself immediately — a front end writes its
    /// own share of the log, and a page attached to no daemon writes all of
    /// it. The daemon is told, because it holds the session and so writes
    /// nearly everything worth reading. And the choice is written down by
    /// whoever keeps a store of their own: the daemon's file on a desktop,
    /// which both processes read, and additionally the page's own browser
    /// store, which no daemon can reach.
    ///
    /// A store that will not take it is a notice rather than a refusal: the
    /// level *has* changed, and what failed is only the memory of it.
    pub fn set_log_level(&mut self, level: LogLevel, cx: &mut Context<Self>) {
        // This process, first and synchronously: it is one atomic store and
        // the point of the control.
        oxidezap_logging::apply(level);
        // Remembered as the window's own ask, so a reconnection can say it
        // again. Not to have the window impose its level on every daemon it
        // reaches — a fresh window at `info` must not quiet a daemon somebody
        // else put at `debug` — but a level chosen while the daemon was
        // unreachable, or chosen at all, is one the daemon never heard.
        self.settings
            .update(cx, |settings, _| settings.remember_log_level(level));
        let told_the_daemon = self
            .client
            .as_ref()
            .map(|client| client.set_log_level(level));

        // And written down by whoever keeps a store of their own. See
        // `platform::log_store`, which answers both halves of that: which
        // store, and on which thread — the desktop write is a file flushed
        // and renamed and belongs off the one that draws, and a page's is
        // `localStorage`, which exists on the window global and on no worker.
        //
        // Where the store is this front end's own, nothing is waited for:
        // no daemon writes it, and an answer that stalled — or a page
        // reloaded during the round trip — would take the choice with it.
        // Where it is not, the daemon writes the very file this window would
        // have, so the window writes nothing and two windows choosing at once
        // cannot leave the next start at the earlier answer. That is *waited
        // for* rather than assumed, because a frame handed to a full outbox
        // has been queued and not delivered: the daemon persists before it
        // acknowledges, so its answer is what makes "somebody remembered
        // this" true, and where it does not come this window is the only
        // thing left that can.
        let keeps_its_own = crate::platform::log_store::is_ours();
        let entity = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            if !keeps_its_own {
                let daemon_answered = match told_the_daemon {
                    Some(answer) => answer.await.is_ok(),
                    None => false,
                };
                if daemon_answered {
                    return;
                }
            }
            if let Err(e) = crate::platform::log_store::remember(cx).await {
                log::warn!("the log level was changed but not stored: {e}");
                let _ = entity.update(cx, |app, cx| {
                    app.notify_user(
                        "Logging at that level now, but the choice will not survive a restart.",
                        Tone::Problem,
                        cx,
                    );
                });
            }
        })
        .detach();
        cx.notify();
    }

    /// Say the level again to a daemon this window has just reached.
    ///
    /// Only the level somebody chose *here*, and only if they chose one: the
    /// daemon reads the same stored answer this window does when it starts,
    /// so replaying an untouched default would be this window quieting a
    /// daemon another window had raised. What it covers is the ask that had
    /// nowhere to go — Settings used while the daemon was unreachable, and
    /// the daemon that restarted, or was never told, under a page whose own
    /// choice lives in a browser store no daemon can read.
    pub fn resend_log_level(&self, cx: &App) {
        let (Some(level), Some(client)) = (self.settings.read(cx).log_level_asked(), &self.client)
        else {
            return;
        };
        // The answer is nobody's to wait for here: this is the window
        // repeating itself to a daemon it has just reached, and what it
        // wanted was for the level to arrive.
        let _answered = client.set_log_level(level);
    }

    /// Delete the cached media and re-measure.
    ///
    /// In that order. The daemon wipes before it acknowledges, so measuring
    /// alongside the request read a size the files still had and the Storage
    /// pane went on showing the old total, which reads as a clear that did
    /// not work.
    pub fn clear_media_cache(&mut self, cx: &mut Context<Self>) {
        let Some(client) = &self.client else {
            return;
        };
        let cleared = client.clear_media_cache();
        let entity = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            // A refusal drops the sender, and the size on screen is then the
            // last honest one there was.
            if cleared.await.is_err() {
                return;
            }
            let _ = entity.update(cx, |app, cx| app.refresh_storage_usage(cx));
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Preset;
    use crate::theme::metrics::Density;

    fn usage(bytes: u64) -> StorageUsage {
        StorageUsage {
            database_bytes: bytes,
            media_bytes: 0,
            media_files: 0,
        }
    }

    /// Settings survives a re-pair, and a measurement asked of the previous
    /// daemon can land after it. Shown under the new account, it is a number
    /// that is simply untrue.
    #[test]
    fn a_measurement_from_the_previous_account_is_dropped() {
        let mut settings = Settings::new();
        let asked_as = settings.epoch();

        settings.depart();

        assert!(!settings.take_measurement(usage(4096), asked_as));
        assert!(
            settings.storage().is_none(),
            "and nothing is drawn in its place"
        );
    }

    /// The same answer, asked for after the change, is the account's own.
    #[test]
    fn a_measurement_for_the_account_on_screen_is_taken() {
        let mut settings = Settings::new();
        settings.depart();
        let asked_as = settings.epoch();

        assert!(settings.take_measurement(usage(4096), asked_as));
        assert_eq!(settings.storage().map(|u| u.database_bytes), Some(4096));
    }

    /// A departure empties the total as well as disowning the query: what
    /// the *old* account occupied is not this one's.
    #[test]
    fn departing_takes_the_total_with_it() {
        let mut settings = Settings::new();
        let epoch = settings.epoch();
        assert!(settings.take_measurement(usage(4096), epoch));

        settings.depart();

        assert!(settings.storage().is_none());
        assert_ne!(settings.epoch(), epoch);
    }

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
