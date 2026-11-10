//! Localized projections of domain facts. Stored messages and wire types stay unchanged.
use super::Language;
use oxidezap_core::{
    CallOutcome, CallRecord, ComposingKind, GroupMembership, MembershipAction, NoticeParticipant,
    QuotedKind, QuotedMessage, TypingSummary, format_duration,
};

impl Language {
    /// Faults currently carry developer-owned static prose rather than a kind.
    /// Unknown future faults retain their original text as an English fallback.
    pub fn fault_labels(self, fault: &oxidezap_core::Fault) -> (&'static str, &'static str) {
        use super::TextKey;
        match fault.headline {
            "Can't reach WhatsApp" => (
                self.text(TextKey::FaultUnreachableTitle),
                self.text(TextKey::FaultUnreachableBody),
            ),
            "Reconnecting to the background service" => (
                self.text(TextKey::FaultFellBehindTitle),
                self.text(TextKey::FaultFellBehindBody),
            ),
            "This window cannot read what the background service sent" => (
                self.text(TextKey::FaultOversizedTitle),
                self.text(TextKey::FaultOversizedBody),
            ),
            "This window and the background service disagree" => (
                self.text(TextKey::FaultMismatchedTitle),
                self.text(TextKey::FaultMismatchedBody),
            ),
            _ => (fault.headline, fault.body),
        }
    }

    pub fn log_note(self, level: oxidezap_core::LogLevel) -> &'static str {
        use super::TextKey;
        use oxidezap_core::LogLevel;
        self.text(match level {
            LogLevel::Off => TextKey::LogOffNote,
            LogLevel::Error => TextKey::LogErrorNote,
            LogLevel::Warn => TextKey::LogWarnNote,
            LogLevel::Info => TextKey::LogInfoNote,
            LogLevel::Debug => TextKey::LogDebugNote,
            LogLevel::Trace => TextKey::LogTraceNote,
        })
    }

    pub fn typing(self, summary: &TypingSummary, is_group: bool, compact: bool) -> String {
        if self != Language::Portuguese {
            return if compact {
                summary.compact_label(is_group)
            } else {
                summary.label()
            };
        }
        let verb = match (summary.kind, summary.total == 1) {
            (ComposingKind::Text, true) => "está digitando",
            (ComposingKind::Text, false) => "estão digitando",
            (ComposingKind::Audio, true) => "está gravando áudio",
            (ComposingKind::Audio, false) => "estão gravando áudio",
        };
        if !is_group {
            return format!("{verb}…");
        }
        let mut names = summary.names().collect::<Vec<_>>().join(", ");
        if summary.overflow() > 0 {
            names.push_str(&format!(" +{}", summary.overflow()));
        }
        format!("{names} {verb}{}", if compact { "…" } else { "" })
    }

    pub fn call_title(self, record: &CallRecord) -> String {
        if self != Language::Portuguese {
            return record.title();
        }
        match (record.is_video, record.outcome, record.is_outgoing) {
            (true, CallOutcome::Missed, false) => "Chamada de vídeo perdida",
            (false, CallOutcome::Missed, false) => "Chamada de voz perdida",
            (true, CallOutcome::Missed, true) => "Chamada de vídeo, sem resposta",
            (false, CallOutcome::Missed, true) => "Chamada de voz, sem resposta",
            (true, _, _) => "Chamada de vídeo",
            (false, _, _) => "Chamada de voz",
        }
        .to_owned()
    }

    pub fn call_summary(self, record: &CallRecord) -> String {
        let title = self.call_title(record);
        match record.outcome {
            CallOutcome::Completed(secs) => format!("{title} · {}", format_duration(secs)),
            _ => title,
        }
    }

    pub fn call_detail(self, record: &CallRecord) -> String {
        if self != Language::Portuguese {
            return record.detail();
        }
        let direction = if record.is_outgoing {
            "realizada"
        } else {
            "recebida"
        };
        match record.outcome {
            CallOutcome::Completed(secs) => format!("{direction} · {}", format_duration(secs)),
            CallOutcome::Missed => "toque para retornar a chamada".to_owned(),
            CallOutcome::Declined => format!("{direction} · recusada"),
        }
    }

    pub fn quoted_kind(self, kind: QuotedKind) -> &'static str {
        match kind {
            QuotedKind::Image => self.text(super::TextKey::Photo),
            QuotedKind::Video => self.text(super::TextKey::Video),
            QuotedKind::Audio => self.text(super::TextKey::VoiceMessage),
            QuotedKind::Document => self.text(super::TextKey::Document),
            QuotedKind::Sticker => self.text(super::TextKey::StickerText),
        }
    }

    pub fn quoted_summary(self, quoted: &QuotedMessage) -> &str {
        if !quoted.preview.is_empty() {
            return &quoted.preview;
        }
        quoted
            .kind
            .map(|kind| self.quoted_kind(kind))
            .unwrap_or_default()
    }

    fn participant(self, person: &NoticeParticipant) -> String {
        if person.is_self {
            return "você".to_owned();
        }
        if let Some(name) = person
            .label
            .as_deref()
            .or(person.offered_name.as_deref())
            .filter(|name| !name.trim().is_empty())
        {
            return name.to_owned();
        }
        let fallback = person.fallback_label();
        if fallback == "Unknown contact" {
            self.text(super::TextKey::UnknownContact).to_owned()
        } else {
            fallback
        }
    }

    pub fn membership(self, event: &GroupMembership) -> String {
        if self != Language::Portuguese {
            return event.summary();
        }
        let who = event
            .actor
            .as_ref()
            .map(|person| {
                if person.is_self {
                    "Você".to_owned()
                } else {
                    self.participant(person)
                }
            })
            .unwrap_or_else(|| "Alguém".to_owned());
        if event.action == MembershipAction::Remove
            && event.participants.len() == 1
            && event
                .actor
                .as_ref()
                .is_some_and(|actor| actor.same_person(&event.participants[0]))
        {
            return format!("{who} saiu");
        }
        let names = match event.participants.as_slice() {
            [] => "alguém".to_owned(),
            [one] => self.participant(one),
            [one, two] => format!("{} e {}", self.participant(one), self.participant(two)),
            [one, rest @ ..] => format!("{} e mais {} pessoas", self.participant(one), rest.len()),
        };
        match event.action {
            MembershipAction::Add => format!("{who} adicionou {names}"),
            MembershipAction::Remove => format!("{who} removeu {names}"),
            MembershipAction::Promote => {
                let role = if event.participants.len() > 1 {
                    "administradores"
                } else {
                    "administrador"
                };
                format!("{who} promoveu {names} a {role}")
            }
            MembershipAction::Demote => {
                format!("{who} removeu a função de administrador de {names}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidezap_core::Typist;

    #[test]
    fn translated_faults_follow_the_domain_constructors() {
        use oxidezap_core::Fault;
        for fault in [
            Fault::unreachable("synthetic"),
            Fault::fell_behind("synthetic"),
            Fault::oversized("synthetic"),
            Fault::mismatched("synthetic"),
        ] {
            assert_eq!(
                Language::English.fault_labels(&fault),
                (fault.headline, fault.body)
            );
            let (headline, body) = Language::Portuguese.fault_labels(&fault);
            assert_ne!(headline, fault.headline);
            assert_ne!(body, fault.body);
        }
        let unknown = Fault {
            headline: "A future failure",
            body: "Keep this fallback",
            detail: String::new(),
            recovery: oxidezap_core::Recovery::Nothing,
        };
        assert_eq!(
            Language::Portuguese.fault_labels(&unknown),
            (unknown.headline, unknown.body)
        );
    }

    #[test]
    fn diagnostic_guidance_tracks_every_log_level() {
        for level in oxidezap_core::LogLevel::ALL {
            assert_eq!(Language::English.log_note(level), level.note());
            assert_ne!(Language::Portuguese.log_note(level), level.note());
        }
    }

    #[test]
    fn typing_covers_audio_plural_and_overflow_without_changing_names() {
        let mut summary = TypingSummary {
            typists: vec![Typist {
                jid: "peer@example.invalid".into(),
                name: "Photo".into(),
            }],
            total: 1,
            kind: ComposingKind::Text,
        };
        assert_eq!(Language::English.typing(&summary, false, true), "typing…");
        assert_eq!(
            Language::Portuguese.typing(&summary, false, true),
            "está digitando…"
        );
        assert_eq!(
            Language::Portuguese.typing(&summary, true, false),
            "Photo está digitando"
        );
        summary.kind = ComposingKind::Audio;
        summary.total = 2;
        assert_eq!(
            Language::Portuguese.typing(&summary, true, true),
            "Photo +1 estão gravando áudio…"
        );
    }

    #[test]
    fn call_records_cover_outcomes_and_direction() {
        let mut record = CallRecord {
            is_video: false,
            is_outgoing: false,
            outcome: CallOutcome::Missed,
        };
        assert_eq!(Language::English.call_title(&record), record.title());
        assert_eq!(
            Language::Portuguese.call_summary(&record),
            "Chamada de voz perdida"
        );
        record.is_outgoing = true;
        record.is_video = true;
        assert_eq!(
            Language::Portuguese.call_title(&record),
            "Chamada de vídeo, sem resposta"
        );
        record.outcome = CallOutcome::Completed(65);
        assert_eq!(
            Language::Portuguese.call_summary(&record),
            "Chamada de vídeo · 1:05"
        );
        assert_eq!(
            Language::Portuguese.call_detail(&record),
            "realizada · 1:05"
        );
        record.outcome = CallOutcome::Declined;
        assert_eq!(
            Language::Portuguese.call_detail(&record),
            "realizada · recusada"
        );
    }

    #[test]
    fn captions_and_contact_names_are_never_used_as_translation_keys() {
        let mut quote = QuotedMessage {
            message_id: "synthetic".into(),
            sender: "peer@example.invalid".into(),
            sender_name: "You".into(),
            preview: "Photo".into(),
            kind: Some(QuotedKind::Image),
        };
        assert_eq!(Language::Portuguese.quoted_summary(&quote), "Photo");
        quote.preview.clear();
        assert_eq!(Language::Portuguese.quoted_summary(&quote), "Foto");
        assert_eq!(Language::English.quoted_summary(&quote), "Photo");
        let person = NoticeParticipant {
            jid: "peer@example.invalid".into(),
            phone_jid: None,
            lid: None,
            offered_name: None,
            label: Some("You".into()),
            is_self: false,
            canonical_jid: None,
        };
        let mut event = GroupMembership {
            action: MembershipAction::Add,
            actor: Some(person.clone()),
            participants: vec![person],
        };
        assert_eq!(Language::Portuguese.membership(&event), "You adicionou You");
        event.actor.as_mut().unwrap().is_self = true;
        event.participants[0].label = None;
        assert_eq!(
            Language::Portuguese.membership(&event),
            "Você adicionou Contato desconhecido"
        );
    }
}
