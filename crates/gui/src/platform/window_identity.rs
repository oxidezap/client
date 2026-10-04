//! Linux desktops match a window to its launcher by this ID. X11 also accepts
//! icon pixels directly; Wayland obtains the icon from the installed launcher.

pub const APP_ID: &str = "org.oxidezap.client.local";

pub fn apply(options: &mut gpui::WindowOptions, cx: &gpui::App) {
    options.app_id = Some(APP_ID.to_owned());
    imp::apply(options, cx);
}

#[cfg(target_os = "linux")]
mod imp {
    pub(super) fn apply(options: &mut gpui::WindowOptions, cx: &gpui::App) {
        match icon(cx) {
            Ok(icon) => options.icon = Some(std::sync::Arc::new(icon)),
            Err(error) => log::warn!("could not load the window icon: {error}"),
        }
    }

    fn icon(cx: &gpui::App) -> anyhow::Result<image::RgbaImage> {
        let rendered = cx.svg_renderer().render_single_frame(
            include_bytes!("../../../../packaging/linux/org.oxidezap.client.local.svg"),
            1.0,
        )?;
        let size = rendered.size(0);
        let mut pixels = rendered
            .as_bytes(0)
            .ok_or_else(|| anyhow::anyhow!("missing icon frame"))?
            .to_vec();
        for pixel in pixels.as_chunks_mut::<4>().0 {
            // GPUI renders premultiplied BGRA; _NET_WM_ICON uses straight RGBA.
            pixel.swap(0, 2);
            let alpha = u16::from(pixel[3]);
            for channel in &mut pixel[..3] {
                *channel = (u16::from(*channel) * 255)
                    .checked_div(alpha)
                    .unwrap_or(0)
                    .min(255) as u8;
            }
        }
        image::RgbaImage::from_raw(size.width.0 as u32, size.height.0 as u32, pixels)
            .ok_or_else(|| anyhow::anyhow!("invalid icon dimensions"))
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    pub(super) fn apply(_: &mut gpui::WindowOptions, _: &gpui::App) {}
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    #[gpui::test]
    fn linux_window_has_the_launcher_id_and_straight_rgba_icon(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut options = gpui::WindowOptions::default();
            super::apply(&mut options, cx);
            assert_eq!(options.app_id.as_deref(), Some(super::APP_ID));
            let icon = options.icon.expect("embedded SVG produces a window icon");
            assert_eq!(icon.dimensions(), (128, 128));
            assert_eq!(icon.get_pixel(10, 64).0, [0x2e, 0xa0, 0x43, 255]);
            assert_eq!(icon.get_pixel(64, 40).0, [255, 255, 255, 255]);
        });
    }
}
