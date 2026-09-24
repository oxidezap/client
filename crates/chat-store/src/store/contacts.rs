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

pub(super) fn clear_contact_names(
    conn: &mut SqliteConnection,
    device_id: i32,
    jid: &str,
) -> QueryResult<bool> {
    use schema::contacts::dsl as contacts;
    let key = contact_key(jid).into_owned();
    let mut keys = vec![key.clone()];
    if let Ok(parsed) = key.parse::<Jid>()
        && parsed.integrator == 0
        && (parsed.is_pn() || parsed.is_lid())
    {
        use schema::lid_pn_mapping::dsl as mapping;
        let mapped = if parsed.is_lid() {
            mapping::lid_pn_mapping
                .filter(
                    mapping::device_id
                        .eq(device_id)
                        .and(mapping::lid.eq(parsed.user.as_str())),
                )
                .select(mapping::phone_number)
                .first::<String>(conn)
                .optional()?
                .map(|user| Jid::new(user, wacore_binary::Server::Pn).to_string())
        } else {
            mapping::lid_pn_mapping
                .filter(
                    mapping::device_id
                        .eq(device_id)
                        .and(mapping::phone_number.eq(parsed.user.as_str())),
                )
                .order((mapping::updated_at.desc(), mapping::lid.desc()))
                .select(mapping::lid)
                .first::<String>(conn)
                .optional()?
                .map(|user| Jid::new(user, wacore_binary::Server::Lid).to_string())
        };
        if let Some(mapped) = mapped {
            keys.push(mapped);
        }
    }

    let changed = diesel::update(
        contacts::contacts
            .filter(contacts::device_id.eq(device_id))
            .filter(contacts::jid.eq_any(keys))
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
    Ok(changed > 0)
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
