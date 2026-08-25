//! The product's own icons.
//!
//! gpui-component ships a general-purpose set; a messaging client needs a
//! call, a paperclip, a double tick. Those live in `assets/icons` and are
//! embedded at build time by [`crate::assets`].
//!
//! They are addressed through this enum rather than by path string so a
//! renamed or deleted file is a compile error at every call site instead of a
//! blank square at runtime — the same guarantee `IconName` gives for the
//! library's own set. `icons_all_resolve` holds the two halves together.

use gpui_component::Icon;

/// An icon this product ships itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductIcon {
    Phone,
    PhoneOff,
    Video,
    VideoOff,
    Mic,
    MicOff,
    Play,
    Pause,
    Stop,
    CheckCheck,
    Clock,
    Paperclip,
    Smile,
    Users,
    UserPlus,
    Reply,
    Image,
    Film,
    FileText,
    Sticker,
    Trash,
    Volume,
    Shield,
    Key,
    Lock,
    Grid,
    MessageSquare,
    WifiOff,
    MinimizeCard,
}

impl ProductIcon {
    /// Every icon, so a test can prove each one actually loads.
    pub const ALL: [Self; 29] = [
        Self::Phone,
        Self::PhoneOff,
        Self::Video,
        Self::VideoOff,
        Self::Mic,
        Self::MicOff,
        Self::Play,
        Self::Pause,
        Self::Stop,
        Self::CheckCheck,
        Self::Clock,
        Self::Paperclip,
        Self::Smile,
        Self::Users,
        Self::UserPlus,
        Self::Reply,
        Self::Image,
        Self::Film,
        Self::FileText,
        Self::Sticker,
        Self::Trash,
        Self::Volume,
        Self::Shield,
        Self::Key,
        Self::Lock,
        Self::Grid,
        Self::MessageSquare,
        Self::WifiOff,
        Self::MinimizeCard,
    ];

    /// Path inside the embedded asset bundle.
    pub fn path(self) -> &'static str {
        match self {
            Self::Phone => "icons/phone.svg",
            Self::PhoneOff => "icons/phone-off.svg",
            Self::Video => "icons/video.svg",
            Self::VideoOff => "icons/video-off.svg",
            Self::Mic => "icons/mic.svg",
            Self::MicOff => "icons/mic-off.svg",
            Self::Play => "icons/play.svg",
            Self::Pause => "icons/pause.svg",
            Self::Stop => "icons/stop.svg",
            Self::CheckCheck => "icons/check-check.svg",
            Self::Clock => "icons/clock.svg",
            Self::Paperclip => "icons/paperclip.svg",
            Self::Smile => "icons/smile.svg",
            Self::Users => "icons/users.svg",
            Self::UserPlus => "icons/user-plus.svg",
            Self::Reply => "icons/reply.svg",
            Self::Image => "icons/image.svg",
            Self::Film => "icons/film.svg",
            Self::FileText => "icons/file-text.svg",
            Self::Sticker => "icons/sticker.svg",
            Self::Trash => "icons/trash.svg",
            Self::Volume => "icons/volume.svg",
            Self::Shield => "icons/shield.svg",
            Self::Key => "icons/key.svg",
            Self::Lock => "icons/lock.svg",
            Self::Grid => "icons/grid.svg",
            Self::MessageSquare => "icons/message-square.svg",
            Self::WifiOff => "icons/wifi-off.svg",
            Self::MinimizeCard => "icons/minimize-card.svg",
        }
    }
}

impl From<ProductIcon> for Icon {
    fn from(icon: ProductIcon) -> Self {
        Icon::default().path(icon.path())
    }
}

#[cfg(test)]
mod tests {
    use super::ProductIcon;
    use crate::assets::CustomIcons;

    /// The enum and the asset folder are two halves of one thing; this is what
    /// keeps a renamed file from becoming a blank square in a toolbar.
    #[test]
    fn every_icon_resolves_to_an_embedded_asset() {
        for icon in ProductIcon::ALL {
            let asset = CustomIcons::get(icon.path())
                .unwrap_or_else(|| panic!("{icon:?} has no asset at {}", icon.path()));
            assert!(
                asset.data.starts_with(b"<svg"),
                "{icon:?} does not look like an SVG"
            );
        }
    }

    /// And the other direction: an icon added to the folder but never wired
    /// into the enum is dead weight in the binary.
    #[test]
    fn every_embedded_asset_is_reachable_from_the_enum() {
        let known: Vec<&str> = ProductIcon::ALL.iter().map(|i| i.path()).collect();
        for path in CustomIcons::iter() {
            assert!(
                known.contains(&path.as_ref()),
                "{path} is embedded but no ProductIcon names it"
            );
        }
    }
}
