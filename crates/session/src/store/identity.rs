//! Conservative evidence for a relink decision. This never removes an account.
use whatsapp_rust::wacore_binary::jid::Jid;

use super::StoredAccount;

/// Both persisted namespaces must agree. A shared LID alone is not proof of
/// stability across relinks, and a shared phone number alone is not enough
/// to retire retained history after number reassignment.
#[must_use]
pub fn same_account_identity(old: &StoredAccount, new: &StoredAccount) -> bool {
    fn canonical(raw: &Option<String>, phone: bool) -> Option<Jid> {
        let jid: Jid = raw.as_deref()?.parse().ok()?;
        if jid.user.is_empty()
            || jid.integrator != 0
            || (phone && !jid.is_pn())
            || (!phone && !jid.is_lid())
        {
            return None;
        }
        // The parser deliberately accepts non-user and server-only JIDs too.
        // Do not let those contracts broaden account identity matching.
        if !jid.user.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        Some(jid.to_non_ad())
    }
    let (Some(old_pn), Some(new_pn), Some(old_lid), Some(new_lid)) = (
        canonical(&old.jid, true),
        canonical(&new.jid, true),
        canonical(&old.lid, false),
        canonical(&new.lid, false),
    ) else {
        return false;
    };
    old_pn == new_pn && old_lid == new_lid
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidezap_core::AccountId;

    fn account(id: i32, pn: Option<&str>, lid: Option<&str>) -> StoredAccount {
        StoredAccount {
            id: AccountId::new(id).unwrap(),
            jid: pn.map(str::to_owned),
            lid: lid.map(str::to_owned),
            push_name: "Synthetic fixture".into(),
            linked: true,
        }
    }

    #[test]
    fn device_suffixes_do_not_make_a_new_whatsapp_identity() {
        let old = account(1, Some("100001:7@s.whatsapp.net"), Some("200001:7@lid"));
        let new = account(2, Some("100001:12@s.whatsapp.net"), Some("200001:12@lid"));
        assert!(same_account_identity(&old, &new));
    }

    #[test]
    fn incomplete_conflicting_and_different_evidence_preserves_both_accounts() {
        let old = account(1, Some("100001@s.whatsapp.net"), Some("200001@lid"));
        for new in [
            account(2, None, Some("200001@lid")),
            account(2, Some("100001@s.whatsapp.net"), None),
            account(2, Some("100002@s.whatsapp.net"), Some("200001@lid")),
            account(2, Some("100001@s.whatsapp.net"), Some("200002@lid")),
            account(2, Some("100002@s.whatsapp.net"), Some("200002@lid")),
            account(2, Some("100001@g.us"), Some("200001@lid")),
            account(2, Some("100001@s.whatsapp.net"), Some("@lid")),
        ] {
            assert!(!same_account_identity(&old, &new));
        }
    }
}
