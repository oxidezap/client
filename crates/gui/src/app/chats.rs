//! Chat list state: filtering, and the per-frame row snapshot.

use std::collections::HashMap;
use std::sync::Arc;

use gpui::{App, Context};
use oxidezap_core::Chat;

use super::chat_row::ChatRow;
use super::{WhatsAppApp, newest_shared_message};
use crate::utils::contains_ignore_case;
use log::info;
use wacore_binary::jid::observe_str;

/// Which conversations the sidebar is showing.
///
/// A filter is part of the information model, not a view detail: the list
/// being short has to be explainable, so the active filter stays visible and
/// an empty result offers the way back to `All`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChatFilter {
    #[default]
    All,
    Unread,
    Groups,
    Archived,
}

impl ChatFilter {
    pub const ALL: [Self; 4] = [Self::All, Self::Unread, Self::Groups, Self::Archived];

    pub fn id(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Unread => "unread",
            Self::Groups => "groups",
            Self::Archived => "archived",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Unread => "Unread",
            Self::Groups => "Groups",
            Self::Archived => "Archived",
        }
    }

    /// Whether `chat` belongs under this filter.
    pub fn matches(self, chat: &Chat) -> bool {
        match self {
            Self::All => !chat.archived,
            Self::Unread => !chat.archived && (chat.unread_count > 0 || chat.manually_unread),
            Self::Groups => !chat.archived && chat.is_group,
            Self::Archived => chat.archived,
        }
    }
}

/// What a complete store load says about a chat already on screen.
///
/// A complete load is the store's whole truth about the rows it has, so a
/// active store-backed chat missing from one was archived or deleted —
/// possibly on another device — and has to leave the main window too. An
/// explicitly archived chat is outside that load's scope and stays available
/// to the Archived filter. Two more things stop absence being a plain removal:
/// a live-only chat was never in the store to be missing from it (during
/// pairing the store is empty while live messages already populate the UI),
/// and the conversation being *read* is not yanked out from under its reader.
///
/// Read, not merely selected. The selection is deliberately kept while the
/// window is in Status, in Settings, under the fullscreen viewer or, on a
/// phone, walking the chat list — so sparing on the selection spared a chat
/// nobody was looking at, and left it in the sidebar until some other
/// conversation was picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Survival {
    /// Still a chat this window should show.
    Keep,
    /// Gone from the store, but on screen. Kept for now and owed a removal
    /// the moment it stops being drawn.
    Defer,
    /// Gone, and nobody is looking at it.
    Drop,
}

/// Apply that rule to one chat.
pub fn survives_complete_load(
    chat: &Chat,
    loaded: &std::collections::HashSet<&str>,
    visible: Option<&str>,
) -> Survival {
    // The ordinary complete load is complete only for the non-archived
    // list. Once an archived page has installed a row, absence from that
    // active load says nothing about it and must not erase the explicit list.
    if chat.archived || !chat.is_from_store() || loaded.contains(chat.jid.as_str()) {
        Survival::Keep
    } else if visible == Some(chat.jid.as_str()) {
        Survival::Defer
    } else {
        Survival::Drop
    }
}

/// What a complete include-archived scan says about rows already held.
///
/// Unlike the active attach load, reaching the final page covers archived
/// rows too. An archived, store-backed row absent from `loaded` was deleted;
/// an active or live-only row is outside this scan's authority.
pub fn survives_archived_scan(
    chat: &Chat,
    loaded: &std::collections::HashSet<String>,
    visible: Option<&str>,
) -> Survival {
    if !chat.archived || !chat.is_from_store() || loaded.contains(&chat.jid) {
        Survival::Keep
    } else if visible == Some(chat.jid.as_str()) {
        Survival::Defer
    } else {
        Survival::Drop
    }
}

/// The sidebar order: pinned chats lead, most recently pinned first; the
/// rest follow by activity; ties by JID descending, so every load renders
/// the same order.
///
/// Descending, because that is the store's order: `ChatStore::chats_page`
/// ranks equal `(pinned_at, last_message_ts)` rows by `jid DESC` and walks
/// on with `jid < cursor.jid`. An ascending tie-breaker here sorted a tied
/// row from the next page ahead of the rows the previous page had already
/// drawn, instead of appending it behind them — and message timestamps are
/// second-granular, so ties are ordinary.
///
/// One function because three paths maintain it: the merge sort below, and
/// the live insertion and repositioning in `super`, which ordered by
/// activity alone and let a live message stand an unpinned chat above an
/// older pinned one until the next load sorted them again.
pub(super) fn chat_list_order(a: &Chat, b: &Chat) -> std::cmp::Ordering {
    b.pinned_at
        .cmp(&a.pinned_at)
        .then_with(|| b.last_message_time.cmp(&a.last_message_time))
        .then_with(|| b.jid.cmp(&a.jid))
}

/// The conversation list as one frame will draw it.
///
/// Rows are derived once and shared, rather than recomputed per visible item:
/// the virtual list rebuilds its range on every scroll, and working out a
/// preview per row per frame is exactly the kind of work that makes scrolling
/// feel heavy.
#[derive(Clone)]
pub struct ChatListCache {
    /// What [`WhatsAppApp::invalidate_chat_cache`] had been called this many
    /// times when the snapshot was taken.
    ///
    /// The whole point of asking this rather than comparing counts: a count
    /// can only be compared after the list has been filtered, which is where
    /// the work is: a `to_lowercase` of the name and of the JID per chat
    /// while a search is running, spent on every frame to conclude that
    /// nothing had changed.
    pub version: u64,
    /// How many chats there were in total, unfiltered. `Vec::len`, so it is
    /// free, and it catches an addition or a removal that reached the list
    /// without announcing itself.
    pub chats_len: usize,
    pub rows: Arc<[ChatRow]>,
}

impl WhatsAppApp {
    /// Fold hydrated chats into the list.
    ///
    /// The shared half of every read that produces chats — a store load, a
    /// page of the list, the rows a snapshot painted — because all three
    /// arrive at a list that may already hold the same conversation. Merging
    /// rather than replacing is what keeps a live bubble that has not reached
    /// the store yet, and what spends the reads a row without messages could
    /// not bound.
    ///
    /// Never prunes: absence is a claim only a complete load may make, and
    /// only the caller knows whether this was one.
    pub(super) fn merge_chats(&mut self, chats: Vec<Chat>, cx: &mut App) {
        // An index, not a scan per incoming chat. A page is a hundred chats
        // and an account is thousands, so the search alone was hundreds of
        // thousands of string comparisons per load, and a history sync
        // commits these back to back for minutes.
        let mut index: HashMap<String, usize> = self
            .chats
            .iter()
            .enumerate()
            .map(|(at, chat)| (chat.jid.clone(), at))
            .collect();
        for chat in chats {
            match index.get(&chat.jid).copied() {
                Some(at) => {
                    let jid = chat.jid.clone();
                    Arc::make_mut(&mut self.chats[at]).merge_history(chat);
                    // The chat *on screen* was read locally the moment the
                    // message arrived; the store row commits with the unread
                    // bump before our receipt lands, so the hydrated counter
                    // must not resurrect the badge. On screen, not selected —
                    // the same distinction the live arrival makes, and for the
                    // same reason: a reload while the reader is in Status
                    // would otherwise clear the badge of a conversation nobody
                    // was looking at.
                    if self.window_focused
                        && crate::platform::application_is_active()
                        && self.visible_chat.as_deref() == Some(jid.as_str())
                    {
                        Arc::make_mut(&mut self.chats[at]).mark_as_read();
                    }
                    // The read a row without messages could not bound. Spent
                    // here because this is what gave it a message to name; see
                    // `owed_reads`.
                    if self.window_focused
                        && crate::platform::application_is_active()
                        && self.visible_chat.as_deref() == Some(jid.as_str())
                        && self.owed_reads.contains(&jid)
                        && let Some(newest) = newest_shared_message(&self.chats[at])
                    {
                        self.owed_reads.remove(&jid);
                        if let Some(client) = &self.client {
                            info!(
                                "Marking {} read, now that it has messages",
                                observe_str(&jid)
                            );
                            client.mark_chat_read(&jid, Some(newest));
                        }
                    }
                    self.invalidate_message_cache(&jid, cx);
                }
                None => {
                    // Into the index too, or the same JID twice in one batch
                    // would be pushed twice: the scan this replaces found the
                    // first of them.
                    index.insert(chat.jid.clone(), self.chats.len());
                    self.chats.push(Arc::new(chat));
                }
            }
        }
        self.chats.sort_by(|a, b| chat_list_order(a, b));
    }

    /// Put chats into the list, with everything that installing them owes.
    ///
    /// One entrance for both: a history load and a page of the sidebar. The
    /// page went through `merge_chats` alone, so a notice waiting for a group
    /// that only ever arrives by page stayed parked — and a notice lives in
    /// no store, so "forever" is not a figure of speech — while the status
    /// tick was never armed for a broadcast that arrived the same way.
    ///
    /// `watched` are the status updates this load says are read, which only a
    /// load can know: a merge keeps the row this window marked, and afterwards
    /// the two are indistinguishable.
    pub(super) fn install_chats(
        &mut self,
        chats: Vec<Chat>,
        watched: &std::collections::HashSet<String>,
        cx: &mut Context<Self>,
    ) {
        self.merge_chats(chats, cx);
        // A selection that no longer names a chat is a selection of nothing:
        // the conversation pane resolves it every frame and would draw the
        // empty state with no way back on a phone.
        self.forget_missing_selection(cx);
        // The merge above took the store's word for every row, and a merge
        // assembled before a view was written does not carry it.
        self.restore_watched_status(watched);
        // Count-based cache guards cannot see reordering or merges.
        self.invalidate_chat_cache();
        // Whatever arrived before its conversation did.
        self.flush_pending_notices(cx);
        // A status update expires on the clock with nothing arriving to say
        // so, and this is where the feed that holds one is installed.
        self.ensure_status_tick(cx);
    }
}

/// Flatten the authoritative community relationships for the Groups filter.
/// A child is nested only when its parent JID is present and explicitly marked
/// as a community; absent/unknown parents leave the subgroup visible at the
/// root instead of guessing from a matching name. A pinned subgroup whose
/// parent is unpinned stays at the root in the pinned block rather than being
/// demoted below that parent.
pub(super) fn hierarchical_group_rows(
    rows: &[ChatRow],
    query: &str,
    collapsed: &std::collections::HashSet<String>,
) -> Vec<ChatRow> {
    use oxidezap_core::GroupHierarchy;

    let query_active = !query.trim().is_empty();
    let matches = |row: &ChatRow| {
        contains_ignore_case(&row.name, query) || contains_ignore_case(&row.jid, query)
    };
    let by_jid: HashMap<&str, &ChatRow> = rows.iter().map(|row| (row.jid.as_str(), row)).collect();
    let mut children: HashMap<&str, Vec<&ChatRow>> = HashMap::new();
    let mut linked_children: HashMap<&str, Vec<&ChatRow>> = HashMap::new();
    let mut nested = std::collections::HashSet::new();
    for row in rows {
        let Some(GroupHierarchy::Subgroup { parent_jid, .. }) = &row.group_hierarchy else {
            continue;
        };
        let Some(parent) = by_jid.get(parent_jid.as_str()).filter(|parent| {
            parent_jid != &row.jid
                && parent
                    .group_hierarchy
                    .as_ref()
                    .is_some_and(|hierarchy| matches!(hierarchy, GroupHierarchy::Community))
        }) else {
            continue;
        };
        linked_children.entry(parent_jid).or_default().push(row);
        // Input rows are sorted with pinned chats first. Never nest across
        // that boundary: an unpinned child under a pinned parent can otherwise
        // jump ahead of later pinned rows. Keep the relationship for search
        // context, while rendering cross-boundary children as roots.
        if row.pinned != parent.pinned {
            continue;
        }
        children.entry(parent_jid).or_default().push(row);
        nested.insert(row.jid.as_str());
    }

    let mut flattened = Vec::with_capacity(rows.len());
    for row in rows {
        if nested.contains(row.jid.as_str()) {
            continue;
        }
        if row
            .group_hierarchy
            .as_ref()
            .is_some_and(|hierarchy| matches!(hierarchy, GroupHierarchy::Community))
        {
            let members = children
                .get(row.jid.as_str())
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let matching_child = linked_children
                .get(row.jid.as_str())
                .is_some_and(|children| children.iter().any(|child| matches(child)));
            let row_matches = matches(row);
            if !row_matches && !matching_child {
                continue;
            }
            let expanded = if query_active {
                matching_child
            } else {
                !collapsed.contains(&row.jid)
            };
            let mut parent = row.clone();
            // Search expands matching paths automatically, so a disclosure
            // control that could not hide a matching result would be a no-op.
            parent.community_toggle_visible = !members.is_empty() && !query_active;
            parent.community_expanded = expanded;
            parent.hierarchy_context = !row_matches;
            flattened.push(parent);
            if expanded {
                flattened.extend(
                    members
                        .iter()
                        .filter(|child| !query_active || matches(child))
                        .map(|child| {
                            let mut child = (*child).clone();
                            child.tree_depth = 1;
                            child
                        }),
                );
            }
        } else if matches(row) {
            flattened.push(row.clone());
        }
    }
    flattened
}

/// The keyboard disclosure action applies only when the selected cached row
/// currently exposes a community toggle.
pub(super) fn selected_community_toggle(rows: &[ChatRow], selected_jid: &str) -> Option<String> {
    rows.iter()
        .find(|row| row.jid == selected_jid && row.community_toggle_visible)
        .map(|row| row.jid.clone())
}

/// A selected subgroup hidden by its collapsed parent keeps that parent as the
/// list's visible selection anchor without changing the open conversation.
fn visible_chat_list_selection(rows: &[ChatRow], selected: &Chat) -> Option<String> {
    if rows.iter().any(|row| row.jid == selected.jid) {
        return Some(selected.jid.clone());
    }
    let oxidezap_core::GroupHierarchy::Subgroup { parent_jid, .. } =
        selected.group_hierarchy.as_ref()?
    else {
        return None;
    };
    rows.iter()
        .find(|row| {
            row.jid == *parent_jid && row.community_toggle_visible && !row.community_expanded
        })
        .map(|row| row.jid.clone())
}

impl WhatsAppApp {
    /// The row the chat list can visibly highlight for the current
    /// conversation, including a collapsed subgroup's community parent.
    pub(crate) fn chat_list_selection_jid(&self, cache: &ChatListCache) -> Option<String> {
        let selected = self
            .selected_chat
            .as_deref()
            .and_then(|jid| self.find_chat(jid))?;
        visible_chat_list_selection(&cache.rows, selected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(jid: &str, is_group: bool, unread: u32, marked: bool) -> Chat {
        let mut chat = Chat::new(jid.to_string());
        chat.is_group = is_group;
        chat.unread_count = unread;
        chat.manually_unread = marked;
        chat
    }

    #[test]
    fn all_keeps_everything() {
        assert!(ChatFilter::All.matches(&chat("a@s.whatsapp.net", false, 0, false)));
        assert!(ChatFilter::All.matches(&chat("g@g.us", true, 0, false)));
    }

    #[test]
    fn unread_counts_both_kinds_of_unread() {
        assert!(ChatFilter::Unread.matches(&chat("a@s.whatsapp.net", false, 3, false)));
        assert!(
            ChatFilter::Unread.matches(&chat("a@s.whatsapp.net", false, 0, true)),
            "marked unread by hand is still unread"
        );
        assert!(!ChatFilter::Unread.matches(&chat("a@s.whatsapp.net", false, 0, false)));
    }

    fn hierarchy_row(jid: &str, name: &str, hierarchy: oxidezap_core::GroupHierarchy) -> ChatRow {
        let mut chat = Chat::new(jid.to_string());
        chat.name = name.to_string();
        chat.is_group = true;
        chat.group_hierarchy = Some(hierarchy);
        ChatRow::new(&chat, None, None, false)
    }

    #[test]
    fn subgroup_parentage_uses_jids_and_keeps_same_named_communities_separate() {
        use oxidezap_core::{GroupHierarchy, SubgroupKind};

        let rows = vec![
            hierarchy_row(
                "g2@g.us",
                "Announcements",
                GroupHierarchy::Subgroup {
                    parent_jid: "community2@g.us".into(),
                    kind: SubgroupKind::Announcement,
                },
            ),
            hierarchy_row("community1@g.us", "Sports", GroupHierarchy::Community),
            hierarchy_row("community2@g.us", "Sports", GroupHierarchy::Community),
            hierarchy_row(
                "g1@g.us",
                "Announcements",
                GroupHierarchy::Subgroup {
                    parent_jid: "community1@g.us".into(),
                    kind: SubgroupKind::General,
                },
            ),
            hierarchy_row("standalone@g.us", "Sports", GroupHierarchy::Standalone),
        ];
        let rows = hierarchical_group_rows(&rows, "", &Default::default());

        assert_eq!(
            rows.iter().map(|row| row.jid.as_str()).collect::<Vec<_>>(),
            vec![
                "community1@g.us",
                "g1@g.us",
                "community2@g.us",
                "g2@g.us",
                "standalone@g.us",
            ]
        );
        assert!(rows[0].community_toggle_visible);
        assert_eq!(rows[1].tree_depth, 1);
        assert_eq!(rows[1].hierarchy_label, Some("General"));
        assert_eq!(rows[3].hierarchy_label, Some("Announcement"));
        assert_eq!(rows[4].tree_depth, 0);
    }

    #[test]
    fn subgroup_search_reveals_a_collapsed_parent_as_context() {
        use oxidezap_core::{GroupHierarchy, SubgroupKind};

        let rows = vec![
            hierarchy_row("community@g.us", "Games", GroupHierarchy::Community),
            hierarchy_row(
                "subgroup@g.us",
                "Chess",
                GroupHierarchy::Subgroup {
                    parent_jid: "community@g.us".into(),
                    kind: SubgroupKind::Regular,
                },
            ),
        ];
        let collapsed = std::collections::HashSet::from(["community@g.us".to_string()]);
        let rows = hierarchical_group_rows(&rows, "chess", &collapsed);

        assert_eq!(rows.len(), 2, "search opens the path to a matching child");
        assert_eq!(rows[0].jid, "community@g.us");
        assert!(rows[0].hierarchy_context);
        assert!(rows[0].community_expanded);
        assert!(
            !rows[0].community_toggle_visible,
            "a matching path is expanded by search rather than a manual toggle"
        );
        assert_eq!(rows[1].jid, "subgroup@g.us");
        assert_eq!(rows[1].tree_depth, 1);
    }

    #[test]
    fn collapsing_a_community_hides_only_its_jid_linked_subgroups() {
        use oxidezap_core::{GroupHierarchy, SubgroupKind};

        let rows = vec![
            hierarchy_row("community1@g.us", "First", GroupHierarchy::Community),
            hierarchy_row(
                "child1@g.us",
                "First child",
                GroupHierarchy::Subgroup {
                    parent_jid: "community1@g.us".into(),
                    kind: SubgroupKind::Regular,
                },
            ),
            hierarchy_row("community2@g.us", "Second", GroupHierarchy::Community),
            hierarchy_row(
                "child2@g.us",
                "Second child",
                GroupHierarchy::Subgroup {
                    parent_jid: "community2@g.us".into(),
                    kind: SubgroupKind::Regular,
                },
            ),
        ];
        let collapsed = std::collections::HashSet::from(["community1@g.us".to_string()]);
        let rows = hierarchical_group_rows(&rows, "", &collapsed);
        assert_eq!(
            rows.iter().map(|row| row.jid.as_str()).collect::<Vec<_>>(),
            vec!["community1@g.us", "community2@g.us", "child2@g.us"]
        );
        assert!(!rows[0].community_expanded);
        assert!(rows[1].community_expanded);
        assert_eq!(rows[2].tree_depth, 1);
    }

    #[test]
    fn a_collapsed_selected_subgroup_uses_its_parent_as_list_selection_anchor() {
        use oxidezap_core::{GroupHierarchy, SubgroupKind};
        use std::collections::HashSet;

        let source_rows = vec![
            hierarchy_row("community@g.us", "Community", GroupHierarchy::Community),
            hierarchy_row(
                "subgroup@g.us",
                "Subgroup",
                GroupHierarchy::Subgroup {
                    parent_jid: "community@g.us".into(),
                    kind: SubgroupKind::Regular,
                },
            ),
        ];
        let mut selected = chat("subgroup@g.us", true, 0, false);
        selected.group_hierarchy = Some(GroupHierarchy::Subgroup {
            parent_jid: "community@g.us".into(),
            kind: SubgroupKind::Regular,
        });
        let expanded_rows = hierarchical_group_rows(&source_rows, "", &HashSet::new());
        assert_eq!(
            visible_chat_list_selection(&expanded_rows, &selected),
            Some("subgroup@g.us".into())
        );
        let collapsed_rows =
            hierarchical_group_rows(&source_rows, "", &HashSet::from(["community@g.us".into()]));
        let visible_selection = visible_chat_list_selection(&collapsed_rows, &selected)
            .expect("collapsed parent remains selected in the list");
        assert_eq!(visible_selection, "community@g.us");
        assert_eq!(
            selected_community_toggle(&collapsed_rows, &visible_selection),
            Some("community@g.us".into())
        );
    }

    #[test]
    fn keyboard_disclosure_targets_only_a_visible_community_row() {
        use oxidezap_core::{GroupHierarchy, SubgroupKind};

        let rows = vec![
            hierarchy_row("community@g.us", "Games", GroupHierarchy::Community),
            hierarchy_row(
                "subgroup@g.us",
                "Chess",
                GroupHierarchy::Subgroup {
                    parent_jid: "community@g.us".into(),
                    kind: SubgroupKind::Regular,
                },
            ),
        ];
        let rows = hierarchical_group_rows(&rows, "", &Default::default());
        assert_eq!(
            selected_community_toggle(&rows, "community@g.us"),
            Some("community@g.us".into())
        );
        assert_eq!(selected_community_toggle(&rows, "subgroup@g.us"), None);
    }

    #[test]
    fn a_pinned_subgroup_stays_in_the_pinned_block_when_its_parent_is_unpinned() {
        use oxidezap_core::{GroupHierarchy, SubgroupKind};

        let mut pinned_child = hierarchy_row(
            "pinned-child@g.us",
            "Pinned subgroup",
            GroupHierarchy::Subgroup {
                parent_jid: "community@g.us".into(),
                kind: SubgroupKind::Regular,
            },
        );
        pinned_child.pinned = true;
        let rows = vec![
            pinned_child,
            hierarchy_row("standalone@g.us", "Standalone", GroupHierarchy::Standalone),
            hierarchy_row("community@g.us", "Community", GroupHierarchy::Community),
        ];

        let rows = hierarchical_group_rows(&rows, "", &Default::default());
        assert_eq!(
            rows.iter().map(|row| row.jid.as_str()).collect::<Vec<_>>(),
            vec!["pinned-child@g.us", "standalone@g.us", "community@g.us",]
        );
        assert_eq!(rows[0].tree_depth, 0);
        assert!(!rows[2].community_toggle_visible);

        let searched = hierarchical_group_rows(&rows, "pinned subgroup", &Default::default());
        assert_eq!(
            searched
                .iter()
                .map(|row| row.jid.as_str())
                .collect::<Vec<_>>(),
            vec!["pinned-child@g.us", "community@g.us"]
        );
        assert!(searched[1].hierarchy_context);
    }

    #[test]
    fn an_unpinned_subgroup_does_not_interrupt_the_pinned_block() {
        use oxidezap_core::{GroupHierarchy, SubgroupKind};

        let mut community = hierarchy_row("community@g.us", "Community", GroupHierarchy::Community);
        community.pinned = true;
        let mut other_pinned = hierarchy_row("pinned@g.us", "Pinned", GroupHierarchy::Standalone);
        other_pinned.pinned = true;
        let child = hierarchy_row(
            "child@g.us",
            "Child",
            GroupHierarchy::Subgroup {
                parent_jid: "community@g.us".into(),
                kind: SubgroupKind::Regular,
            },
        );
        let rows =
            hierarchical_group_rows(&[community, other_pinned, child], "", &Default::default());

        assert_eq!(
            rows.iter().map(|row| row.jid.as_str()).collect::<Vec<_>>(),
            vec!["community@g.us", "pinned@g.us", "child@g.us"]
        );
        assert!(rows.iter().all(|row| row.tree_depth == 0));
    }

    #[test]
    fn a_missing_parent_is_not_guessed_from_a_matching_name() {
        use oxidezap_core::{GroupHierarchy, SubgroupKind};

        let rows = vec![
            hierarchy_row("unrelated@g.us", "Same name", GroupHierarchy::Community),
            hierarchy_row(
                "child@g.us",
                "Same name",
                GroupHierarchy::Subgroup {
                    parent_jid: "missing@g.us".into(),
                    kind: SubgroupKind::Other,
                },
            ),
        ];
        let rows = hierarchical_group_rows(&rows, "", &Default::default());
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.tree_depth == 0));
        assert!(!rows[0].community_toggle_visible);
    }

    #[test]
    fn groups_excludes_direct_chats() {
        assert!(ChatFilter::Groups.matches(&chat("g@g.us", true, 0, false)));
        assert!(!ChatFilter::Groups.matches(&chat("a@s.whatsapp.net", false, 9, false)));
    }

    #[test]
    fn archived_is_a_separate_list_for_direct_chats_and_groups() {
        let mut direct = chat("a@s.whatsapp.net", false, 0, false);
        direct.archived = true;
        let mut group = chat("g@g.us", true, 0, false);
        group.archived = true;

        assert!(ChatFilter::Archived.matches(&direct));
        assert!(ChatFilter::Archived.matches(&group));
        assert!(!ChatFilter::All.matches(&direct));
        assert!(!ChatFilter::Unread.matches(&direct));
        assert!(!ChatFilter::Groups.matches(&group));
    }

    fn from_store(jid: &str) -> Chat {
        Chat::from_store(jid.to_string(), "Someone".to_string(), 0)
    }

    #[test]
    fn a_chat_the_store_still_has_stays() {
        let loaded = std::collections::HashSet::from(["a@s.whatsapp.net"]);
        assert_eq!(
            survives_complete_load(&from_store("a@s.whatsapp.net"), &loaded, None),
            Survival::Keep
        );
    }

    #[test]
    fn a_live_only_chat_is_not_the_stores_to_delete() {
        let loaded = std::collections::HashSet::new();
        assert_eq!(
            survives_complete_load(&chat("a@s.whatsapp.net", false, 0, false), &loaded, None),
            Survival::Keep,
            "during pairing the store is empty while live messages already exist"
        );
    }

    #[test]
    fn a_stored_chat_missing_from_a_complete_load_is_gone() {
        let loaded = std::collections::HashSet::from(["b@s.whatsapp.net"]);
        assert_eq!(
            survives_complete_load(&from_store("a@s.whatsapp.net"), &loaded, None),
            Survival::Drop
        );
    }

    #[test]
    fn an_archived_chat_survives_the_active_lists_complete_load() {
        let mut archived = from_store("a@s.whatsapp.net");
        archived.archived = true;
        assert_eq!(
            survives_complete_load(&archived, &std::collections::HashSet::new(), None),
            Survival::Keep
        );
    }

    #[test]
    fn a_complete_archived_scan_drops_only_missing_archived_rows() {
        let loaded = std::collections::HashSet::from(["kept@s.whatsapp.net".to_string()]);
        let mut kept = from_store("kept@s.whatsapp.net");
        kept.archived = true;
        let mut deleted = from_store("deleted@s.whatsapp.net");
        deleted.archived = true;
        let active = from_store("active@s.whatsapp.net");

        assert_eq!(survives_archived_scan(&kept, &loaded, None), Survival::Keep);
        assert_eq!(
            survives_archived_scan(&deleted, &loaded, None),
            Survival::Drop
        );
        assert_eq!(
            survives_archived_scan(&deleted, &loaded, Some("deleted@s.whatsapp.net")),
            Survival::Defer
        );
        assert_eq!(
            survives_archived_scan(&active, &loaded, None),
            Survival::Keep
        );
    }

    #[test]
    fn the_conversation_on_screen_is_spared_but_owed_a_removal() {
        let loaded = std::collections::HashSet::from(["b@s.whatsapp.net"]);
        assert_eq!(
            survives_complete_load(
                &from_store("a@s.whatsapp.net"),
                &loaded,
                Some("a@s.whatsapp.net")
            ),
            Survival::Defer,
            "spared only because it is being read — not forgiven"
        );
    }

    #[test]
    fn a_chat_nobody_is_looking_at_goes_even_if_it_is_selected() {
        let loaded = std::collections::HashSet::from(["b@s.whatsapp.net"]);
        assert_eq!(
            survives_complete_load(&from_store("a@s.whatsapp.net"), &loaded, None),
            Survival::Drop,
            "the selection survives a trip to Status; being drawn does not"
        );
    }

    #[test]
    fn filter_ids_are_stable_and_distinct() {
        let ids: Vec<&str> = ChatFilter::ALL.iter().map(|f| f.id()).collect();
        assert_eq!(ids, vec!["all", "unread", "groups", "archived"]);
    }

    fn tied_chat(jid: &str, pin_secs: Option<i64>, secs: Option<i64>) -> Chat {
        let at = |secs: i64| chrono::DateTime::from_timestamp(secs, 0);
        let mut chat = Chat::new(jid.to_string());
        chat.pinned_at = pin_secs.and_then(at);
        chat.last_message_time = secs.and_then(at);
        chat
    }

    /// The store pages ties by JID descending (`jid < cursor.jid`), so the
    /// list must too: an ascending tie-breaker sorted a tied row from the
    /// next page ahead of the previous page's rows instead of appending it
    /// behind them. Message timestamps are second-granular, so ties are
    /// ordinary — and both runs (pinned and activity) share the rule.
    #[test]
    fn tied_chats_sort_like_the_store_page() {
        for (pin_secs, secs) in [
            (Some(1_700_000_100), Some(1_700_000_000)),
            (None, Some(1_700_000_000)),
        ] {
            let lower = tied_chat("559900000001@s.whatsapp.net", pin_secs, secs);
            let higher = tied_chat("559900000002@s.whatsapp.net", pin_secs, secs);
            assert_eq!(
                chat_list_order(&lower, &higher),
                std::cmp::Ordering::Greater,
                "the higher JID leads on a tie (pin {pin_secs:?})"
            );
            let mut chats = [lower.clone(), higher.clone()];
            chats.sort_by(chat_list_order);
            assert_eq!(
                chats
                    .iter()
                    .map(|chat| chat.jid.as_str())
                    .collect::<Vec<_>>(),
                vec!["559900000002@s.whatsapp.net", "559900000001@s.whatsapp.net"]
            );
        }
    }

    /// A stopwatch rather than an assertion: what finding a page's chats in
    /// the list costs, scanned against indexed.
    ///
    /// `merge_chats` searched linearly per incoming chat, so a page of 100
    /// over an account of 3000 was 300k string comparisons, and a history
    /// sync commits pages like this back to back for minutes.
    ///
    /// `cargo test -p oxidezap-gui -- --ignored --nocapture chat_merge_lookup_costs`
    #[test]
    #[ignore = "a measurement, not an assertion"]
    fn chat_merge_lookup_costs() {
        const HELD: usize = 3_000;
        const PAGE: usize = 100;

        let held: Vec<String> = (0..HELD)
            .map(|i| format!("55990000{i:04}@s.whatsapp.net"))
            .collect();
        // The page is the oldest end of the list, which is where a backfill
        // lands and where a scan from the front pays the most.
        let page: Vec<String> = held.iter().rev().take(PAGE).cloned().collect();

        let started = wacore::time::Instant::now();
        let mut found = 0;
        for jid in &page {
            found += usize::from(held.iter().any(|held| held == jid));
        }
        let scanning = started.elapsed();

        let started = wacore::time::Instant::now();
        let index: HashMap<&str, usize> = held
            .iter()
            .enumerate()
            .map(|(at, jid)| (jid.as_str(), at))
            .collect();
        let mut indexed = 0;
        for jid in &page {
            indexed += usize::from(index.contains_key(jid.as_str()));
        }
        let hashing = started.elapsed();

        assert_eq!(found, indexed, "the two answer the same");
        println!("{HELD} chats, a page of {PAGE}: scanned {scanning:?}, indexed {hashing:?}");
    }

    /// A stopwatch rather than an assertion: what the sidebar's search filter
    /// costs for one pass over an account's chats.
    ///
    /// The number that matters is how often it is paid. `get_chat_list_cache`
    /// is called at least twice a frame and used to run this before it could
    /// compare the count it produced, so an idle window at 60 fps spent it
    /// 120 times a second on a list nothing had touched. With the cache
    /// answering from a version and a length, both O(1), an unchanged list
    /// pays it zero times.
    ///
    /// `cargo test -p oxidezap-gui -- --ignored --nocapture chat_filter_costs`
    #[test]
    #[ignore = "a measurement, not an assertion"]
    fn chat_filter_costs() {
        const CHATS: usize = 1_000;
        const PASSES: usize = 100;

        let chats: Vec<Chat> = (0..CHATS)
            .map(|i| {
                let mut chat = Chat::new(format!("55990000{i:04}@s.whatsapp.net"));
                chat.name = format!("Contact {i}");
                chat
            })
            .collect();
        let query = "contact 9";

        let started = wacore::time::Instant::now();
        let mut kept = 0;
        for _ in 0..PASSES {
            kept += chats
                .iter()
                .filter(|chat| {
                    chat.name.to_lowercase().contains(query)
                        || chat.jid.to_lowercase().contains(query)
                })
                .count();
        }
        let elapsed = started.elapsed();
        println!(
            "{CHATS} chats, {PASSES} passes: {elapsed:?} ({:?} per pass, {kept} kept)",
            elapsed / PASSES as u32
        );
    }
}
