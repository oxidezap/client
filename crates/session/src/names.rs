//! One place a person becomes a label.
//!
//! WhatsApp answers "who is this" three ways and they do not agree: the
//! address book this account synced, the push name the sender chose for
//! themselves, and the number. Every surface that names somebody — a group
//! bubble, the typing row, a chat's title, a quote bar — has to pick the
//! same one, or the same person shows up twice under two names in the same
//! window. That choice is made here, once, in one order: the address book,
//! then the push name, then the number.
//!
//! It is also where the lookups are paid for. A name costs two queries (the
//! PN/LID pair behind a JID, then the contact row), and a group page names
//! the same handful of people over and over, so both are memoized per JID —
//! misses included, which are the common case for a stranger in a large
//! group. A full history reload is what re-reads the address book, so it is
//! also what forgets these.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use log::warn;
use oxidezap_chat_store::ChatStore;
use oxidezap_core::fallback_chat_name;
use whatsapp_rust::client::Client;
use whatsapp_rust::wacore_binary::jid::Jid;

/// How much weight a name carries, so a better source is never overwritten
/// by a worse one arriving later.
pub(crate) mod priority {
    /// The address book: what this account's owner decided to call them.
    pub const ADDRESS_BOOK: u8 = 3;
    /// A name the sender chose for themselves, or the one the history sync
    /// carried for a chat.
    pub const SELF_CHOSEN: u8 = 1;
    /// The number, or "Unnamed group". True, and nothing more.
    pub const NONE: u8 = 0;
}

/// The two JIDs one person can be addressed by, and what to call them when
/// nobody has a name.
pub(crate) struct ChatIdentity {
    /// The JID this conversation is keyed by everywhere: the LID when the
    /// pair is known, so a chat migrated from a phone number does not split
    /// in two.
    pub canonical_jid: String,
    /// Both aliases, because address-book names are normally PN-keyed while
    /// push names may be LID-keyed.
    pub contact_jids: Vec<Jid>,
    pub fallback_name: String,
    /// Whether a phone number is known for this person, which is what makes
    /// the server's own masked label ("+55 ·· ····") worse than useless.
    pub has_phone: bool,
}

/// The address book, memoized, and the order every label is chosen in.
pub(crate) struct NameBook {
    /// Where the address book is. The book is handed to the event loop, which
    /// has the client but not the store, so it carries its own source rather
    /// than making every caller thread one through.
    chat_store: crate::whatsapp::ChatStoreHandle,
    /// Address-book name per *contact* JID, misses included.
    contacts: Mutex<HashMap<String, Option<String>>>,
    /// The PN/LID pair behind a sender JID. A separate map on purpose: the
    /// two are keyed by JIDs that look alike and mean different things, and
    /// one map for both lets a resolved label be read back as an address-book
    /// entry it never was.
    identities: Mutex<HashMap<String, Arc<ChatIdentity>>>,
}

impl NameBook {
    pub(crate) fn new(chat_store: crate::whatsapp::ChatStoreHandle) -> Self {
        Self {
            chat_store,
            contacts: Mutex::new(HashMap::new()),
            identities: Mutex::new(HashMap::new()),
        }
    }

    /// Drop everything learned. Called where the address book is being
    /// re-read anyway, so a contact renamed on the phone appears under its
    /// new name without a restart.
    pub(crate) fn forget(&self) {
        clear(&self.contacts);
        clear(&self.identities);
    }

    /// The PN/LID pair behind a JID.
    pub(crate) async fn identity(&self, client: &Client, jid: &Jid) -> Arc<ChatIdentity> {
        let key = jid.to_non_ad_string();
        if let Some(known) = read(&self.identities, &key) {
            return known;
        }
        let identity = Arc::new(build_identity(client, jid).await);
        write(&self.identities, key, identity.clone());
        identity
    }

    /// What to call whoever is at `jid`, and how much the answer is worth.
    ///
    /// `offered` is whatever the envelope carried — a push name on a live
    /// message, the stored chat name on a hydrated one. It is used only when
    /// the address book has nothing, which is what makes a live bubble and a
    /// reloaded one say the same thing.
    pub(crate) async fn resolve(
        &self,
        store: &ChatStore,
        jid: &Jid,
        offered: Option<&str>,
        identity: &ChatIdentity,
    ) -> (String, u8) {
        if jid.is_pn() || jid.is_lid() {
            for candidate in &identity.contact_jids {
                if let Some(name) = self
                    .contact_name(store, candidate)
                    .await
                    .filter(|name| usable_name(name, identity.has_phone))
                {
                    return (name, priority::ADDRESS_BOOK);
                }
            }
        }

        if let Some(name) = offered.filter(|name| usable_name(name, identity.has_phone)) {
            return (name.to_string(), priority::SELF_CHOSEN);
        }

        (identity.fallback_name.clone(), priority::NONE)
    }

    /// What somebody is actually called, for the live paths — or `None` when
    /// nobody has ever said.
    ///
    /// The number is deliberately not an answer here. A live event names a
    /// person *and* can name the conversation they are in, and a phone number
    /// arriving as a name would displace one the history sync had already
    /// found; the front ends already render a nameless JID themselves.
    ///
    /// While the store is still coming up there is no address book to ask, so
    /// what the envelope offered stands: an event can reach the loop before
    /// the chat store is installed, and a push name then beats nothing.
    pub(crate) async fn known(
        &self,
        client: &Client,
        jid: &Jid,
        offered: Option<&str>,
    ) -> Option<String> {
        let identity = self.identity(client, jid).await;
        let Some(store) = self.chat_store.lock().await.clone() else {
            return offered
                .filter(|name| usable_name(name, identity.has_phone))
                .map(str::to_owned);
        };
        match self.resolve(&store, jid, offered, &identity).await {
            (_, priority::NONE) => None,
            (name, _) => Some(name),
        }
    }

    /// The address book's answer for one alias.
    async fn contact_name(&self, store: &ChatStore, jid: &Jid) -> Option<String> {
        let key = jid.to_string();
        if let Some(known) = read(&self.contacts, &key) {
            return known;
        }
        // An error is not an answer, so it is not written down: memoizing it
        // would file somebody as nameless for the rest of the session over a
        // pool that was busy for a moment.
        let contact = match store.contact(jid).await {
            Ok(contact) => contact,
            Err(e) => {
                warn!("Address book lookup for {} failed: {}", key, e);
                return None;
            }
        };
        let name = contact
            .and_then(|contact| contact.display_name().map(str::to_owned))
            .filter(|name| !name.trim().is_empty());
        write(&self.contacts, key, name.clone());
        name
    }
}

async fn build_identity(client: &Client, jid: &Jid) -> ChatIdentity {
    let source = jid.to_non_ad();
    let mut canonical = source.clone();
    let mut contact_jids = vec![source.clone()];
    let mut fallback_jid = source.clone();
    let mut has_phone = source.is_pn();

    if let Ok(Some(mapping)) = client.get_lid_pn_entry(&source).await {
        let pn = Jid::pn(mapping.phone_number.as_ref());
        let lid = Jid::lid(mapping.lid.as_ref());
        canonical = lid.clone();
        fallback_jid = pn.clone();
        has_phone = true;
        contact_jids = vec![pn, lid];
    }

    ChatIdentity {
        canonical_jid: canonical.to_string(),
        contact_jids,
        fallback_name: fallback_chat_name(&fallback_jid),
        has_phone,
    }
}

/// Whether a name is worth carrying.
///
/// One predicate rather than one per caller: the resolution order applies it
/// to the address book and to the push name, and [`NameBook::known`] applies
/// it on the path that runs before the chat store exists — where an unfiltered
/// push name of three spaces used to become somebody's name, and stay it,
/// since a name only ever gains weight.
fn usable_name(name: &str, has_phone: bool) -> bool {
    !(name.trim().is_empty() || has_phone && is_masked_phone_label(name))
}

/// The server's own stand-in for a number it will not spell out, e.g.
/// `+55 ·· ···· ··43`. Worse than the number we already hold.
fn is_masked_phone_label(name: &str) -> bool {
    name.starts_with('+')
        && name
            .chars()
            .filter(|c| matches!(c, '\u{00b7}' | '\u{2022}' | '\u{2219}'))
            .count()
            >= 2
}

// A poisoned lock here means a panic while holding a `HashMap`, which cannot
// leave one inconsistent. Recovering is strictly better than turning a naming
// question into a second panic.
fn read<T: Clone>(map: &Mutex<HashMap<String, T>>, key: &str) -> Option<T> {
    map.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(key)
        .cloned()
}

fn write<T>(map: &Mutex<HashMap<String, T>>, key: String, value: T) {
    map.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(key, value);
}

fn clear<T>(map: &Mutex<HashMap<String, T>>) {
    map.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blank_or_masked_push_name_is_not_a_name() {
        assert!(!usable_name("   ", false));
        assert!(!usable_name("", true));
        assert!(!usable_name("+55 ·· ···· ··43", true));
        // Nothing better is known, so the mask is all there is.
        assert!(usable_name("+55 ·· ···· ··43", false));
        assert!(usable_name("Ana", true));
    }

    #[test]
    fn a_masked_label_is_recognised_by_its_dots() {
        assert!(is_masked_phone_label("+55 ·· ···· ··43"));
        assert!(is_masked_phone_label("+1 •• ••••"));
        assert!(is_masked_phone_label("+55\u{2219}\u{2219}\u{2219}00"));
        assert!(!is_masked_phone_label("+12025550143"));
        assert!(!is_masked_phone_label("Ana"));
        // One dot is a separator somebody typed, not a mask.
        assert!(!is_masked_phone_label("+55 ·11"));
    }
}
