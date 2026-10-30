//! Web avatar decoding and the persistent storage implementation.

pub use super::web::*;

pub(super) async fn decode_avatar(
    bytes: Vec<u8>,
    _priority: super::DecodePriority,
    current: impl FnOnce() -> bool + Send + 'static,
) -> Option<(std::sync::Arc<gpui::RenderImage>, u64)> {
    current()
        .then(|| crate::session::media::decode_avatar(&bytes))
        .flatten()
}
