//! Contact names, from wherever they arrive: push names riding on live
//! messages, business verified names, and the app-state contact action.
//!
//! The three upserts below read as one function called three times, and are
//! not: what differs between them is the column list, which is the whole of
//! an upsert. One that took every name would write the columns its caller
//! knows nothing about — a push name arriving on a message would blank the
//! address-book name beside it, in the INSERT and again in the DO UPDATE —
//! and a version that skipped them per call is a macro over diesel's typed
//! values. `contact_key` is the part they genuinely share, so it is shared.

use std::borrow::Cow;

use diesel::prelude::*;
use wacore_binary::Jid;

use crate::schema;

/// Contacts are keyed by the peer's bare identity, the canonical form
/// [`ChatStore::contact`] looks up. Message senders keep their device by
/// design (a peer texting from WhatsApp Web is `user:48@lid`), so writing the
/// sender verbatim would file the name under a key nothing ever reads.
fn contact_key(jid: &str) -> Cow<'_, str> {
    match jid.parse::<Jid>() {
        // Bare already renders identically; only pay the allocation otherwise.
        Ok(parsed) if parsed.device != 0 || parsed.agent != 0 => {
            Cow::Owned(parsed.to_non_ad_string())
        }
        _ => Cow::Borrowed(jid),
    }
}

pub(super) fn upsert_contact_push_name(
    conn: &mut SqliteConnection,
    device_id: i32,
    jid: &str,
    push_name: &str,
) -> QueryResult<()> {
    use schema::contacts::dsl;
    let jid = contact_key(jid);
    diesel::insert_into(dsl::contacts)
        .values((
            dsl::device_id.eq(device_id),
            dsl::jid.eq(&jid),
            dsl::push_name.eq(push_name),
        ))
        .on_conflict((dsl::device_id, dsl::jid))
        .do_update()
        .set(dsl::push_name.eq(push_name))
        .execute(conn)?;
    Ok(())
}

pub(super) fn upsert_contact_business_name(
    conn: &mut SqliteConnection,
    device_id: i32,
    jid: &str,
    business_name: &str,
) -> QueryResult<()> {
    use schema::contacts::dsl;
    let jid = contact_key(jid);
    diesel::insert_into(dsl::contacts)
        .values((
            dsl::device_id.eq(device_id),
            dsl::jid.eq(&jid),
            dsl::business_name.eq(business_name),
        ))
        .on_conflict((dsl::device_id, dsl::jid))
        .do_update()
        .set(dsl::business_name.eq(business_name))
        .execute(conn)?;
    Ok(())
}

fn contact_keys(
    conn: &mut SqliteConnection,
    device_id: i32,
    jid: &str,
) -> QueryResult<Vec<String>> {
    let key = contact_key(jid).into_owned();
    let mut keys = vec![key.clone()];
    if let Ok(parsed) = key.parse::<Jid>()
        && parsed.integrator == 0
        && (parsed.is_pn() || parsed.is_lid())
    {
        use schema::lid_pn_mapping::dsl as mapping;
        let phone_number = if parsed.is_lid() {
            mapping::lid_pn_mapping
                .filter(
                    mapping::device_id
                        .eq(device_id)
                        .and(mapping::lid.eq(parsed.user.as_str())),
                )
                .select(mapping::phone_number)
                .first::<String>(conn)
                .optional()?
        } else {
            Some(parsed.user.to_string())
        };
        if let Some(phone_number) = phone_number {
            keys.push(Jid::new(&phone_number, wacore_binary::Server::Pn).to_string());
            let lids = mapping::lid_pn_mapping
                .filter(
                    mapping::device_id
                        .eq(device_id)
                        .and(mapping::phone_number.eq(phone_number)),
                )
                .select(mapping::lid)
                .load::<String>(conn)?;
            keys.extend(
                lids.into_iter()
                    .map(|user| Jid::new(user, wacore_binary::Server::Lid).to_string()),
            );
        }
    }
    keys.sort();
    keys.dedup();
    Ok(keys)
}

pub(super) fn update_address_book_chat_names(
    conn: &mut SqliteConnection,
    device_id: i32,
    jid: &str,
    full_name: Option<&str>,
    first_name: Option<&str>,
) -> QueryResult<bool> {
    use schema::chats::dsl as chats;
    let keys = contact_keys(conn, device_id, jid)?;
    let name = full_name
        .filter(|name| !name.trim().is_empty())
        .or_else(|| first_name.filter(|name| !name.trim().is_empty()));
    let rows = chats::chats
        .filter(chats::device_id.eq(device_id))
        .filter(chats::jid.eq_any(&keys))
        .filter(chats::name_from_address_book.eq(true))
        .select((chats::jid, chats::name))
        .load::<(String, Option<String>)>(conn)?;
    let mut changed = false;
    for (chat_jid, current) in rows {
        if current.as_deref() == name {
            continue;
        }
        diesel::update(
            chats::chats
                .filter(chats::device_id.eq(device_id))
                .filter(chats::jid.eq(chat_jid)),
        )
        .set((
            chats::name.eq(name),
            chats::name_from_address_book.eq(name.is_some()),
        ))
        .execute(conn)?;
        changed = true;
    }
    Ok(changed)
}

pub(super) fn clear_contact_names(
    conn: &mut SqliteConnection,
    device_id: i32,
    jid: &str,
) -> QueryResult<(bool, bool)> {
    use schema::contacts::dsl as contacts;
    let keys = contact_keys(conn, device_id, jid)?;
    let contact_changed = diesel::update(
        contacts::contacts
            .filter(contacts::device_id.eq(device_id))
            .filter(contacts::jid.eq_any(&keys))
            .filter(
                contacts::full_name
                    .is_not_null()
                    .or(contacts::first_name.is_not_null()),
            ),
    )
    .set((
        contacts::full_name.eq(None::<String>),
        contacts::first_name.eq(None::<String>),
    ))
    .execute(conn)?;

    let chat_changed = diesel::update(
        schema::chats::dsl::chats
            .filter(schema::chats::dsl::device_id.eq(device_id))
            .filter(schema::chats::dsl::jid.eq_any(&keys))
            .filter(schema::chats::dsl::name_from_address_book.eq(true)),
    )
    .set((
        schema::chats::dsl::name.eq(None::<String>),
        schema::chats::dsl::name_from_address_book.eq(false),
    ))
    .execute(conn)?;

    Ok((contact_changed > 0, chat_changed > 0))
}

pub(super) fn upsert_contact_names(
    conn: &mut SqliteConnection,
    device_id: i32,
    jid: &str,
    full_name: Option<&str>,
    first_name: Option<&str>,
) -> QueryResult<()> {
    use schema::contacts::dsl;
    let jid = contact_key(jid);
    diesel::insert_into(dsl::contacts)
        .values((
            dsl::device_id.eq(device_id),
            dsl::jid.eq(&jid),
            dsl::full_name.eq(full_name),
            dsl::first_name.eq(first_name),
        ))
        .on_conflict((dsl::device_id, dsl::jid))
        .do_update()
        .set((dsl::full_name.eq(full_name), dsl::first_name.eq(first_name)))
        .execute(conn)?;
    Ok(())
}
