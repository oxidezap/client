//! Display-only business content. No response IDs, flow JSON, or executable actions.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BusinessContent {
    pub kind: BusinessKind,
    pub header: String,
    pub body: String,
    pub footer: String,
    pub buttons: Vec<BusinessButton>,
    /// A readable limitation, never raw protobuf identifiers or payloads.
    pub notice: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BusinessKind {
    Template,
    Buttons,
    List,
    Interactive,
    Response,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BusinessButton {
    pub label: String,
    pub action: BusinessAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BusinessAction {
    Reply,
    OpenLink,
    Call,
    NativeFlow,
    Unsupported,
}

impl BusinessAction {
    pub fn unavailable_reason(self) -> &'static str {
        match self {
            Self::Reply => "Button replies are not supported by this client.",
            Self::OpenLink => "Opening business button links is not supported by this client.",
            Self::Call => "Calling business button numbers is not supported by this client.",
            Self::NativeFlow => "This interactive action must be completed on your phone.",
            Self::Unsupported => "This button action is not supported by this client.",
        }
    }
}

impl BusinessContent {
    /// Human-readable output for CLI clients which do not draw a business card.
    pub fn summary(&self) -> String {
        let mut lines: Vec<&str> = [&self.header, &self.body, &self.footer]
            .into_iter()
            .map(String::as_str)
            .filter(|s| !s.is_empty())
            .collect();
        lines.extend(self.buttons.iter().map(|button| button.label.as_str()));
        lines.extend(self.notice.as_deref());
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn business_cards_cross_daemon_serialization_and_old_messages_still_decode() {
        let mut message = crate::ChatMessage::new_incoming(
            "synthetic".into(),
            "example@lid".into(),
            "Ready".into(),
        );
        message.business = Some(BusinessContent {
            kind: BusinessKind::Template,
            header: "Order".into(),
            body: "Ready".into(),
            footer: "Example shop".into(),
            buttons: vec![BusinessButton {
                label: "Thanks".into(),
                action: BusinessAction::Reply,
            }],
            notice: None,
        });
        let encoded = serde_json::to_value(&message).unwrap();
        assert_eq!(
            serde_json::from_value::<crate::ChatMessage>(encoded.clone()).unwrap(),
            message
        );
        let mut old = encoded;
        old.as_object_mut().unwrap().remove("business");
        assert!(
            serde_json::from_value::<crate::ChatMessage>(old)
                .unwrap()
                .business
                .is_none()
        );
        message.status = crate::MessageStatus::Failed;
        message.is_from_me = true;
        assert!(
            message.resend().is_none(),
            "a business body must not become a button response or resend"
        );
    }
}
