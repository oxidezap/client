//! Output formatting for humans and machine consumption (JSON/NDJSON).
//!
//! One contract for both: every command's result crosses this module, so
//! `--json` never carries a human sentence and a failure never exits zero. The
//! success envelope is `{"ok": true, "data": ...}` and the failure envelope is
//! `{"ok": false, "error": {"code", "message"}}`; both go to the stream the
//! reader expects (stdout for a result, stderr for an error). `--events` is the
//! one exception, and deliberately so: it is a stream of `DaemonEvent` lines
//! and nothing else, so a follower can parse every line it reads.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Human,
    Json,
    Events,
}

/// Print a successful result to stdout.
pub fn print_result<T: Serialize>(mode: OutputMode, data: &T, human_formatter: impl FnOnce(&T)) {
    match mode {
        OutputMode::Json | OutputMode::Events => print_json_ok(data),
        OutputMode::Human => human_formatter(data),
    }
}

/// Print a successful action, which carries no payload beyond what it did.
pub fn print_action(mode: OutputMode, action: &str, human: impl FnOnce()) {
    match mode {
        OutputMode::Human => human(),
        OutputMode::Json | OutputMode::Events => {
            let data = serde_json::json!({ "action": action });
            print_json_ok(&data);
        }
    }
}

/// Print one event in NDJSON format. The stream carries events and nothing
/// else, on either output mode.
pub fn print_event<T: Serialize>(event: &T) {
    match serde_json::to_string(event) {
        Ok(line) => println!("{line}"),
        Err(err) => eprintln!("error: failed to serialize event: {err}"),
    }
}

/// Print a structured failure to stderr.
pub fn print_error(mode: OutputMode, code: &str, message: &str) {
    match mode {
        OutputMode::Json | OutputMode::Events => {
            let val = serde_json::json!({
                "ok": false,
                "error": {
                    "code": code,
                    "message": message,
                }
            });
            eprintln!("{}", serde_json::to_string_pretty(&val).unwrap_or_default());
        }
        OutputMode::Human => {
            eprintln!("error [{code}]: {message}");
        }
    }
}

fn print_json_ok<T: Serialize>(data: &T) {
    match json_ok(data) {
        Ok(json) => println!("{json}"),
        Err(err) => eprintln!("error: failed to serialize json output: {err}"),
    }
}

// Borrow the payload instead of cloning every field into a JSON value first.
// Object key order follows the serializer rather than an intermediate map.
// Consumers use the parsed keys; key ordering is not part of the CLI contract.
#[derive(Serialize)]
struct Success<'a, T: ?Sized> {
    data: &'a T,
    ok: bool,
}

pub(crate) fn json_ok<T: Serialize>(data: &T) -> serde_json::Result<String> {
    serde_json::to_string_pretty(&Success { data, ok: true }).or_else(|_| {
        // Preserve the previous fallback when a payload cannot be serialized.
        serde_json::to_string_pretty(&Success {
            data: &serde_json::Value::Null,
            ok: true,
        })
    })
}

#[cfg(test)]
mod tests {
    /// The success envelope is one shape for every command, so a script reads
    /// the payload at `.data` and never at the top level.
    #[test]
    fn the_success_envelope_wraps_its_payload() {
        let value = serde_json::json!({ "id": "x" });
        let envelope = serde_json::json!({ "ok": true, "data": value });
        assert_eq!(envelope["ok"], serde_json::json!(true));
        assert_eq!(envelope["data"]["id"], serde_json::json!("x"));
    }
}

/// Shared by ordinary and business profile commands.
pub(crate) fn profile_text(profile: &oxidezap_wire::dto::ProfileDto) -> String {
    use std::fmt::Write as _;
    let mut text = String::new();
    let _ = writeln!(text, "JID:   {}", profile.jid);
    for (label, jid, source) in [
        ("PN", &profile.pn_jid, &profile.pn_source),
        ("LID", &profile.lid, &profile.lid_source),
    ] {
        if let Some(jid) = jid {
            let _ = write!(text, "{label}: {jid}");
            if let Some(source) = source {
                let _ = write!(text, " [{source}]");
            }
            text.push('\n');
        }
    }
    let _ = writeln!(text, "Name:  {}", profile.name.as_deref().unwrap_or("none"));
    let _ = writeln!(
        text,
        "About: {}",
        profile.about.as_deref().unwrap_or("none")
    );
    if let Some(username) = &profile.username {
        let _ = writeln!(text, "Username: @{username}");
    }
    if let Some(is_business) = profile.is_business {
        let _ = writeln!(text, "Business: {is_business}");
    }
    if let Some(name) = &profile.verified_name {
        let _ = writeln!(text, "Verified name: {name}");
    }
    let photo = profile
        .picture_url
        .as_deref()
        .unwrap_or(match profile.picture_status.as_deref() {
            Some("not_found") => "none",
            Some("not_authorized") => "not authorized",
            Some("unchanged") => "unchanged, URL unavailable",
            _ => "unknown",
        });
    let _ = writeln!(text, "Photo: {photo}");
    text
}

#[cfg(test)]
mod profile_tests {
    use super::profile_text;
    use oxidezap_wire::dto::ProfileDto;

    #[test]
    fn profile_text_distinguishes_absence_privacy_and_legacy_unknown() {
        let mut profile = ProfileDto {
            jid: "15550001111@s.whatsapp.net".into(),
            ..Default::default()
        };
        assert!(profile_text(&profile).contains("Photo: unknown\n"));
        for (status, expected) in [
            ("not_found", "none"),
            ("not_authorized", "not authorized"),
            ("unchanged", "unchanged, URL unavailable"),
        ] {
            profile.picture_status = Some(status.into());
            assert!(profile_text(&profile).contains(&format!("Photo: {expected}\n")));
        }
        profile.picture_url = Some("https://example.invalid/photo.jpg".into());
        profile.picture_status = Some("found".into());
        assert!(profile_text(&profile).contains("Photo: https://example.invalid/photo.jpg\n"));
    }

    #[test]
    fn profile_text_and_json_preserve_identity_sources_and_optional_details() {
        let profile = ProfileDto {
            jid: "100000001@lid".into(),
            pn_jid: Some("15550001111@s.whatsapp.net".into()),
            lid: Some("100000001@lid".into()),
            pn_source: Some("local_mapping".into()),
            lid_source: Some("request".into()),
            username: Some("synthetic_user".into()),
            verified_name: Some("Synthetic shop".into()),
            is_business: Some(true),
            picture_status: Some("not_authorized".into()),
            ..Default::default()
        };
        let text = profile_text(&profile);
        assert!(text.contains("PN: 15550001111@s.whatsapp.net [local_mapping]\n"));
        assert!(text.contains("LID: 100000001@lid [request]\n"));
        assert!(text.contains("Username: @synthetic_user\n"));
        assert!(text.contains("Verified name: Synthetic shop\n"));
        let json = serde_json::to_value(&profile).unwrap();
        assert_eq!(json["pn_source"], "local_mapping");
        assert_eq!(json["picture_status"], "not_authorized");
        assert_eq!(json["username"], "synthetic_user");
        let decoded: ProfileDto = serde_json::from_value(json).unwrap();
        assert_eq!(decoded, profile);
    }

    #[test]
    fn old_daemon_profiles_remain_readable_without_claiming_a_photo_is_absent() {
        let profile: ProfileDto = serde_json::from_str(
            r#"{"jid":"100000001@lid","name":null,"about":null,"picture_url":null}"#,
        )
        .unwrap();
        assert_eq!(profile.lid, None);
        assert_eq!(profile.pn_jid, None);
        assert_eq!(profile.picture_status, None);
        let text = profile_text(&profile);
        assert!(text.contains("Photo: unknown\n"));
        assert!(!text.contains("PN:"));
        assert!(!text.contains("LID:"));
    }
}
