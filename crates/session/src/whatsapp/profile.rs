//! Profile reads use user addresses, never companion-device addresses.
use std::collections::HashMap;

use whatsapp_rust::features::ProfilePictureLookup;
use whatsapp_rust::lid_pn_cache::LidPnEntry;
use whatsapp_rust::wacore::iq::usync::UserInfo;
use whatsapp_rust::wacore_binary::jid::{Jid, JidExt as _};
use whatsapp_rust::{ProfilePictureRequest, ProfilePictureTarget, ProfilePictureType};

use super::{Session, WhatsAppClient};
use crate::exec::Task;

/// Protocol profile details plus explicitly sourced identity information.
#[derive(Debug, Clone, Default)]
pub struct ProfileView {
    pub jid: String,
    pub name: Option<String>,
    pub about: Option<String>,
    pub picture_url: Option<String>,
    pub picture_status: &'static str,
    pub pn_jid: Option<String>,
    pub lid: Option<String>,
    pub pn_source: Option<&'static str>,
    pub lid_source: Option<&'static str>,
    pub username: Option<String>,
    pub verified_name: Option<String>,
    pub is_business: Option<bool>,
}

impl WhatsAppClient {
    /// `None` selects the current account, just as its explicit PN or LID does.
    pub fn get_profile(&self, jid: Option<String>) -> Task<Result<ProfileView, String>> {
        self.read_profile(jid, false)
    }

    pub fn get_business_profile(&self, jid: String) -> Task<Result<ProfileView, String>> {
        self.read_profile(Some(jid), true)
    }

    fn read_profile(
        &self,
        jid: Option<String>,
        business: bool,
    ) -> Task<Result<ProfileView, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let live = session.lock().await.clone().ok_or("no session yet")?;
            query_profile(&live, jid, business).await
        })
    }
}

fn profile_target(jid: Option<String>, pn: Option<Jid>, lid: Option<Jid>) -> Result<Jid, String> {
    let target: Jid = match jid {
        Some(jid) => jid.parse().map_err(|_| "not a user address")?,
        None => pn.clone().or_else(|| lid.clone()).ok_or("not logged in")?,
    };
    if target.user.is_empty()
        || target.is_psa()
        || !(target.server.is_pn_family() || target.server.is_lid_family())
    {
        return Err("not a user address".into());
    }
    let target = target.to_non_ad();
    // Both self aliases select the same query route. Other contacts retain
    // their requested namespace; a local mapping does not disclose a PN.
    if [pn.as_ref(), lid.as_ref()]
        .into_iter()
        .flatten()
        .any(|own| own.to_non_ad() == target)
    {
        return Ok(pn.or(lid).expect("a matching self identity").to_non_ad());
    }
    Ok(target)
}

fn matching_user<'a>(
    info: &'a HashMap<Jid, UserInfo>,
    target: &Jid,
) -> Result<&'a UserInfo, String> {
    if let Some(user) = info.values().find(|user| user.jid.to_non_ad() == *target) {
        return Ok(user);
    }
    // Some replies address the other namespace. Accept that only when the
    // reply itself supplies the association, never by taking an arbitrary row.
    let mut matches = info.values().filter(|user| {
        [user.pn_jid.as_ref(), user.lid.as_ref()]
            .into_iter()
            .flatten()
            .any(|jid| jid.to_non_ad() == *target)
    });
    let user = matches
        .next()
        .ok_or("no profile returned for the requested user")?;
    if matches.next().is_some() {
        return Err("ambiguous profile response for the requested user".into());
    }
    Ok(user)
}

async fn query_profile(
    live: &Session,
    jid: Option<String>,
    business: bool,
) -> Result<ProfileView, String> {
    let client = &live.client;
    let pn = client.pn();
    let lid = client.lid();
    let target = profile_target(jid, pn.clone(), lid.clone())?;
    let info = client
        .contacts()
        .get_user_info(std::slice::from_ref(&target))
        .await
        .map_err(|e| e.to_string())?;
    let user = matching_user(&info, &target)?;
    let mapping = client
        .get_lid_pn_entry(&target)
        .await
        .map_err(|e| e.to_string())?;
    let mut profile = profile_details(&target, user, pn.as_ref(), lid.as_ref(), mapping.as_ref());
    let picture = client
        .pictures()
        .lookup(ProfilePictureRequest::new(
            ProfilePictureTarget::Contact(&target),
            ProfilePictureType::Full,
        ))
        .await
        .map_err(|e| format!("profile photo lookup failed: {e}"))?;
    (profile.picture_url, profile.picture_status) = picture_details(picture);
    profile.name = if business {
        profile.verified_name.clone()
    } else {
        live.chat_store
            .contact(&target)
            .await
            .map_err(|e| e.to_string())?
            .and_then(|contact| {
                contact
                    .full_name
                    .or(contact.push_name)
                    .or(contact.business_name)
            })
    };
    Ok(profile)
}

fn picture_details(picture: ProfilePictureLookup) -> (Option<String>, &'static str) {
    match picture {
        ProfilePictureLookup::Found(picture) => (Some(picture.url), "found"),
        ProfilePictureLookup::NotFound => (None, "not_found"),
        ProfilePictureLookup::NotAuthorized => (None, "not_authorized"),
        ProfilePictureLookup::Unchanged => (None, "unchanged"),
        _ => (None, "unknown"),
    }
}

fn profile_details(
    target: &Jid,
    user: &UserInfo,
    own_pn: Option<&Jid>,
    own_lid: Option<&Jid>,
    mapping: Option<&LidPnEntry>,
) -> ProfileView {
    let mut profile = ProfileView {
        jid: target.to_string(),
        about: user.status.clone(),
        username: user.username.as_ref().map(ToString::to_string),
        verified_name: user
            .verified_name
            .as_ref()
            .and_then(|name| name.name.clone()),
        is_business: user.is_business.then_some(true),
        ..Default::default()
    };
    let is_self = [own_pn, own_lid]
        .into_iter()
        .flatten()
        .any(|own| own.to_non_ad() == *target);
    let mut pn = target
        .server
        .is_pn_family()
        .then(|| (target.clone(), "request"));
    let mut lid = target
        .server
        .is_lid_family()
        .then(|| (target.clone(), "request"));
    if let Some(mapping) = mapping {
        // The lookup is keyed by the requested identity, never by string similarity.
        if (target.server.is_pn_family() && target.user.as_str() == mapping.phone_number.as_ref())
            || (target.server.is_lid_family() && target.user.as_str() == mapping.lid.as_ref())
        {
            pn.get_or_insert_with(|| (Jid::pn(mapping.phone_number.as_ref()), "local_mapping"));
            lid.get_or_insert_with(|| (Jid::lid(mapping.lid.as_ref()), "local_mapping"));
        }
    }
    if is_self {
        if let Some(own) = own_pn {
            pn = Some((own.to_non_ad(), "session"));
        }
        if let Some(own) = own_lid {
            lid = Some((own.to_non_ad(), "session"));
        }
    }
    // The returned primary JID is protocol evidence too, including a reply
    // addressed through the other namespace with an explicit association.
    if user.jid.server.is_pn_family() {
        pn = Some((user.jid.to_non_ad(), "protocol"));
    } else if user.jid.server.is_lid_family() {
        lid = Some((user.jid.to_non_ad(), "protocol"));
    }
    if let Some(disclosed) = user.pn_jid.as_ref().filter(|jid| jid.server.is_pn_family()) {
        pn = Some((disclosed.to_non_ad(), "protocol"));
    }
    if let Some(disclosed) = user.lid.as_ref().filter(|jid| jid.server.is_lid_family()) {
        lid = Some((disclosed.to_non_ad(), "protocol"));
    }
    if let Some((jid, source)) = pn {
        profile.pn_jid = Some(jid.to_string());
        profile.pn_source = Some(source);
    }
    if let Some((jid, source)) = lid {
        profile.lid = Some(jid.to_string());
        profile.lid_source = Some(source);
    }
    profile
}

#[cfg(test)]
mod tests {
    use super::*;
    use whatsapp_rust::wacore::iq::{
        contacts::ProfilePictureSpec, spec::IqSpec, usync::UserInfoSpec,
    };
    use whatsapp_rust::wacore_binary::{Node, builder::NodeBuilder};

    fn user_info(jid: &Jid, children: Vec<Node>) -> HashMap<Jid, UserInfo> {
        let response = NodeBuilder::new("iq")
            .attr("type", "result")
            .children([NodeBuilder::new("usync")
                .children([NodeBuilder::new("list")
                    .children([NodeBuilder::new("user")
                        .attr("jid", jid)
                        .children(children)
                        .build()])
                    .build()])
                .build()])
            .build();
        UserInfoSpec::new(vec![jid.clone()], "profile-test")
            .parse_response(&response.as_node_ref())
            .unwrap()
    }

    #[test]
    fn default_and_explicit_self_profile_use_the_same_user_address() {
        let pn = Jid::pn("15550001111").with_device(7);
        let lid = Jid::lid("100000001").with_device(7);
        let implicit = profile_target(None, Some(pn.clone()), Some(lid.clone())).unwrap();
        for explicit in [
            pn.to_string(),
            pn.to_non_ad_string(),
            lid.to_string(),
            lid.to_non_ad_string(),
        ] {
            let target =
                profile_target(Some(explicit), Some(pn.clone()), Some(lid.clone())).unwrap();
            assert_eq!(target, implicit);
            assert_eq!(target.device, 0);
            // This is the production IQ builder used by Pictures::lookup.
            let implicit_iq = ProfilePictureSpec::full(&implicit).build_iq();
            let explicit_iq = ProfilePictureSpec::full(&target).build_iq();
            assert_eq!(implicit_iq.target, Some(pn.to_non_ad()));
            assert_eq!(explicit_iq.target, implicit_iq.target);
            assert!(ProfilePictureSpec::full(&target).existing_id.is_none());
        }
    }

    #[test]
    fn profiles_validate_user_namespaces_and_keep_other_accounts_separate() {
        let own = Jid::pn("15550001111");
        let other = Jid::pn("15550002222");
        assert_eq!(
            profile_target(Some(other.with_device(3).to_string()), Some(own), None).unwrap(),
            other
        );
        for bad in [
            "120000000@g.us",
            "status@broadcast",
            "123@newsletter",
            "s.whatsapp.net",
            "0@s.whatsapp.net",
        ] {
            assert!(
                profile_target(Some(bad.into()), None, None).is_err(),
                "{bad}"
            );
        }
        assert!(profile_target(None, None, None).is_err());
        let lid = Jid::lid("100000001").with_device(4);
        assert_eq!(
            profile_target(None, None, Some(lid.clone())).unwrap(),
            lid.to_non_ad()
        );
    }

    #[test]
    fn protocol_identity_and_local_mapping_have_distinct_sources() {
        let target = Jid::pn("15550001111");
        let info = user_info(
            &target,
            vec![NodeBuilder::new("lid").attr("val", "100000001@lid").build()],
        );
        let user = matching_user(&info, &target).unwrap();
        let mapping = LidPnEntry::new(
            "100000009",
            "15550001111",
            whatsapp_rust::lid_pn_cache::LearningSource::Usync,
        );
        let profile = profile_details(&target, user, None, None, Some(&mapping));
        assert_eq!(profile.lid.as_deref(), Some("100000001@lid"));
        assert_eq!(profile.lid_source, Some("protocol"));
        assert_eq!(profile.pn_source, Some("protocol"));

        let info = user_info(&target, vec![]);
        let user = matching_user(&info, &target).unwrap();
        let profile = profile_details(&target, user, None, None, Some(&mapping));
        assert_eq!(profile.lid.as_deref(), Some("100000009@lid"));
        assert_eq!(profile.lid_source, Some("local_mapping"));
        let profile = profile_details(&target, user, None, None, None);
        assert_eq!(profile.lid, None);
        assert_eq!(profile.lid_source, None);
    }

    #[test]
    fn lid_profiles_only_disclose_protocol_pn_or_known_mapping() {
        let target = Jid::lid("100000001");
        let info = user_info(
            &target,
            vec![
                NodeBuilder::new("business")
                    .attr("pn_jid", "15550001111:2@s.whatsapp.net")
                    .build(),
            ],
        );
        let profile = profile_details(
            &target,
            matching_user(&info, &target).unwrap(),
            None,
            None,
            None,
        );
        assert_eq!(
            profile.pn_jid.as_deref(),
            Some("15550001111@s.whatsapp.net")
        );
        assert_eq!(profile.pn_source, Some("protocol"));
        let info = user_info(&target, vec![]);
        let profile = profile_details(
            &target,
            matching_user(&info, &target).unwrap(),
            None,
            None,
            None,
        );
        assert_eq!(profile.pn_jid, None);
        assert_eq!(profile.lid.as_deref(), Some("100000001@lid"));
        let unrelated = Jid::pn("15550002222");
        assert!(matching_user(&info, &unrelated).is_err());
    }

    #[test]
    fn equal_user_strings_in_different_namespaces_are_not_an_alias() {
        let target = Jid::pn("100000001");
        let own_lid = Jid::lid("100000001");
        let own_pn = Jid::pn("15550001111");
        assert_eq!(
            profile_target(
                Some(target.to_string()),
                Some(own_pn.clone()),
                Some(own_lid.clone())
            )
            .unwrap(),
            target
        );
        let info = user_info(&target, vec![]);
        let profile = profile_details(
            &target,
            matching_user(&info, &target).unwrap(),
            Some(&own_pn),
            Some(&own_lid),
            None,
        );
        assert_eq!(profile.lid, None);
        assert_eq!(profile.pn_jid.as_deref(), Some("100000001@s.whatsapp.net"));
    }

    #[test]
    fn response_aliases_require_an_explicit_protocol_association() {
        let lid = Jid::lid("100000001");
        let pn = Jid::pn("15550001111");
        let info = user_info(
            &lid,
            vec![
                NodeBuilder::new("business")
                    .attr("pn_jid", pn.clone())
                    .build(),
            ],
        );
        assert_eq!(matching_user(&info, &pn).unwrap().jid, lid);
        let profile = profile_details(&pn, matching_user(&info, &pn).unwrap(), None, None, None);
        assert_eq!(
            profile.pn_jid.as_deref(),
            Some("15550001111@s.whatsapp.net")
        );
        assert_eq!(profile.lid.as_deref(), Some("100000001@lid"));
        assert_eq!(profile.pn_source, Some("protocol"));
        assert_eq!(profile.lid_source, Some("protocol"));
        assert!(matching_user(&info, &Jid::pn("15550002222")).is_err());
    }

    #[test]
    fn optional_protocol_details_are_not_invented() {
        let target = Jid::pn("15550001111");
        let info = user_info(
            &target,
            vec![
                NodeBuilder::new("username")
                    .string_content("synthetic_user")
                    .build(),
                NodeBuilder::new("status")
                    .string_content("Synthetic about")
                    .build(),
            ],
        );
        let profile = profile_details(
            &target,
            matching_user(&info, &target).unwrap(),
            None,
            None,
            None,
        );
        assert_eq!(profile.username.as_deref(), Some("synthetic_user"));
        assert_eq!(profile.about.as_deref(), Some("Synthetic about"));
        assert_eq!(profile.verified_name, None);
        assert_eq!(profile.is_business, None);
        let info = user_info(&target, vec![]);
        let profile = profile_details(
            &target,
            matching_user(&info, &target).unwrap(),
            None,
            None,
            None,
        );
        assert_eq!(profile.username, None);
        assert_eq!(profile.about, None);
    }

    #[test]
    fn session_identity_is_only_used_for_the_selected_self() {
        let pn = Jid::pn("15550001111");
        let lid = Jid::lid("100000001").with_device(9);
        let info = user_info(&pn, vec![]);
        let profile = profile_details(
            &pn,
            matching_user(&info, &pn).unwrap(),
            Some(&pn),
            Some(&lid),
            None,
        );
        assert_eq!(profile.lid.as_deref(), Some("100000001@lid"));
        assert_eq!(profile.lid_source, Some("session"));
        let other = Jid::pn("15550002222");
        let info = user_info(&other, vec![]);
        let profile = profile_details(
            &other,
            matching_user(&info, &other).unwrap(),
            Some(&pn),
            Some(&lid),
            None,
        );
        assert_eq!(profile.lid, None);
    }

    #[test]
    fn photo_outcomes_do_not_turn_privacy_or_unchanged_into_absence() {
        assert_eq!(
            picture_details(ProfilePictureLookup::NotFound),
            (None, "not_found")
        );
        assert_eq!(
            picture_details(ProfilePictureLookup::NotAuthorized),
            (None, "not_authorized")
        );
        assert_eq!(
            picture_details(ProfilePictureLookup::Unchanged),
            (None, "unchanged")
        );
        let target = Jid::pn("15550001111");
        let spec = ProfilePictureSpec::full(&target);
        let response = NodeBuilder::new("iq")
            .attr("type", "result")
            .children([NodeBuilder::new("picture")
                .attr("id", "synthetic-photo")
                .attr("url", "https://example.invalid/profile.jpg")
                .build()])
            .build();
        assert_eq!(
            picture_details(spec.parse_response(&response.as_node_ref()).unwrap()),
            (Some("https://example.invalid/profile.jpg".into()), "found")
        );
        let refused = NodeBuilder::new("iq")
            .attr("type", "result")
            .children([NodeBuilder::new("picture")
                .children([NodeBuilder::new("error")
                    .attr("code", "429")
                    .attr("text", "rate-overlimit")
                    .build()])
                .build()])
            .build();
        assert!(spec.parse_response(&refused.as_node_ref()).is_err());
    }
}
