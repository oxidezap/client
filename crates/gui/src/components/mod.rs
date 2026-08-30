//! Reusable UI components

mod avatar;
mod call_card;
mod chat_header;
mod chat_item;
mod chat_list;
mod conversation_search;
mod empty_state;
mod icons;
mod input_area_view;
mod media_viewer;
pub mod message_bubble;
mod message_list;
mod nav_rail;
pub mod plugin_ui;
mod rich_text;
mod status;
mod status_ticks;

pub use avatar::Avatar;
pub use call_card::render_call_card;
pub use chat_header::render_chat_header;
pub use chat_item::render_chat_item;
pub use chat_list::{AccountSummary, ChatListProps, render_chat_list};
pub use conversation_search::render_conversation_search;
pub use empty_state::EmptyState;
pub use icons::ProductIcon;
pub use input_area_view::{InputAreaEvent, InputAreaView, ReplyDraft};
pub use media_viewer::{ViewerProps, render_media_viewer};
pub use message_bubble::render_message_bubble;
pub use message_list::{new_timeline_state, render_message_list};
pub use nav_rail::render_nav_rail;
pub use plugin_ui::PluginContext;
pub use rich_text::render_rich_text;
pub use status::{
    StatusListProps, StatusSelection, StatusViewProps, render_status_list, render_status_view,
};
pub use status_ticks::{bubble_status_ticks, status_ticks};
