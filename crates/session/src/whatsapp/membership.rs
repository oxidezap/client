//! Resolve every event participant independently, using only local identity/contact data.
use crate::names::NameBook;
use oxidezap_core::{GroupMembership, NoticeParticipant};
use std::sync::Arc;
use whatsapp_rust::client::Client;
use whatsapp_rust::wacore_binary::jid::Jid;

pub(super) async fn resolve(
    client: &Arc<Client>,
    names: &NameBook,
    store: &oxidezap_chat_store::ChatStore,
    event: &mut GroupMembership,
) {
    let own = super::history::own_jids(client);
    for person in event.actor.iter_mut().chain(event.participants.iter_mut()) {
        resolve_person(client, names, store, person, &own).await;
    }
}
pub(super) async fn resolve_person(
    client: &Arc<Client>,
    names: &NameBook,
    store: &oxidezap_chat_store::ChatStore,
    person: &mut NoticeParticipant,
    own: &[String],
) {
    person.label = None;
    person.is_self = false;
    person.canonical_jid = None;
    let Ok(jid) = person.jid.parse::<Jid>() else {
        person.label = Some("Unknown contact".to_owned());
        return;
    };
    if !(jid.is_pn() || jid.is_lid())
        || jid.user_base().is_empty()
        || !jid.user_base().bytes().all(|c| c.is_ascii_digit())
    {
        person.label = Some("Unknown contact".to_owned());
        return;
    }
    let identity = names.identity(client, &jid).await;
    person.canonical_jid = Some(identity.canonical_jid.clone());
    let mut candidates = vec![jid.to_non_ad()];
    // These aliases are explicit fields of this participant, never another
    // participant's number and never a string guessed from a display name.
    if let Some(pn) = person
        .phone_jid
        .as_deref()
        .and_then(|s| s.parse::<Jid>().ok())
        .filter(|j| j.is_pn())
    {
        candidates.push(pn.to_non_ad());
    }
    if let Some(lid) = person
        .lid
        .as_deref()
        .and_then(|s| s.parse::<Jid>().ok())
        .filter(|j| j.is_lid())
    {
        candidates.push(lid.to_non_ad());
    }
    candidates.sort_by_key(|jid| !jid.is_pn());
    if own.contains(&identity.canonical_jid)
        || candidates.iter().any(|j| own.contains(&j.to_string()))
    {
        person.is_self = true;
        return;
    }
    // Search all authoritative aliases for a contact before considering the
    // event's display label. A PN-keyed saved name outranks a LID push name.
    {
        let mut best = None;
        for candidate in &candidates {
            let id = names.identity(client, candidate).await;
            let (name, priority) = names.resolve(store, candidate, None, &id).await;
            if priority > 0 && best.as_ref().is_none_or(|(_, old)| priority > *old) {
                best = Some((name, priority));
            }
        }
        if let Some((label, _)) = best {
            person.label = Some(label);
            return;
        }
    }
    // The shared resolver filters blank/masked names under the same policy as
    // ordinary messages. It does not issue profile or metadata requests.
    let offered_identity = if !identity.has_phone {
        if let Some(pn) = candidates.iter().find(|j| j.is_pn()) {
            names.identity(client, pn).await
        } else {
            identity.clone()
        }
    } else {
        identity.clone()
    };
    person.label = names
        .named(
            store,
            &jid,
            person.offered_name.as_deref(),
            &offered_identity,
        )
        .await;
    if person.label.is_none() {
        person.label = Some(
            identity
                .contact_jids
                .iter()
                .filter(|j| j.is_pn())
                .find_map(|pn| oxidezap_core::format_phone_jid(&pn.to_string()))
                .unwrap_or_else(|| person.fallback_label()),
        );
    }
}
