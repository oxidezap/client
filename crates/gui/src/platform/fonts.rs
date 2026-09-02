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
//!
//! Emoji are the other half, and they are *not* embedded. A page draws every
//! glyph itself — `swash` rasterizes out of the database this file fills, on
//! a canvas the browser's own text pipeline never touches — so the system's
//! emoji font is unreachable no matter what the page is served with: CSS
//! `local()` feeds the DOM, and the window is not the DOM. Without a face
//! that covers them, every emoji in a chat title, a reaction or a message
//! draws as a tofu box, which is what this looked like from the outside.
//!
//! Which face is decided by the rasterizer rather than by taste, and it
//! rules out the obvious one. gpui only takes swash's *colour* path for a
//! font whose PostScript name is exactly `NotoColorEmoji`
//! (`check_is_known_emoji_font`), and swash reads COLR **version 0** plus
//! CPAL, or CBDT bitmap strikes — while the Noto Color Emoji that Google
//! Fonts and the npm mirrors serve today is COLRv1: 25 MB of TTF that this
//! renderer would draw as nothing at all. The colour font that does work is
//! the CBDT build, at about ten megabytes. So the page carries Noto Emoji,
//! the monochrome outline face: 1,875 glyphs in 878 KB, drawn through the
//! ordinary outline path in the colour of the text around it.
//!
//! And it is *fetched*, not `include_bytes!`d. The module is already twenty
//! megabytes and the browser caches a second file beside it perfectly well,
//! so the bundle does not grow and a build with no emoji font next to it
//! still runs. The file is copied into `dist` by `web/index.html`; nothing in
//! the document trunk emits names it, so `cargo xtask bundle check` carries
//! it in `REQUIRED` and a bundle without it fails there rather than in a
//! browser.

/// Give the text system something to resolve, where the platform hands it
/// nothing.
///
/// Called before the window opens, because the first frame is what resolves
/// a font and a frame that cannot is a panic rather than a blank page.
pub fn fonts(cx: &mut gpui::App) {
    imp::fonts(cx);
}

/// Fetch what could not be embedded, then start.
///
/// The window opens *behind* the download rather than beside it, and that
/// ordering is the whole reason this is a seam rather than a task spawned
/// from the first frame. gpui caches a shaped line for as long as something
/// keeps asking for it — `LineLayoutCache` holds the current frame and the
/// previous one, and a row on screen is requested every frame — so a face
/// added after the first frame reaches only the text that was scrolled away
/// and back. The chat list a page opens onto would have kept its tofu until
/// it was scrolled twice.
///
/// A deadline is what keeps that from being a page that never draws: the
/// window opens either way, and a fetch that fails or hangs costs the emoji
/// rather than the client. On a desktop there is nothing to wait for and
/// `start` is called where it stands.
pub fn with_downloaded_fonts(start: impl FnOnce() + 'static) {
    imp::with_downloaded_fonts(start);
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

/// The face the page fetches, once it has arrived.
///
/// A `OnceLock` because `fonts` is handed the `App` and the download is not:
/// the fetch finishes before `gpui::Application::run` is called at all, so
/// there is no context to hand it to and nothing to hand it to one with.
#[cfg(target_family = "wasm")]
static DOWNLOADED: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();

/// What the emoji face is called in `dist`, and what the page asks for.
///
/// Relative, like every other asset in the bundle: the deployment lives in a
/// directory under a domain that is not ours, so a name from the origin root
/// would be fetched from that domain's root. `web/index.html` is what copies
/// the file in under this name.
#[cfg(any(target_family = "wasm", test))]
const EMOJI: &str = "NotoEmoji-Regular.ttf";

#[cfg(not(target_family = "wasm"))]
mod imp {
    /// Nothing to add: the system's own families are what font-kit reads, and
    /// a binary that embedded its own would draw in a font the rest of the
    /// desktop does not use. Emoji included — a desktop that draws them is
    /// one with an emoji font installed, and gpui takes swash's colour path
    /// for exactly one PostScript name, `NotoColorEmoji`.
    pub(super) fn fonts(_cx: &mut gpui::App) {}

    /// Nothing to wait for either.
    pub(super) fn with_downloaded_fonts(start: impl FnOnce() + 'static) {
        start();
    }
}

#[cfg(target_family = "wasm")]
mod imp {
    use std::borrow::Cow;
    use std::time::Duration;
    use wasm_bindgen::JsCast as _;
    use wasm_bindgen_futures::JsFuture;

    /// How long the window waits for the emoji face before opening without
    /// it.
    ///
    /// Generous, because it is spent on a same-origin file the browser has
    /// usually already cached and it is reached only after twenty megabytes
    /// of module have arrived over the same connection. Short enough that a
    /// host answering nothing costs a visitor a pause rather than the page.
    const DEADLINE: Duration = Duration::from_secs(10);

    /// A failure here is a window that cannot draw, so it is said out loud —
    /// but it is not a refusal to start: the panic it precedes names the
    /// family and the fallbacks, and a page that reached that panic with this
    /// line above it in the console is one whose report says which of the two
    /// went wrong.
    ///
    /// The emoji face is the one addition that is allowed to be missing. It
    /// is fetched rather than embedded, so a bundle served without it draws
    /// tofu where the emoji are — which is what every build did before it
    /// existed, and is not worth failing a window over.
    pub(super) fn fonts(cx: &mut gpui::App) {
        let fonts: Vec<Cow<'static, [u8]>> = super::BUNDLED
            .iter()
            .map(|(_, bytes)| Cow::Borrowed(*bytes))
            .chain(
                super::DOWNLOADED
                    .get()
                    .map(|bytes| Cow::Borrowed(bytes.as_slice())),
            )
            .collect();
        if let Err(error) = cx.text_system().add_fonts(fonts) {
            log::error!("the page's own fonts could not be loaded: {error:#}");
        }
    }

    /// Fetch the emoji face, then start the window whether it arrived or not.
    pub(super) fn with_downloaded_fonts(start: impl FnOnce() + 'static) {
        wasm_bindgen_futures::spawn_local(async move {
            // `oxidezap_platform` rather than a timer of this module's own:
            // a wait is armed in one place, which /clippy.toml enforces.
            match oxidezap_platform::with_timeout(emoji(), DEADLINE).await {
                Some(Ok(face)) => {
                    // The only writer, and it runs before the window opens,
                    // so a refusal is impossible rather than tolerated.
                    let _ = super::DOWNLOADED.set(face);
                }
                Some(Err(why)) => {
                    log::warn!(
                        "emoji will draw as boxes: {} was not loaded: {why}",
                        super::EMOJI
                    )
                }
                None => log::warn!(
                    "emoji will draw as boxes: {} did not arrive within {DEADLINE:?}",
                    super::EMOJI
                ),
            }
            start();
        });
    }

    /// The face itself, off the origin that served the page.
    async fn emoji() -> Result<Vec<u8>, String> {
        let window =
            web_sys::window().ok_or_else(|| "there is no window to fetch from".to_string())?;
        let response = JsFuture::from(window.fetch_with_str(super::EMOJI))
            .await
            .map_err(js)?;
        let response: web_sys::Response = response
            .dyn_into()
            .map_err(|_| "the fetch did not answer with a response".to_string())?;
        if !response.ok() {
            return Err(format!("the server answered {}", response.status()));
        }
        let buffer = JsFuture::from(response.array_buffer().map_err(js)?)
            .await
            .map_err(js)?;
        // JS to wasm, which is the direction that is allowed to be a copy and
        // the only one that is allowed at all: the module is built with
        // `--shared-memory`, so a view *into* wasm memory is what the browser
        // refuses. `Uint8Array::new` wraps the JS-owned buffer and `to_vec`
        // copies out of it.
        Ok(js_sys::Uint8Array::new(&buffer).to_vec())
    }

    /// A thrown `JsValue`, in a form a log line can carry.
    fn js(error: wasm_bindgen::JsValue) -> String {
        error.as_string().unwrap_or_else(|| format!("{error:?}"))
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

    /// A `WindowTextSystem` rather than the `TextSystem` under it, because
    /// shaping is a window's half of the pair — `layout_line` is where the
    /// line layout cache lives — and it derefs to the other, so every test
    /// that only resolves a family reads the same.
    fn a_page_with_its_fonts() -> gpui::WindowTextSystem {
        let text_system = a_page_without_fonts();
        text_system
            .add_fonts(
                BUNDLED
                    .iter()
                    .map(|(_, bytes)| Cow::Borrowed(*bytes))
                    .collect(),
            )
            .expect("the bundled faces load");
        gpui::WindowTextSystem::new(Arc::new(text_system))
    }

    /// The face a page fetches, read from the file the bundle copies.
    ///
    /// The one `include_bytes!` of it in the tree, and deliberately under
    /// `cfg(test)`: what ships is the file itself, next to `index.html`, and
    /// embedding it here would put 878 KB in the module the page downloads
    /// for the sake of an assertion. Reading it from the same path the copy
    /// is made from is what keeps the tested face and the served one the
    /// same file.
    const EMOJI_FACE: &[u8] = include_bytes!("../../assets/fonts/noto-emoji/NotoEmoji-Regular.ttf");

    fn a_page_that_also_fetched_its_emoji() -> gpui::WindowTextSystem {
        let text_system = a_page_with_its_fonts();
        text_system
            .add_fonts(vec![Cow::Borrowed(EMOJI_FACE)])
            .expect("the emoji face loads");
        text_system
    }

    /// What a line of chat shapes to: the glyph ids, in order.
    ///
    /// Glyph 0 is `.notdef` — the tofu box — so this is the whole assertion
    /// both emoji tests make. Shaped through `.SystemUIFont`, which is the
    /// family the theme sets and so the one every one of those chat titles
    /// went through.
    fn glyphs(text_system: &gpui::WindowTextSystem, text: &str) -> Vec<u32> {
        let runs = [gpui::TextRun {
            len: text.len(),
            font: gpui::font(".SystemUIFont"),
            color: gpui::Hsla::default(),
            background_color: None,
            underline: None,
            strikethrough: None,
        }];
        text_system
            .layout_line(text, gpui::px(16.0), &runs, None)
            .runs
            .iter()
            .flat_map(|run| run.glyphs.iter().map(|glyph| glyph.id.0))
            .collect()
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

    /// The failure in the screenshot, held as a test: a page carrying only
    /// the two text families shapes an emoji to `.notdef`, which is the box
    /// that was drawn where a chat's name had one.
    #[test]
    fn without_the_emoji_face_an_emoji_is_a_tofu_box() {
        let drawn = glyphs(&a_page_with_its_fonts(), "\u{1f9ea}");
        assert!(
            drawn.iter().all(|&id| id == 0),
            "IBM Plex Sans and Lilex are not supposed to cover emoji, but shaped {drawn:?}"
        );
    }

    /// And the fix. Nothing configures a fallback for it: cosmic-text's own
    /// iterator ends by walking every face in the database — the script and
    /// common lists are empty on a target that is neither unix, macOS nor
    /// Windows — so a face that is *loaded* is a face that answers.
    #[test]
    fn the_fetched_face_is_what_draws_an_emoji() {
        let text_system = a_page_that_also_fetched_its_emoji();
        // A test tube, a flag (two regional indicators), a document and a
        // tick: the four shapes the reported window had a box for.
        for emoji in ["\u{1f9ea}", "\u{1f1e7}\u{1f1f7}", "\u{1f4c4}", "\u{2705}"] {
            let drawn = glyphs(&text_system, emoji);
            assert!(
                !drawn.is_empty() && drawn.iter().all(|&id| id != 0),
                "{emoji:?} still shapes to {drawn:?}"
            );
        }
    }

    /// The text around them is still Plex, which is the risk of a last-resort
    /// fallback that covers punctuation and digits of its own: a face added
    /// for emoji must not start answering for the Latin the page is written
    /// in.
    #[test]
    fn the_fetched_face_does_not_take_over_ordinary_text() {
        let plain = "Notes/Save 12:34";
        assert_eq!(
            glyphs(&a_page_with_its_fonts(), plain),
            glyphs(&a_page_that_also_fetched_its_emoji(), plain),
            "the emoji face changed how ordinary text shapes"
        );
    }

    /// The name the page fetches is the name the bundle copies in. Nothing
    /// else connects the two: `web/index.html` names the file, trunk copies
    /// it flat, and this string is what asks for it back.
    #[test]
    fn the_page_asks_for_the_file_the_bundle_carries() {
        let index = include_str!("../../../../web/index.html");
        assert!(
            index.contains(EMOJI),
            "web/index.html no longer copies {EMOJI} into the bundle"
        );
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
