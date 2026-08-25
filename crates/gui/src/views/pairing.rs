//! Linking a phone: the QR code, the pair code, and the steps to reach them.

use std::sync::Arc;

use gpui::{App, Image, ImageFormat, ImageSource, IntoElement, ParentElement, Styled, div, img};
use gpui_component::ActiveTheme as _;

use super::centered_view;
use crate::theme::{ActiveProductTheme as _, Metrics};
use oxidezap_core::CachedQrCode;

/// How long a code is valid at issue, so the expiry bar has a full scale to
/// drain against. The server reissues well before this.
const CODE_LIFETIME_SECS: u64 = 60;

/// Generate QR code as PNG bytes (called once when QR data changes)
pub fn generate_qr_png(data: &str) -> Option<Vec<u8>> {
    use image::ImageEncoder;
    use qrcode::QrCode;

    let code = QrCode::new(data.as_bytes()).ok()?;
    let image = code.render::<image::Luma<u8>>().build();

    let mut png_bytes = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut png_bytes);
    encoder
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            image::ExtendedColorType::L8,
        )
        .ok()?;

    Some(png_bytes)
}

pub fn render_pairing_view(
    qr_code: Option<&CachedQrCode>,
    pair_code: Option<String>,
    timeout_secs: u64,
    cx: &App,
) -> impl IntoElement {
    let metrics = cx.product().metrics;

    centered_view(metrics.space_xxl(), cx)
        .child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(metrics.space_md())
                .child(
                    div()
                        .text_size(metrics.text_title())
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(cx.theme().foreground)
                        .child("Link your phone"),
                )
                .child(
                    div()
                        .text_size(metrics.text_secondary())
                        .text_color(cx.theme().muted_foreground)
                        .child("This device stays linked until you unlink it."),
                ),
        )
        .child(render_steps(metrics, cx))
        .child(render_qr(qr_code, metrics, cx))
        .children(pair_code.map(|code| render_pair_code(code, metrics, cx)))
        .child(render_expiry(timeout_secs, metrics, cx))
}

/// What to do on the phone, in order.
///
/// The QR code is meaningless without them: it is the last step, not the
/// instruction.
fn render_steps(metrics: Metrics, cx: &App) -> impl IntoElement + use<> {
    const STEPS: [&str; 3] = [
        "Open WhatsApp on your phone",
        "Go to Settings → Linked devices",
        "Tap Link a device and scan this code",
    ];
    let subtle = cx.product().hsla(cx.product().palette.subtle_foreground);

    div()
        .flex()
        .flex_col()
        .gap(metrics.space_md())
        .children(STEPS.into_iter().enumerate().map(move |(ix, step)| {
            div()
                .flex()
                .items_center()
                .gap(metrics.space_lg())
                .child(
                    div()
                        .size(metrics.space_xxl())
                        .flex_shrink_0()
                        .rounded_full()
                        .bg(cx.theme().secondary)
                        .border_1()
                        .border_color(cx.theme().border)
                        .flex()
                        .items_center()
                        .justify_center()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_size(metrics.text_meta())
                        .text_color(subtle)
                        .child((ix + 1).to_string()),
                )
                .child(
                    div()
                        .text_size(metrics.text_secondary())
                        .text_color(cx.theme().muted_foreground)
                        .child(step),
                )
        }))
}

fn render_qr(
    qr_code: Option<&CachedQrCode>,
    metrics: Metrics,
    _cx: &App,
) -> impl IntoElement + use<> {
    let size = metrics.qr_size();

    div()
        .size(size)
        // A light frame, not a bare raster: the code needs its quiet zone to
        // scan, and on a dark window it has to supply its own.
        .p(metrics.space_lg())
        .bg(gpui::white())
        .rounded(metrics.radius_lg())
        .flex()
        .justify_center()
        .items_center()
        .child(match qr_code {
            Some(cached) => {
                let image = Image::from_bytes(ImageFormat::Png, cached.png_bytes.as_ref().clone());
                img(ImageSource::Image(Arc::new(image)))
                    .size_full()
                    .into_any_element()
            }
            None => div()
                // Sits on the QR code's white raster, not on a themed
                // surface, so it is not a theme colour to resolve.
                .text_color(gpui::black())
                .text_size(metrics.text_small())
                .child("Waiting for a code…")
                .into_any_element(),
        })
}

/// The typed alternative, spaced so it can be read aloud or copied by eye.
fn render_pair_code(code: String, metrics: Metrics, cx: &App) -> impl IntoElement + use<> {
    let subtle = cx.product().hsla(cx.product().palette.subtle_foreground);

    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(metrics.space_md())
        .child(
            div()
                .text_size(metrics.text_small())
                .text_color(subtle)
                .child("Or enter this code on your phone"),
        )
        .child(div().flex().gap(metrics.space_md()).children(
            code.chars().filter(|c| !c.is_whitespace()).map(|ch| {
                div()
                    .px(metrics.space_lg())
                    .py(metrics.space_md())
                    .rounded(metrics.radius_md())
                    .bg(cx.theme().secondary)
                    .border_1()
                    .border_color(cx.theme().border)
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_size(metrics.text_heading())
                    .text_color(cx.theme().foreground)
                    .child(ch.to_string())
            }),
        ))
}

/// How much of the code's life is left, as a bar rather than a sentence.
///
/// "Expires in 48 seconds" is a number that has to be re-read to mean
/// anything; a bar draining is legible without reading it at all.
fn render_expiry(timeout_secs: u64, metrics: Metrics, cx: &App) -> impl IntoElement + use<> {
    let remaining = (timeout_secs as f32 / CODE_LIFETIME_SECS as f32).clamp(0.0, 1.0);
    let subtle = cx.product().hsla(cx.product().palette.subtle_foreground);
    // Running out is worth noticing before it happens.
    let colour = if remaining < 0.25 {
        cx.theme().warning
    } else {
        cx.theme().primary
    };

    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(metrics.space_md())
        .w(metrics.qr_size())
        .child(
            div()
                .w_full()
                .h(metrics.space_xs())
                .rounded_full()
                .bg(cx.theme().secondary)
                .child(
                    div()
                        .h_full()
                        .w(gpui::relative(remaining))
                        .rounded_full()
                        .bg(colour),
                ),
        )
        .child(
            div()
                .font_family(cx.theme().mono_font_family.clone())
                .text_size(metrics.text_micro())
                .text_color(subtle)
                .child(if timeout_secs > 0 {
                    format!("code refreshes in {timeout_secs}s")
                } else {
                    "refreshing…".to_string()
                }),
        )
}
