//! The readable part of a shared contact, reused by live rendering, history,
//! previews and search. The original vCard stays in the stored protobuf.

use waproto::whatsapp as wa;

/// Project single and multiple shared contacts to names and phone numbers.
/// Unknown vCard fields are retained in the proto rather than displayed as
/// raw markup. A nameless/empty card still has a readable contact label.
pub fn shared_contacts_text(message: &wa::Message) -> Option<String> {
    let base = crate::normalized_message(message);
    if let Some(contact) = base.contact_message.as_option() {
        return Some(contact_text(contact));
    }
    let contacts = base.contacts_array_message.as_option()?;
    if contacts.contacts.is_empty() {
        return Some(label(contacts.display_name.as_deref()).unwrap_or_else(|| "Contacts".into()));
    }
    Some(
        contacts
            .contacts
            .iter()
            .map(contact_text)
            .collect::<Vec<_>>()
            .join("\n\n"),
    )
}

fn contact_text(contact: &wa::message::ContactMessage) -> String {
    let mut lines: Vec<String> = Vec::new();
    // RFC 2425/6350 folding removes exactly the first space or tab of a
    // continuation line, before property parsing and vCard text unescaping.
    for line in contact.vcard.as_deref().unwrap_or_default().lines() {
        if let Some(tail) = line.strip_prefix([' ', '\t'])
            && let Some(last) = lines.last_mut()
        {
            last.push_str(tail);
        } else {
            lines.push(line.into());
        }
    }
    let mut formatted_name = None;
    let mut structured_name = None;
    let mut phones = Vec::new();
    for line in lines {
        let Some((property, value)) = line.split_once(':') else {
            continue;
        };
        // iOS commonly groups properties as item1.TEL / item1.X-ABLabel.
        let property = property.split(';').next().unwrap_or_default();
        let property = property.rsplit('.').next().unwrap_or_default();
        match property.to_ascii_uppercase().as_str() {
            "FN" => formatted_name = label(Some(&unescape(value))),
            "N" => {
                let fields = split_name(value);
                // N is family;given;additional;prefix;suffix, not display order.
                structured_name = label(Some(
                    &[3, 1, 2, 0, 4]
                        .into_iter()
                        .filter_map(|i| fields.get(i))
                        .filter(|s| !s.is_empty())
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(" "),
                ));
            }
            "TEL" => {
                let value = unescape(value);
                let value = if value
                    .get(..4)
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("tel:"))
                {
                    &value[4..]
                } else {
                    &value
                };
                if let Some(phone) = label(Some(value))
                    && !phones.contains(&phone)
                {
                    phones.push(phone);
                }
            }
            _ => {}
        }
    }
    let mut text = label(contact.display_name.as_deref())
        .or(formatted_name)
        .or(structured_name)
        .unwrap_or_else(|| "Contact".into());
    for phone in phones {
        text.push('\n');
        text.push_str(&phone);
    }
    text
}

fn label(value: Option<&str>) -> Option<String> {
    let text = value?.split_whitespace().collect::<Vec<_>>().join(" ");
    let text: String = text.chars().filter(|c| !c.is_control()).collect();
    (!text.is_empty()).then_some(text)
}

fn unescape(value: &str) -> String {
    let mut chars = value.chars();
    let mut text = String::new();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n' | 'N') => text.push('\n'),
                Some(ch) => text.push(ch),
                None => text.push('\\'),
            }
        } else {
            text.push(ch);
        }
    }
    text
}

fn split_name(value: &str) -> Vec<String> {
    let mut fields = vec![String::new()];
    let mut escaped = false;
    for ch in value.chars() {
        if ch == ';' && !escaped {
            fields.push(String::new());
        } else {
            fields.last_mut().expect("one field exists").push(ch);
        }
        escaped = ch == '\\' && !escaped;
    }
    fields.into_iter().map(|field| unescape(&field)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use buffa::MessageField;

    #[test]
    fn contact_names_phones_folding_and_vcard_escapes_are_readable() {
        let contact = wa::message::ContactMessage {
            vcard: Some("BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Example\\, Contact\r\nitem1.TEL;waid=559900000001:+55 99 0000-\r\n 0001\r\nTEL;VALUE=uri:tel:+55 99 0000-0001\r\nEND:VCARD".into()),
            ..Default::default()
        };
        assert_eq!(contact_text(&contact), "Example, Contact\n+55 99 0000-0001");
        let named = wa::message::ContactMessage {
            display_name: Some("Shared name".into()),
            ..contact
        };
        assert_eq!(contact_text(&named), "Shared name\n+55 99 0000-0001");
    }

    #[test]
    fn multiple_contacts_and_structured_names_do_not_drop_cards() {
        let contacts = wa::Message {
            contacts_array_message: MessageField::some(wa::message::ContactsArrayMessage {
                contacts: vec![
                    wa::message::ContactMessage {
                        vcard: Some("N:Family\\;Name;Given;;;\nTEL:+559900000001".into()),
                        ..Default::default()
                    },
                    wa::message::ContactMessage::default(),
                ],
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            shared_contacts_text(&contacts).as_deref(),
            Some("Given Family;Name\n+559900000001\n\nContact")
        );
        assert!(shared_contacts_text(&wa::Message::default()).is_none());
    }
}
