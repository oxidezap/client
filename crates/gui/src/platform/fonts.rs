//! What the window draws text with, where the platform has none of its own.
//!
//! A desktop has a font database — `gpui_platform` builds its text system
//! over font-kit and the system's own families answer for `.SystemUIFont`. A
//! page has nothing of the sort: `CosmicTextSystem::new_without_system_fonts`
//! is what the web backend constructs, and a browser exposes no font files to
//! wasm, so the database starts empty and stays that way until somebody fills
//! it. Filling it is the application's job, and this is where it is done.
//!
//! It used to be gpui's. `gpui_web` bundled IBM Plex Sans and Lilex and added
//! them to the text system as it built the platform; a revision bump took
//! that out with the note "applications must add fonts through
//! `gpui::App::text_system` before opening a window", and this file is that
//! sentence answered. The failure it fixes is not subtle in the log and is
//! total on screen: `resolve_font` *panics* when neither the asked-for family
//! nor one of the fallbacks resolves, so the first frame trapped, and — since
//! a wasm trap unwinds nothing — every `RefCell` gpui held across that frame
//! stayed borrowed for the life of the page. The console filled with
//! "RefCell already borrowed" from a window that never drew a pixel, while
//! the session behind it connected, hydrated and synced perfectly.
//!
//! Which two families, and not some others, is decided upstream rather than
//! by taste: `gpui::font_name_with_fallbacks` maps `.ZedSans` to "IBM Plex
//! Sans" and `.ZedMono` to "Lilex", and the web platform passes "IBM Plex
//! Sans" as the name `.SystemUIFont` resolves to. Those three names are the
//! whole of what a page asks for by default — the theme's own mono family is
//! "DejaVu Sans Mono" here, which no page has either, so it reaches Lilex
//! through gpui's fallback stack — so these are the files that were bundled,
//! at the revision they were bundled from.

/// Give the text system something to resolve, where the platform hands it
/// nothing.
///
/// Called before the window opens, because the first frame is what resolves
/// a font and a frame that cannot is a panic rather than a blank page.
pub fn fonts(cx: &mut gpui::App) {
    imp::fonts(cx);
}

/// The name the web backend resolves `.SystemUIFont` to, and so the family
/// the page has to be able to answer with.
///
/// Written down here rather than read from gpui, which does not export it:
/// `gpui_web` passes it to `CosmicTextSystem::new_without_system_fonts`.
#[cfg(any(target_family = "wasm", test))]
const WEB_SYSTEM_FONT: &str = "IBM Plex Sans";

/// What a page carries, and what it costs: eight faces, about 1.6 MB before
/// compression, the same files and the same revision `gpui_web` used to embed
/// itself.
#[cfg(any(target_family = "wasm", test))]
const BUNDLED: &[(&str, &[u8])] = &[
    (
        "IBMPlexSans-Regular",
        include_bytes!("../../assets/fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf"),
    ),
    (
        "IBMPlexSans-Italic",
        include_bytes!("../../assets/fonts/ibm-plex-sans/IBMPlexSans-Italic.ttf"),
    ),
    (
        "IBMPlexSans-SemiBold",
        include_bytes!("../../assets/fonts/ibm-plex-sans/IBMPlexSans-SemiBold.ttf"),
    ),
    (
        "IBMPlexSans-SemiBoldItalic",
        include_bytes!("../../assets/fonts/ibm-plex-sans/IBMPlexSans-SemiBoldItalic.ttf"),
    ),
    (
        "Lilex-Regular",
        include_bytes!("../../assets/fonts/lilex/Lilex-Regular.ttf"),
    ),
    (
        "Lilex-Bold",
        include_bytes!("../../assets/fonts/lilex/Lilex-Bold.ttf"),
    ),
    (
        "Lilex-Italic",
        include_bytes!("../../assets/fonts/lilex/Lilex-Italic.ttf"),
    ),
    (
        "Lilex-BoldItalic",
        include_bytes!("../../assets/fonts/lilex/Lilex-BoldItalic.ttf"),
    ),
];

#[cfg(not(target_family = "wasm"))]
mod imp {
    /// Nothing to add: the system's own families are what font-kit reads, and
    /// a binary that embedded its own would draw in a font the rest of the
    /// desktop does not use.
    pub(super) fn fonts(_cx: &mut gpui::App) {}
}

#[cfg(target_family = "wasm")]
mod imp {
    use std::borrow::Cow;

    /// A failure here is a window that cannot draw, so it is said out loud —
    /// but it is not a refusal to start: the panic it precedes names the
    /// family and the fallbacks, and a page that reached that panic with this
    /// line above it in the console is one whose report says which of the two
    /// went wrong.
    pub(super) fn fonts(cx: &mut gpui::App) {
        let fonts = super::BUNDLED
            .iter()
            .map(|(_, bytes)| Cow::Borrowed(*bytes))
            .collect();
        if let Err(error) = cx.text_system().add_fonts(fonts) {
            log::error!("the page's own fonts could not be loaded: {error:#}");
        }
    }
}

/// The page's text system, reproduced on the host.
///
/// Not a stand-in: `gpui::TextSystem` over a
/// `CosmicTextSystem::new_without_system_fonts` is exactly what `gpui_web`
/// builds, with the same empty database and the same name behind
/// `.SystemUIFont`. So a font a page cannot resolve is a font this cannot
/// resolve either, which is the only way to hold a browser-only failure with
/// a `cargo test`.
#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;
    use std::sync::Arc;

    fn a_page_without_fonts() -> gpui::TextSystem {
        gpui::TextSystem::new(Arc::new(
            gpui_wgpu::CosmicTextSystem::new_without_system_fonts(WEB_SYSTEM_FONT),
        ))
    }

    fn a_page_with_its_fonts() -> gpui::TextSystem {
        let text_system = a_page_without_fonts();
        text_system
            .add_fonts(
                BUNDLED
                    .iter()
                    .map(|(_, bytes)| Cow::Borrowed(*bytes))
                    .collect(),
            )
            .expect("the bundled faces load");
        text_system
    }

    /// Every family the page asks for by default, resolved without falling
    /// off the end of the stack. `.SystemUIFont` is what the theme sets,
    /// `.ZedSans` and `.ZedMono` are what gpui's own fallback stack names,
    /// and "DejaVu Sans Mono" is gpui-component's default mono family here —
    /// which no page has, so it is the one that proves the fallback chain
    /// still lands somewhere rather than panicking.
    #[test]
    fn the_families_a_page_asks_for_all_resolve() {
        let text_system = a_page_with_its_fonts();
        for family in [".SystemUIFont", ".ZedSans", ".ZedMono", "DejaVu Sans Mono"] {
            // `resolve_font` panics when nothing in the stack answers, which
            // is the production failure itself rather than a proxy for it.
            text_system.resolve_font(&gpui::font(family));
        }
    }

    /// The other half, and the reason the module exists: with nothing added,
    /// the very first family the window asks for takes the frame down. A
    /// browser has no fonts to fall back to and gpui no longer brings any.
    #[test]
    #[should_panic(expected = "failed to resolve font")]
    fn a_page_that_adds_no_fonts_cannot_draw_at_all() {
        a_page_without_fonts().resolve_font(&gpui::font(".SystemUIFont"));
    }

    /// Each file is a font, and each is the face its name claims: an empty or
    /// truncated asset loads as a database with nothing in it, which the two
    /// tests above would not notice as long as one of the eight was intact.
    #[test]
    fn every_bundled_face_is_a_font_on_its_own() {
        for (name, bytes) in BUNDLED {
            assert!(bytes.len() > 1024, "{name} is too small to be a font");
            let text_system = a_page_without_fonts();
            text_system
                .add_fonts(vec![Cow::Borrowed(*bytes)])
                .unwrap_or_else(|e| panic!("{name} did not load: {e}"));
            let family = if name.starts_with("Lilex") {
                "Lilex"
            } else {
                "IBM Plex Sans"
            };
            text_system.resolve_font(&gpui::font(family));
        }
    }
}
