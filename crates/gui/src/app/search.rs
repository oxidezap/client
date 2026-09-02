//! The window's two searches.
//!
//! The sidebar's field filters *chats* by name; the header's magnifier
//! searches *messages* in the conversation on screen. They were one control
//! once — the header's magnifier said "Search in conversation" and focused
//! the sidebar's field, so it did something other than what it was labelled —
//! and keeping them in one module is what keeps that distinction written down
//! rather than rediscovered.
//!
//! The conversation's half searches what is loaded, and says so when that is
//! all it searched: the store's FTS index lives behind the daemon, and a
//! control that silently covers only part of a history is worse than one that
//! names its horizon.

use gpui::{Context, Entity, Task, WeakEntity, Window};
use gpui_component::input::InputState;
use oxidezap_core::ChatMessage;

use super::WhatsAppApp;

/// How long a keystroke waits before the list is filtered again.
///
/// Long enough that typing a word is one filter rather than five, short
/// enough that it reads as instant. The conversation's search has no
/// equivalent because it walks messages already in memory, while this one
/// rebuilds a cache over every chat the window holds.
const LIST_DEBOUNCE_MS: u64 = 150;

/// Both of this window's searches, and the fields they are typed into.
///
/// Two searches rather than one, and deliberately not merged: the sidebar's
/// field filters *chats* by name and the header's searches *messages* in the
/// open conversation. They are together because they are the same kind of
/// state — a query, a box to type it in, and nothing else the window needs —
/// and because Escape has to dismiss them in an order, which is one decision
/// about two things.
///
/// An entity because the pair's methods can then say, in their signatures,
/// that typing a letter into the sidebar changes a query and no chat: none of
/// them can reach a `Context<WhatsAppApp>`. What they cannot do alone is act
/// on a result — filtering the list invalidates the window's cache, and
/// following a match scrolls the window's timeline — so both boxes are built
/// by the window, and it is the window that hears them change.
///
/// That last part is load-bearing rather than incidental. A subscription
/// belongs to the context it is registered in, and gpui hands the handler its
/// entity *leased out of the map*: a handler registered here that called back
/// into the window would return through a method that updates this entity,
/// and a second lease of one entity is a panic, not a glitch. So the boxes
/// are the window's, their events are the window's, and what arrives here is
/// the result.
pub(super) struct Search {
    /// The sidebar's field. Created the first time a window has one to build
    /// it in.
    list_input: Option<Entity<InputState>>,
    /// What the list is filtered by: lowercase, trimmed, and one debounce
    /// behind what is in the box.
    list_query: String,
    /// The debounce. Dropped to cancel it, which is what a later keystroke
    /// does.
    debounce: Option<Task<()>>,
    /// Searching inside the open conversation, when that is open. Separate
    /// from the field above, which filters chats by name.
    conversation: Option<ConversationSearch>,
    /// The field for it, created the first time the search is opened.
    conversation_input: Option<Entity<InputState>>,
}

impl Search {
    pub(super) fn new() -> Self {
        Self {
            list_input: None,
            list_query: String::new(),
            debounce: None,
            conversation: None,
            conversation_input: None,
        }
    }

    pub(super) fn list_input(&self) -> Option<&Entity<InputState>> {
        self.list_input.as_ref()
    }

    /// What the chat list is filtered by, lowercased and trimmed.
    pub(super) fn list_query(&self) -> &str {
        &self.list_query
    }

    /// Whether a search is currently narrowing the list, which is what makes
    /// an empty list mean "no matches" rather than "no chats".
    pub(super) fn narrowing_the_list(&self) -> bool {
        !self.list_query.is_empty()
    }

    pub(super) fn conversation(&self) -> Option<&ConversationSearch> {
        self.conversation.as_ref()
    }

    pub(super) fn conversation_input(&self) -> Option<&Entity<InputState>> {
        self.conversation_input.as_ref()
    }

    /// Put the caret in the sidebar's field.
    ///
    /// The field itself is built by the window — see
    /// [`WhatsAppApp::ensure_chat_search_input`] for why — so this is only
    /// ever asked once there is one.
    pub(super) fn focus_list_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(input) = &self.list_input {
            input.update(cx, |state, cx| state.focus(window, cx));
        }
    }

    /// Keep the box the window built for the sidebar.
    pub(super) fn adopt_list_input(&mut self, input: Entity<InputState>) {
        self.list_input = Some(input);
    }

    /// Take what was typed, and say what to do about it now.
    ///
    /// `true` means the list is already filtered differently and the window's
    /// cache is stale; `false` means a debounce is running and the answer
    /// comes later, through the same route.
    ///
    /// Emptying is immediate on purpose: clearing a search is somebody asking
    /// for their list back, and making them wait for a timer to notice is the
    /// one case where the debounce is felt.
    pub(super) fn set_list_query(
        &mut self,
        query: String,
        app: WeakEntity<WhatsAppApp>,
        cx: &mut Context<Self>,
    ) -> bool {
        // Whatever was queued was for an earlier keystroke.
        self.debounce = None;
        if query.is_empty() {
            self.list_query.clear();
            cx.notify();
            return true;
        }
        let trimmed = query.trim().to_lowercase();
        self.debounce = Some(cx.spawn(async move |me: WeakEntity<Self>, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(LIST_DEBOUNCE_MS))
                .await;
            let _ = me.update(cx, |search, cx| {
                search.list_query = trimmed;
                cx.notify();
            });
            let _ = app.update(cx, |app, cx| {
                app.invalidate_chat_cache();
                cx.notify();
            });
        }));
        false
    }

    /// Empty the field and the query behind it.
    ///
    /// The box is set as well as the query, because this is reached from
    /// Escape and from the button as well as from typing — and a query
    /// cleared under text still on screen is a list that disagrees with its
    /// own field.
    ///
    /// Dropping the debounce is the other half of that, and it is a fix
    /// rather than tidying: the timer armed by the last keystroke used to
    /// outlive the clear, land a hundred milliseconds later, and filter the
    /// list by a word that was no longer in the box — with nothing on screen
    /// to say why the chats had gone.
    pub(super) fn clear_list_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(input) = &self.list_input {
            input.update(cx, |state, cx| state.set_value("", window, cx));
        }
        self.debounce = None;
        self.list_query.clear();
        cx.notify();
    }

    /// Everything a departing account typed into either field.
    pub(super) fn forget(&mut self, cx: &mut Context<Self>) {
        self.debounce = None;
        self.list_query.clear();
        self.conversation = None;
        cx.notify();
    }

    /// Open the search over `jid`.
    ///
    /// The box is the window's to build, and is handed over on the first
    /// opening; every later one reuses it.
    pub(super) fn open_conversation(
        &mut self,
        jid: String,
        input: Option<Entity<InputState>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.conversation = Some(ConversationSearch::new(jid));
        if let Some(input) = input {
            self.conversation_input = Some(input);
        }
        if let Some(input) = &self.conversation_input {
            input.update(cx, |state, cx| {
                state.set_value("", window, cx);
                state.focus(window, cx);
            });
        }
        cx.notify();
    }

    pub(super) fn close_conversation(&mut self, cx: &mut Context<Self>) -> bool {
        if self.conversation.take().is_some() {
            cx.notify();
            return true;
        }
        false
    }

    /// Re-run the conversation's query over `messages`, and say which message
    /// the timeline should be showing.
    ///
    /// The messages come from above because a chat is the window's. Nothing
    /// is cloned to get them here: the search is refreshed in place, which is
    /// what the borrow used to make impossible — holding one field mutably
    /// while reading another forced a copy of the entire message vector, on
    /// every keystroke, in the longest conversation the reader has.
    pub(super) fn refresh_conversation(
        &mut self,
        query: &str,
        messages: &[ChatMessage],
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let search = self.conversation.as_mut()?;
        search.refresh(query, messages);
        cx.notify();
        search.current_match().map(str::to_string)
    }

    /// Walk the matches. `forward` is down the timeline, the way reading goes.
    pub(super) fn step_conversation(
        &mut self,
        forward: bool,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let target = self
            .conversation
            .as_mut()?
            .step(forward)
            .map(str::to_string);
        if target.is_some() {
            cx.notify();
        }
        target
    }

    /// Whether an open search is about `jid` and has matches that a change
    /// to that conversation could have made wrong.
    pub(super) fn stale_for(&self, jid: &str) -> bool {
        self.conversation
            .as_ref()
            .is_some_and(|search| search.jid == jid && !search.query.is_empty())
    }

    /// Re-run the open search over the messages it is about.
    ///
    /// The matches used to be rebuilt only when the *query* changed, so a
    /// message arriving, a history merge, an edit or a revoke left the count
    /// and the navigation describing a conversation that had moved on — with
    /// no way to correct it but to retype the query.
    pub(super) fn refresh_open(&mut self, messages: &[ChatMessage], cx: &mut Context<Self>) {
        let Some(search) = self.conversation.as_mut() else {
            return;
        };
        let query = search.query.clone();
        search.refresh(&query, messages);
        cx.notify();
    }

    /// Close a search that was typed for a different conversation.
    ///
    /// A search belongs to the conversation it was typed for, so leaving that
    /// conversation closes it rather than carrying a query into a chat it
    /// says nothing about.
    pub(super) fn close_unless_about(&mut self, jid: &str, cx: &mut Context<Self>) {
        if self
            .conversation
            .as_ref()
            .is_some_and(|search| search.jid != jid)
        {
            self.conversation = None;
            cx.notify();
        }
    }

    /// Which chat the open search belongs to, if one is open.
    pub(super) fn conversation_jid(&self) -> Option<&str> {
        self.conversation.as_ref().map(|search| search.jid.as_str())
    }
}

/// One conversation's search, while it is open.
pub struct ConversationSearch {
    /// The chat being searched. Switching chats closes the search rather than
    /// carrying a query into a conversation it was never typed for.
    pub jid: String,
    /// Lowercased and trimmed, like the chat-list query.
    pub query: String,
    /// Ids of the matching messages, newest last — timeline order, so
    /// "next" walks the way the eye does.
    pub matches: Vec<String>,
    /// Which match is current, when there are any.
    pub current: usize,
}

impl ConversationSearch {
    pub fn new(jid: String) -> Self {
        Self {
            jid,
            query: String::new(),
            matches: Vec::new(),
            current: 0,
        }
    }

    /// The id the timeline should be showing.
    pub fn current_match(&self) -> Option<&str> {
        self.matches.get(self.current).map(String::as_str)
    }

    /// "3 of 12", or what to say instead.
    ///
    /// The empty answer names its horizon. A conversation holds the messages
    /// this window has loaded — one page, unless the reader has asked for
    /// more — while the store behind the daemon holds the rest and an FTS
    /// index over it. "No matches" is a claim about the whole history that
    /// this search is in no position to make; "in the loaded messages" is
    /// what it actually looked at.
    pub fn status(&self) -> Option<String> {
        if self.query.is_empty() {
            return None;
        }
        if self.matches.is_empty() {
            return Some("No matches in the loaded messages".to_string());
        }
        Some(format!("{} of {}", self.current + 1, self.matches.len()))
    }

    pub fn has_matches(&self) -> bool {
        !self.matches.is_empty()
    }

    /// Re-run the query over `messages`.
    ///
    /// Keeps the reader where they were when it can: narrowing a query
    /// usually leaves the current hit in the result, and jumping them back to
    /// the top of the conversation for a typed character is disorienting.
    pub fn refresh(&mut self, query: &str, messages: &[ChatMessage]) {
        let previous = self.current_match().map(str::to_string);
        self.query = query.trim().to_lowercase();
        self.matches = if self.query.is_empty() {
            Vec::new()
        } else {
            messages
                .iter()
                .filter(|message| matches(message, &self.query))
                .map(|message| message.id.clone())
                .collect()
        };
        self.current = previous
            .and_then(|id| self.matches.iter().position(|candidate| *candidate == id))
            // Otherwise the newest match, which is the one nearest where the
            // reader already is.
            .unwrap_or(self.matches.len().saturating_sub(1));
    }

    /// Step to the next match, wrapping. Returns the id to jump to.
    pub fn step(&mut self, forward: bool) -> Option<&str> {
        if self.matches.is_empty() {
            return None;
        }
        let last = self.matches.len() - 1;
        self.current = if forward {
            if self.current >= last {
                0
            } else {
                self.current + 1
            }
        } else if self.current == 0 {
            last
        } else {
            self.current - 1
        };
        self.current_match()
    }
}

/// Whether a message answers `query`, which is already lowercased.
///
/// Captions and file names count: a photo is often remembered by what was
/// said about it, and a document by what it was called.
fn matches(message: &ChatMessage, query: &str) -> bool {
    if message.content.to_lowercase().contains(query) {
        return true;
    }
    message.media.as_ref().is_some_and(|media| {
        media
            .file_name
            .as_ref()
            .is_some_and(|name| name.to_lowercase().contains(query))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidezap_core::fixtures;

    /// A [`Search`] with the conversation half open over `jid`, and no window
    /// behind it. What the tests below drive is the part that decides — when
    /// an open search has gone stale, and what a departing account leaves.
    fn searching(jid: &str, query: &str) -> Search {
        let mut search = Search::new();
        let mut open = ConversationSearch::new(jid.to_string());
        open.refresh(query, &history());
        search.conversation = Some(open);
        search
    }

    /// Every path that invalidates a chat's timeline asks this, on every
    /// chat: the answer has to be cheap and it has to be about *this*
    /// conversation, or a message arriving in one chat would re-run the
    /// search open over another.
    #[test]
    fn only_the_searched_conversation_goes_stale() {
        let search = searching("chat", "invoice");

        assert!(search.stale_for("chat"));
        assert!(!search.stale_for("someone-else"));
    }

    /// An empty query has no matches to go stale, and re-running it on every
    /// message that arrives is a walk over the whole conversation for a
    /// result that is already empty.
    #[test]
    fn a_search_nobody_has_typed_into_yet_is_never_stale() {
        let search = searching("chat", "   ");

        assert!(!search.stale_for("chat"));
    }

    /// The list filter is the one thing here that changes what the sidebar
    /// draws, and an empty list means "no matches" only while it is set.
    #[test]
    fn the_list_is_narrowed_only_while_a_query_is_held() {
        let mut search = Search::new();
        assert!(!search.narrowing_the_list());

        search.list_query = "invoice".to_string();

        assert!(search.narrowing_the_list());
    }

    fn history() -> Vec<ChatMessage> {
        vec![
            fixtures::message("1", fixtures::PEER, "the invoice is attached"),
            fixtures::message("2", fixtures::PEER, "thanks"),
            fixtures::message("3", fixtures::PEER, "Invoice again, sorry"),
        ]
    }

    #[test]
    fn matching_ignores_case_and_keeps_timeline_order() {
        let mut search = ConversationSearch::new("chat".into());
        search.refresh("INVOICE", &history());
        assert_eq!(search.matches, vec!["1".to_string(), "3".to_string()]);
    }

    #[test]
    fn the_newest_match_is_the_one_shown_first() {
        let mut search = ConversationSearch::new("chat".into());
        search.refresh("invoice", &history());
        assert_eq!(search.current_match(), Some("3"));
        assert_eq!(search.status().as_deref(), Some("2 of 2"));
    }

    #[test]
    fn stepping_wraps_in_both_directions() {
        let mut search = ConversationSearch::new("chat".into());
        search.refresh("invoice", &history());
        assert_eq!(search.step(true), Some("1"), "forward from the last wraps");
        assert_eq!(search.step(false), Some("3"), "and back again");
    }

    /// Typing one more character must not throw the reader back to the top.
    #[test]
    fn narrowing_a_query_keeps_the_current_hit() {
        let history = history();
        let mut search = ConversationSearch::new("chat".into());
        search.refresh("invoice", &history);
        search.step(true);
        assert_eq!(search.current_match(), Some("1"));

        search.refresh("invoice is", &history);
        assert_eq!(search.current_match(), Some("1"));
    }

    #[test]
    fn an_empty_query_says_nothing_rather_than_no_matches() {
        let mut search = ConversationSearch::new("chat".into());
        search.refresh("   ", &history());
        assert!(search.status().is_none());
        assert!(!search.has_matches());
    }

    #[test]
    fn a_query_nothing_answers_says_so() {
        let mut search = ConversationSearch::new("chat".into());
        search.refresh("receipt", &history());
        assert_eq!(
            search.status().as_deref(),
            Some("No matches in the loaded messages")
        );
    }
}
