//! Avatars.
//!
//! Nobody has a profile picture in this client yet, so an avatar is an
//! initial on a coloured ground. The colour is derived from the JID rather
//! than the display name: a contact who renames themselves — or who is known
//! by a push name in one chat and a phone number in another — keeps the same
//! colour, which is the whole point of having one.

use gpui::{
    AnyElement, App, Hsla, IntoElement, ParentElement, Pixels, RenderOnce, SharedString, Styled,
    Window, div, linear_color_stop, linear_gradient, px,
};
use gpui_component::ActiveTheme as _;
use gpui_component::{Icon, IconName};

use crate::theme::ActiveProductTheme as _;

/// Where a contact is, as far as the avatar needs to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    Online,
    /// Known to be away. Drawn as a hollow marker rather than a coloured dot,
    /// so availability is never carried by colour alone.
    Away,
}

/// A marker in the avatar's lower corner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Badge {
    Presence(Presence),
    /// This conversation is a group, not a person.
    Group,
}

/// An initial on a ground derived from an identity.
#[derive(IntoElement)]
pub struct Avatar {
    /// The JID, or any stable identity. Drives the colour.
    identity: SharedString,
    initial: char,
    size: Pixels,
    badge: Option<Badge>,
    /// The surface the avatar sits on, which the badge's ring has to match to
    /// read as a cut-out rather than a second circle.
    ground: Option<Hsla>,
}

impl Avatar {
    /// An avatar for `identity`, labelled with the first character of `name`.
    pub fn new(identity: impl Into<SharedString>, name: &str, size: Pixels) -> Self {
        Self {
            identity: identity.into(),
            initial: name
                .chars()
                .find(|c| !c.is_whitespace())
                .unwrap_or('?')
                .to_uppercase()
                .next()
                .unwrap_or('?'),
            size,
            badge: None,
            ground: None,
        }
    }

    /// Mark the avatar with the contact's availability.
    pub fn presence(mut self, presence: Option<Presence>) -> Self {
        if let Some(presence) = presence {
            self.badge = Some(Badge::Presence(presence));
        }
        self
    }

    /// Mark the avatar as a group rather than a person.
    pub fn group(mut self, is_group: bool) -> Self {
        if is_group {
            self.badge = Some(Badge::Group);
        }
        self
    }

    /// The surface behind the avatar, so a badge can ring itself in it.
    ///
    /// Without this the ring uses the panel colour, which is wrong on a
    /// selected row and shows as a faint halo.
    pub fn on(mut self, ground: Hsla) -> Self {
        self.ground = Some(ground);
        self
    }
}

impl RenderOnce for Avatar {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let product = cx.product();
        let hue = product.palette.speaker(&self.identity);
        let ground = self.ground.unwrap_or(cx.theme().sidebar);

        // The ground is the identity's hue pulled almost all the way down to
        // the card surface: enough tint to tell two avatars apart, never
        // enough to compete with the initial drawn on it.
        let top = product.hsla(hue.mix(product.palette.secondary, 0.82));
        let bottom = product.hsla(product.palette.secondary);

        let badge_size = (self.size * 0.28).max(px(10.0));

        div()
            .relative()
            .flex_shrink_0()
            .size(self.size)
            .child(
                div()
                    .size(self.size)
                    .rounded_full()
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(linear_gradient(
                        160.0,
                        linear_color_stop(top, 0.0),
                        linear_color_stop(bottom, 1.0),
                    ))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(self.size * 0.38)
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(product.hsla(hue))
                    .child(self.initial.to_string()),
            )
            .children(
                self.badge
                    .map(|badge| render_badge(badge, badge_size, ground, cx)),
            )
    }
}

fn render_badge(badge: Badge, size: Pixels, ground: Hsla, cx: &App) -> AnyElement {
    let ringed = div()
        .absolute()
        .bottom_0()
        .right_0()
        .size(size)
        .rounded_full()
        // The ring is the surface behind the avatar, so the badge reads as a
        // hole punched in the circle rather than a sticker on top of it.
        .border_2()
        .border_color(ground)
        .flex()
        .items_center()
        .justify_center();

    match badge {
        Badge::Presence(Presence::Online) => ringed.bg(cx.theme().success).into_any_element(),
        // Away is the same shape without the fill: shape carries the state, so
        // it survives both a colour-blind reader and a custom palette.
        Badge::Presence(Presence::Away) => ringed
            .bg(cx.theme().secondary)
            .border_color(ground)
            .child(
                div()
                    .size(size * 0.4)
                    .rounded_full()
                    .bg(cx.product().hsla(cx.product().palette.muted_foreground)),
            )
            .into_any_element(),
        Badge::Group => ringed
            .bg(cx.theme().secondary)
            .child(
                Icon::new(IconName::User)
                    .size(size * 0.6)
                    .text_color(cx.theme().muted_foreground),
            )
            .into_any_element(),
    }
}
