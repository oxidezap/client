//! Platform-sensitive keyboard geometry.

use gpui::{Pixels, Window};

/// The key that opens message search in the active conversation.
pub fn conversation_search_key() -> &'static str {
    imp::conversation_search_key()
}

/// The keys that page through the active conversation.
pub fn chat_history_page_keys() -> (&'static str, &'static str) {
    imp::chat_history_page_keys()
}

/// The distance used by a chat history page movement.
pub fn page_scroll_distance(window: &Window) -> Pixels {
    imp::page_scroll_distance(window)
}

#[cfg(not(target_family = "wasm"))]
mod imp {
    use super::*;

    pub(super) fn conversation_search_key() -> &'static str {
        "secondary-f"
    }

    pub(super) fn chat_history_page_keys() -> (&'static str, &'static str) {
        ("pageup", "pagedown")
    }

    pub(super) fn page_scroll_distance(window: &Window) -> Pixels {
        window.viewport_size().height
    }
}

#[cfg(target_family = "wasm")]
mod imp {
    use super::*;

    pub(super) fn conversation_search_key() -> &'static str {
        "secondary-f"
    }

    pub(super) fn chat_history_page_keys() -> (&'static str, &'static str) {
        ("pageup", "pagedown")
    }

    pub(super) fn page_scroll_distance(window: &Window) -> Pixels {
        window.viewport_size().height
    }
}

#[cfg(test)]
mod tests {
    use super::{chat_history_page_keys, conversation_search_key};

    #[test]
    fn conversation_search_uses_the_platform_secondary_modifier() {
        assert_eq!(conversation_search_key(), "secondary-f");
    }

    #[test]
    fn history_page_keys_are_directional() {
        assert_eq!(chat_history_page_keys(), ("pageup", "pagedown"));
    }
}
