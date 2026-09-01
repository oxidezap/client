//! Contact names, from wherever they arrive: push names riding on live
//! messages, business verified names, and the app-state contact action.

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
