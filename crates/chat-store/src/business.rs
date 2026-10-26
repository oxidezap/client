//! Bounded, shared projections of business messages for live traffic and history.
use oxidezap_core::{BusinessAction, BusinessButton, BusinessContent, BusinessKind};
use waproto::whatsapp as wa;

const TEXT_LIMIT: usize = 16 * 1024;
const BUTTON_LIMIT: usize = 20;
const LABEL_LIMIT: usize = 512;

fn text(value: Option<&str>) -> String {
    bounded_text(value, TEXT_LIMIT)
}
fn bounded_text(value: Option<&str>, limit: usize) -> String {
    let value = value.unwrap_or_default();
    if value.len() <= limit {
        return value.to_owned();
    }
    let mut end = limit;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}
fn card(kind: BusinessKind) -> BusinessContent {
    BusinessContent {
        kind,
        header: String::new(),
        body: String::new(),
        footer: String::new(),
        buttons: Vec::new(),
        notice: None,
    }
}
fn button(out: &mut BusinessContent, label: Option<&str>, action: BusinessAction) {
    if out.buttons.len() >= BUTTON_LIMIT {
        out.notice = Some("Additional choices are available on your phone.".into());
        return;
    }
    out.buttons.push(BusinessButton {
        label: bounded_text(
            label
                .filter(|s| !s.trim().is_empty())
                .or(Some("Unsupported button")),
            LABEL_LIMIT,
        ),
        action,
    });
}

pub fn hydrated_template(
    message: &wa::Message,
) -> Option<&wa::message::template_message::HydratedFourRowTemplate> {
    let template = template_of(message)?;
    template
        .hydrated_template
        .as_option()
        .or_else(|| match template.format.as_ref() {
            Some(wa::message::template_message::Format::HydratedFourRowTemplate(t)) => {
                Some(t.as_ref())
            }
            _ => None,
        })
}
fn template_of(message: &wa::Message) -> Option<&wa::message::TemplateMessage> {
    message.template_message.as_option().or_else(|| {
        message
            .highly_structured_message
            .as_option()?
            .hydrated_hsm
            .as_option()
    })
}
pub fn interactive_business(message: &wa::Message) -> Option<&wa::message::InteractiveMessage> {
    if let Some(template) = template_of(message)
        && let Some(wa::message::template_message::Format::InteractiveMessageTemplate(inner)) =
            template.format.as_ref()
    {
        return Some(inner);
    }
    message.interactive_message.as_option()
}

fn finish(message: &wa::Message, mut out: BusinessContent) -> BusinessContent {
    if let Some(media) = business_media(message) {
        if out.body.is_empty() {
            out.body = text(match &media {
                BusinessMedia::Image(m) => m.caption.as_deref(),
                BusinessMedia::Video(m) => m.caption.as_deref(),
                BusinessMedia::Document(m) => m.caption.as_deref(),
                BusinessMedia::Location(_) | BusinessMedia::Thumbnail(_) => None,
            });
        }
        if let BusinessMedia::Location(location) = media {
            let location = [
                text(location.name.as_deref()),
                text(location.address.as_deref()),
            ]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
            if !location.is_empty() {
                if !out.header.is_empty() {
                    out.header.push('\n');
                }
                out.header.push_str(&location);
            } else {
                out.notice = Some("Location attachment is available on your phone.".into());
            }
        }
    }
    if out.header.is_empty()
        && out.body.is_empty()
        && out.footer.is_empty()
        && out.buttons.is_empty()
        && out.notice.is_none()
    {
        out.notice = Some("Business message content is unavailable. Open it on your phone.".into());
    }
    out
}

/// Contexts attached to business content, including nested interactive templates.
/// A template can carry mentions on its outer context and quote linkage inside.
pub fn business_contexts(message: &wa::Message) -> impl Iterator<Item = &wa::ContextInfo> {
    let message = crate::normalized_message(message);
    let attachment_context = business_media(message).and_then(|media| match media {
        BusinessMedia::Image(m) => m.context_info.as_option(),
        BusinessMedia::Video(m) => m.context_info.as_option(),
        BusinessMedia::Document(m) => m.context_info.as_option(),
        BusinessMedia::Location(m) => m.context_info.as_option(),
        BusinessMedia::Thumbnail(_) => None,
    });
    [
        template_of(message).and_then(|t| t.context_info.as_option()),
        interactive_business(message).and_then(|m| m.context_info.as_option()),
        message
            .template_button_reply_message
            .as_option()
            .and_then(|m| m.context_info.as_option()),
        message
            .buttons_message
            .as_option()
            .and_then(|m| m.context_info.as_option()),
        message
            .buttons_response_message
            .as_option()
            .and_then(|m| m.context_info.as_option()),
        message
            .list_message
            .as_option()
            .and_then(|m| m.context_info.as_option()),
        message
            .list_response_message
            .as_option()
            .and_then(|m| m.context_info.as_option()),
        message
            .interactive_response_message
            .as_option()
            .and_then(|m| m.context_info.as_option()),
        attachment_context,
    ]
    .into_iter()
    .flatten()
}

pub fn business_content(message: &wa::Message) -> Option<BusinessContent> {
    let message = crate::normalized_message(message);
    if let Some(interactive) = interactive_business(message) {
        let mut out = card(if template_of(message).is_some() {
            BusinessKind::Template
        } else {
            BusinessKind::Interactive
        });
        if let Some(header) = interactive.header.as_option() {
            out.header = [
                text(header.title.as_deref()),
                text(header.subtitle.as_deref()),
            ]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        }
        out.body = text(interactive.body.as_option().and_then(|b| b.text.as_deref()));
        out.footer = text(
            interactive
                .footer
                .as_option()
                .and_then(|f| f.text.as_deref()),
        );
        use wa::message::interactive_message::InteractiveMessage;
        match interactive.interactive_message.as_ref() {
            Some(InteractiveMessage::NativeFlowMessage(flow)) => {
                for item in flow.buttons.iter().take(BUTTON_LIMIT + 1) {
                    // Parse only bounded JSON to read an explicit label. Never expose IDs,
                    // URLs, tokens, or unknown flow parameters as a fallback.
                    let params = item
                        .button_params_json
                        .as_deref()
                        .filter(|s| s.len() <= TEXT_LIMIT)
                        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
                    let label = params
                        .as_ref()
                        .and_then(|v| v.get("display_text"))
                        .and_then(|v| v.as_str());
                    let action = match item.name.as_deref() {
                        Some("quick_reply") => BusinessAction::Reply,
                        Some("cta_url") => BusinessAction::OpenLink,
                        Some("cta_call") => BusinessAction::Call,
                        _ => BusinessAction::NativeFlow,
                    };
                    button(&mut out, label, action);
                }
            }
            Some(InteractiveMessage::CarouselMessage(carousel)) => {
                // Keep readable card text even when its attachment/action cannot
                // be displayed. Do not recurse through nested carousel payloads.
                for card in carousel.cards.iter().take(10) {
                    for value in [
                        card.header.as_option().and_then(|h| h.title.as_deref()),
                        card.body.as_option().and_then(|b| b.text.as_deref()),
                        card.footer.as_option().and_then(|f| f.text.as_deref()),
                    ]
                    .into_iter()
                    .flatten()
                    {
                        if out.body.len() >= TEXT_LIMIT {
                            break;
                        }
                        if !out.body.is_empty() {
                            out.body.push('\n');
                        }
                        out.body.push_str(&bounded_text(
                            Some(value),
                            TEXT_LIMIT.saturating_sub(out.body.len()),
                        ));
                    }
                    if out.body.len() >= TEXT_LIMIT {
                        break;
                    }
                }
                out.notice =
                    Some("Carousel attachments and actions are available on your phone.".into());
            }
            Some(_) => {
                out.notice = Some("This business interaction is available on your phone.".into())
            }
            None => {}
        }
        if interactive
            .footer
            .as_option()
            .is_some_and(|footer| footer.media.is_some())
        {
            out.notice = Some("Footer attachment is available on your phone.".into());
        }
        if interactive
            .header
            .as_option()
            .is_some_and(|header| header.media.is_some())
            && business_media(message).is_none()
        {
            out.notice = Some("Header attachment is available on your phone.".into());
        }
        return Some(finish(message, out));
    }
    if let Some(template) = hydrated_template(message) {
        let mut out = card(BusinessKind::Template);
        if let Some(
            wa::message::template_message::hydrated_four_row_template::Title::HydratedTitleText(
                title,
            ),
        ) = template.title.as_ref()
        {
            out.header = text(Some(title));
        }
        out.body = text(template.hydrated_content_text.as_deref());
        out.footer = text(template.hydrated_footer_text.as_deref());
        use wa::hydrated_template_button::HydratedButton;
        for item in template.hydrated_buttons.iter().take(BUTTON_LIMIT + 1) {
            let (label, action) = match item.hydrated_button.as_ref() {
                Some(HydratedButton::QuickReplyButton(b)) => {
                    (b.display_text.as_deref(), BusinessAction::Reply)
                }
                Some(HydratedButton::UrlButton(b)) => {
                    (b.display_text.as_deref(), BusinessAction::OpenLink)
                }
                Some(HydratedButton::CallButton(b)) => {
                    (b.display_text.as_deref(), BusinessAction::Call)
                }
                _ => (None, BusinessAction::Unsupported),
            };
            button(&mut out, label, action);
        }
        return Some(finish(message, out));
    }
    if template_of(message).is_some() || message.highly_structured_message.is_set() {
        let mut out = card(BusinessKind::Template);
        out.notice = Some("This template requires translation on your phone.".into());
        return Some(finish(message, out));
    }
    if let Some(buttons) = message.buttons_message.as_option() {
        let mut out = card(BusinessKind::Buttons);
        if let Some(wa::message::buttons_message::Header::Text(title)) = buttons.header.as_ref() {
            out.header = text(Some(title));
        }
        out.body = text(buttons.content_text.as_deref());
        out.footer = text(buttons.footer_text.as_deref());
        for item in buttons.buttons.iter().take(BUTTON_LIMIT + 1) {
            button(
                &mut out,
                item.button_text
                    .as_option()
                    .and_then(|t| t.display_text.as_deref()),
                if item.native_flow_info.is_set() {
                    BusinessAction::NativeFlow
                } else {
                    BusinessAction::Reply
                },
            );
        }
        return Some(finish(message, out));
    }
    if let Some(list) = message.list_message.as_option() {
        let mut out = card(BusinessKind::List);
        out.header = text(list.title.as_deref());
        out.body = text(list.description.as_deref());
        out.footer = text(list.footer_text.as_deref());
        for section in list.sections.iter().take(BUTTON_LIMIT) {
            for row in section.rows.iter().take(BUTTON_LIMIT + 1) {
                let label = [
                    text(section.title.as_deref()),
                    text(row.title.as_deref()),
                    text(row.description.as_deref()),
                ]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
                button(&mut out, Some(&label), BusinessAction::Reply);
                if out.buttons.len() >= BUTTON_LIMIT {
                    break;
                }
            }
        }
        if list.sections.len() > BUTTON_LIMIT
            || list
                .sections
                .iter()
                .take(BUTTON_LIMIT)
                .fold(0usize, |count, section| {
                    count.saturating_add(section.rows.len())
                })
                > BUTTON_LIMIT
        {
            out.notice = Some("Additional choices are available on your phone.".into());
        }
        if out.buttons.is_empty() {
            button(
                &mut out,
                list.button_text.as_deref(),
                BusinessAction::Unsupported,
            );
        }
        return Some(finish(message, out));
    }
    let response = message
        .template_button_reply_message
        .as_option()
        .and_then(|r| r.selected_display_text.as_deref())
        .or_else(|| {
            message
                .buttons_response_message
                .as_option()
                .and_then(|r| match r.response.as_ref() {
                    Some(wa::message::buttons_response_message::Response::SelectedDisplayText(
                        s,
                    )) => Some(s.as_str()),
                    _ => None,
                })
        })
        .or_else(|| {
            message
                .list_response_message
                .as_option()
                .and_then(|r| r.title.as_deref())
        })
        .or_else(|| {
            message
                .interactive_response_message
                .as_option()
                .and_then(|r| r.body.as_option())
                .and_then(|b| b.text.as_deref())
        });
    response.map(|body| {
        let mut out = card(BusinessKind::Response);
        out.body = text(Some(body));
        out
    })
}

/// Borrow the attachment without copying a protobuf tree or fetching anything.
pub enum BusinessMedia<'a> {
    Image(&'a wa::message::ImageMessage),
    Video(&'a wa::message::VideoMessage),
    Document(&'a wa::message::DocumentMessage),
    Location(&'a wa::message::LocationMessage),
    Thumbnail(&'a [u8]),
}
pub fn business_media(message: &wa::Message) -> Option<BusinessMedia<'_>> {
    if let Some(interactive) = interactive_business(message) {
        use wa::message::interactive_message::header::Media;
        let header = interactive.header.as_option()?;
        return match header.media.as_ref()? {
            Media::JpegThumbnail(bytes) if bytes.len() <= 512 * 1024 => {
                Some(BusinessMedia::Thumbnail(bytes))
            }
            Media::ImageMessage(m) if header.has_media_attachment == Some(true) => {
                Some(BusinessMedia::Image(m))
            }
            Media::VideoMessage(m) if header.has_media_attachment == Some(true) => {
                Some(BusinessMedia::Video(m))
            }
            Media::DocumentMessage(m) if header.has_media_attachment == Some(true) => {
                Some(BusinessMedia::Document(m))
            }
            Media::LocationMessage(m) if header.has_media_attachment == Some(true) => {
                Some(BusinessMedia::Location(m))
            }
            _ => None,
        };
    }
    if let Some(template) = hydrated_template(message) {
        use wa::message::template_message::hydrated_four_row_template::Title;
        return match template.title.as_ref()? {
            Title::ImageMessage(m) => Some(BusinessMedia::Image(m)),
            Title::VideoMessage(m) => Some(BusinessMedia::Video(m)),
            Title::DocumentMessage(m) => Some(BusinessMedia::Document(m)),
            Title::LocationMessage(m) => Some(BusinessMedia::Location(m)),
            _ => None,
        };
    }
    if let Some(template) = template_of(message)
        && let Some(wa::message::template_message::Format::FourRowTemplate(template)) =
            template.format.as_ref()
    {
        use wa::message::template_message::four_row_template::Title;
        return match template.title.as_ref()? {
            Title::ImageMessage(m) => Some(BusinessMedia::Image(m)),
            Title::VideoMessage(m) => Some(BusinessMedia::Video(m)),
            Title::DocumentMessage(m) => Some(BusinessMedia::Document(m)),
            Title::LocationMessage(m) => Some(BusinessMedia::Location(m)),
            _ => None,
        };
    }
    if let Some(buttons) = message.buttons_message.as_option() {
        use wa::message::buttons_message::Header;
        return match buttons.header.as_ref()? {
            Header::ImageMessage(m) => Some(BusinessMedia::Image(m)),
            Header::VideoMessage(m) => Some(BusinessMedia::Video(m)),
            Header::DocumentMessage(m) => Some(BusinessMedia::Document(m)),
            Header::LocationMessage(m) => Some(BusinessMedia::Location(m)),
            _ => None,
        };
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use buffa::MessageField;
    use wa::message::template_message::hydrated_four_row_template::Title;
    use wa::message::template_message::{Format, HydratedFourRowTemplate};

    fn hydrated() -> wa::Message {
        let mut template = HydratedFourRowTemplate::default();
        template.title = Some(Title::HydratedTitleText("Order update".into()));
        template.hydrated_content_text = Some("Hello *customer*\nYour order is ready.".into());
        template.hydrated_footer_text = Some("Example shop".into());
        let mut reply = wa::hydrated_template_button::HydratedQuickReplyButton::default();
        reply.display_text = Some("Thanks".into());
        reply.id = Some("private-response-token".into());
        let mut button = wa::HydratedTemplateButton::default();
        button.hydrated_button =
            Some(wa::hydrated_template_button::HydratedButton::QuickReplyButton(reply.into()));
        template.hydrated_buttons.push(button);
        let mut envelope = wa::message::TemplateMessage::default();
        envelope.format = Some(Format::HydratedFourRowTemplate(template.into()));
        let mut message = wa::Message::default();
        message.template_message = MessageField::some(envelope);
        message
    }

    #[test]
    fn hydrated_preserves_formatting_and_never_projects_response_ids() {
        let out = business_content(&hydrated()).unwrap();
        assert_eq!(out.header, "Order update");
        assert_eq!(out.body, "Hello *customer*\nYour order is ready.");
        assert_eq!(out.footer, "Example shop");
        assert_eq!(out.buttons[0].label, "Thanks");
        assert_eq!(out.buttons[0].action, BusinessAction::Reply);
        assert!(!out.summary().contains("private-response-token"));
    }

    #[test]
    fn interactive_template_body_is_visible_without_hydrated_rows() {
        let mut body = wa::message::interactive_message::Body::default();
        body.text = Some("Your delivery is ready".into());
        let mut interactive = wa::message::InteractiveMessage::default();
        interactive.body = MessageField::some(body);
        let mut envelope = wa::message::TemplateMessage::default();
        envelope.format = Some(Format::InteractiveMessageTemplate(interactive.into()));
        let mut message = wa::Message::default();
        message.template_message = MessageField::some(envelope);
        assert_eq!(
            business_content(&message).unwrap().body,
            "Your delivery is ready"
        );
        assert_eq!(
            crate::materialize::extract_text(&message).as_deref(),
            Some("Your delivery is ready")
        );
    }

    #[test]
    fn nested_wrappers_and_encoded_reload_keep_the_same_card() {
        let expected = business_content(&hydrated()).unwrap();
        let mut message = hydrated();
        for _ in 0..5 {
            let mut wrapper = wa::message::FutureProofMessage::default();
            wrapper.message = MessageField::some(message);
            message = wa::Message::default();
            message.ephemeral_message = MessageField::some(wrapper);
        }
        let bytes = waproto::codec::message_to_vec(&message);
        let decoded = waproto::codec::message_decode(&bytes).unwrap();
        assert_eq!(business_content(&decoded), Some(expected));
        assert_eq!(
            crate::MessageKind::of(&decoded),
            crate::MessageKind::Template
        );
    }

    #[test]
    fn missing_translation_has_a_safe_explanation() {
        let mut message = wa::Message::default();
        message.template_message = MessageField::some(wa::message::TemplateMessage::default());
        let out = business_content(&message).unwrap();
        assert!(out.summary().contains("translation"));
        assert!(out.body.is_empty());
    }

    #[test]
    fn long_unicode_and_choice_counts_are_bounded() {
        let mut message = hydrated();
        let envelope = message.template_message.as_option_mut().unwrap();
        let Some(Format::HydratedFourRowTemplate(template)) = envelope.format.as_mut() else {
            panic!()
        };
        template.hydrated_content_text = Some("🙂".repeat(TEXT_LIMIT));
        template.hydrated_buttons = vec![wa::HydratedTemplateButton::default(); 100];
        let out = business_content(&message).unwrap();
        assert!(out.body.len() <= TEXT_LIMIT + '…'.len_utf8());
        assert_eq!(out.buttons.len(), BUTTON_LIMIT);
        assert!(out.notice.is_some());
    }

    #[test]
    fn native_flow_only_reads_explicit_labels_from_bounded_valid_json() {
        use wa::message::interactive_message::{InteractiveMessage, NativeFlowMessage};
        let mut flow = NativeFlowMessage::default();
        for params in [
            r#"{"display_text":"Track order","url":"https://example.invalid/private","id":"secret"}"#,
            "{malformed",
            r#"{"token":"hidden"}"#,
        ] {
            let mut button =
                wa::message::interactive_message::native_flow_message::NativeFlowButton::default();
            button.name = Some("cta_url".into());
            button.button_params_json = Some(params.into());
            flow.buttons.push(button);
        }
        let mut interactive = wa::message::InteractiveMessage::default();
        interactive.interactive_message = Some(InteractiveMessage::NativeFlowMessage(flow.into()));
        let mut message = wa::Message::default();
        message.interactive_message = MessageField::some(interactive);
        let out = business_content(&message).unwrap();
        assert_eq!(out.buttons[0].label, "Track order");
        assert_eq!(out.buttons[1].label, "Unsupported button");
        let summary = out.summary();
        for hidden in ["secret", "hidden", "https://", "malformed"] {
            assert!(!summary.contains(hidden));
        }
    }

    #[test]
    fn business_header_attachment_is_borrowed_without_media_fetch() {
        let mut message = hydrated();
        let Some(Format::HydratedFourRowTemplate(template)) = message
            .template_message
            .as_option_mut()
            .unwrap()
            .format
            .as_mut()
        else {
            panic!()
        };
        let mut image = wa::message::ImageMessage::default();
        image.jpeg_thumbnail = Some(vec![1, 2, 3]);
        template.title = Some(Title::ImageMessage(image.into()));
        assert!(matches!(
            business_media(&message),
            Some(BusinessMedia::Image(_))
        ));
        assert!(!business_content(&message).unwrap().body.is_empty());
    }
}

#[cfg(test)]
mod related_tests {
    use super::*;
    use buffa::MessageField;

    #[test]
    fn list_choices_keep_display_text_and_descriptions_without_ids() {
        let mut row = wa::message::list_message::Row::default();
        row.title = Some("Morning".into());
        row.description = Some("Before noon".into());
        row.row_id = Some("private-row-id".into());
        let mut section = wa::message::list_message::Section::default();
        section.rows.push(row);
        let mut list = wa::message::ListMessage::default();
        list.title = Some("Delivery".into());
        list.description = Some("Choose a time".into());
        list.sections.push(section);
        let mut message = wa::Message::default();
        message.list_message = MessageField::some(list);
        let out = business_content(&message).unwrap();
        assert_eq!(out.kind, BusinessKind::List);
        assert_eq!(out.buttons[0].label, "Morning\nBefore noon");
        assert!(!out.summary().contains("private-row-id"));
    }

    #[test]
    fn legacy_buttons_keep_their_header_footer_and_display_label() {
        let mut label = wa::message::buttons_message::button::ButtonText::default();
        label.display_text = Some("Confirm".into());
        let mut button = wa::message::buttons_message::Button::default();
        button.button_text = MessageField::some(label);
        button.button_id = Some("response-token".into());
        let mut buttons = wa::message::ButtonsMessage::default();
        buttons.header = Some(wa::message::buttons_message::Header::Text(
            "Delivery".into(),
        ));
        buttons.content_text = Some("Confirm delivery".into());
        buttons.footer_text = Some("Example shop".into());
        buttons.buttons.push(button);
        let mut message = wa::Message::default();
        message.buttons_message = MessageField::some(buttons);
        let out = business_content(&message).unwrap();
        assert_eq!(out.kind, BusinessKind::Buttons);
        assert_eq!(out.header, "Delivery");
        assert_eq!(out.footer, "Example shop");
        assert_eq!(out.buttons[0].label, "Confirm");
        assert!(!out.summary().contains("response-token"));
    }

    #[test]
    fn template_replies_show_the_selected_label_only() {
        let mut reply = wa::message::TemplateButtonReplyMessage::default();
        reply.selected_display_text = Some("Confirmed".into());
        reply.selected_id = Some("response-token".into());
        let mut message = wa::Message::default();
        message.template_button_reply_message = MessageField::some(reply);
        let out = business_content(&message).unwrap();
        assert_eq!(out.body, "Confirmed");
        assert_eq!(out.kind, BusinessKind::Response);
        assert!(!out.summary().contains("response-token"));
    }

    #[test]
    fn unsupported_carousel_preserves_readable_card_bodies() {
        let mut body = wa::message::interactive_message::Body::default();
        body.text = Some("Example product description".into());
        let mut card = wa::message::InteractiveMessage::default();
        card.body = MessageField::some(body);
        let mut carousel = wa::message::interactive_message::CarouselMessage::default();
        carousel.cards.push(card);
        let mut interactive = wa::message::InteractiveMessage::default();
        interactive.interactive_message = Some(
            wa::message::interactive_message::InteractiveMessage::CarouselMessage(carousel.into()),
        );
        let mut message = wa::Message::default();
        message.interactive_message = MessageField::some(interactive);
        let out = business_content(&message).unwrap();
        assert_eq!(out.body, "Example product description");
        assert!(out.notice.is_some());
    }
}
