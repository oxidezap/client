//! Application views

use gpui::{
    AnyElement, App, Hsla, InteractiveElement, IntoElement, ParentElement, Pixels, RenderOnce,
    StatefulInteractiveElement, Styled, Window, div, relative,
};
use gpui_component::ActiveTheme as _;

mod chat;
mod error;
mod loading;
mod logged_out;
pub mod pairing;
mod settings;

pub use chat::{render_call_overlay, render_connected_view};
pub use error::{render_error_view, render_refused_view};
pub use loading::{render_connecting_view, render_loading_view, render_syncing_view};
pub use logged_out::render_logged_out_view;
pub use pairing::render_pairing_view;
pub use settings::render_settings_view;

/// A pane whose content sits in the middle of it, and which never clips it.
///
/// The base layout for the screens on the way to a conversation — loading,
/// error, logged out, pairing — and for any other pane whose whole content is
/// one centred column. Centred *and* scrollable, which
/// is one decision rather than two: a column that is only centred is clipped
/// at both ends the moment it outgrows the window — the way a QR code, three
/// steps and two countdowns outgrow a 640px-tall handheld, which showed the
/// middle of the screen with the title above it and the pair code below it,
/// both off the glass and neither reachable. Content shorter than the window
/// is centred by `min_h_full`; content taller than it makes the column grow
/// past the viewport, where centring becomes a no-op and the scroll takes
/// over. Nothing has to ask which case it is in.
#[derive(IntoElement)]
pub struct CenteredView {
    id: &'static str,
    gap: Pixels,
    padding: Pixels,
    /// The surface it sits on. `None` is the screens' own — the sidebar
    /// colour, which is what a window with no conversation in it shows.
    surface: Option<Hsla>,
    children: Vec<AnyElement>,
}

/// Create a centered pane container with consistent styling.
pub fn centered_view(id: &'static str, gap: Pixels) -> CenteredView {
    CenteredView {
        id,
        gap,
        // The screen's own margin, which is also what keeps the first and last
        // child off the edge once the column is scrolling rather than centred.
        padding: gap,
        surface: None,
        children: Vec::new(),
    }
}

impl CenteredView {
    /// Draw on a surface other than the screens' own.
    pub fn surface(mut self, surface: Hsla) -> Self {
        self.surface = Some(surface);
        self
    }
}

impl ParentElement for CenteredView {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl RenderOnce for CenteredView {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        div()
            .id(self.id)
            .size_full()
            .bg(self.surface.unwrap_or(cx.theme().sidebar))
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .min_h(relative(1.0))
                    .justify_center()
                    .items_center()
                    .gap(self.gap)
                    .p(self.padding)
                    .children(self.children),
            )
    }
}
