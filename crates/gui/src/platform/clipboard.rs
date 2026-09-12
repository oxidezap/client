//! Reading an image from the platform clipboard.

use crate::platform::picker::Picked;

/// Read the clipboard image, if the clipboard contains one.
pub fn read(cx: &gpui::App) -> gpui::Task<Result<Option<Picked>, String>> {
    imp::read(cx)
}

fn image_from_item(item: gpui::ClipboardItem) -> Option<Picked> {
    let image = item.into_entries().find_map(|entry| match entry {
        gpui::ClipboardEntry::Image(image) => Some(image),
        _ => None,
    })?;
    let mime_type = image.format.mime_type().to_string();
    let file_name = format!("pasted.{}", image.format.extension());
    if crate::platform::picker::unsendable(&file_name, image.bytes.len() as u64).is_some() {
        return None;
    }
    Some(Picked {
        file_name,
        mime_type,
        bytes: image.bytes,
    })
}

#[cfg(not(target_family = "wasm"))]
mod imp {
    use super::{Picked, image_from_item};

    pub(super) fn read(cx: &gpui::App) -> gpui::Task<Result<Option<Picked>, String>> {
        gpui::Task::ready(Ok(cx.read_from_clipboard().and_then(image_from_item)))
    }
}

#[cfg(target_family = "wasm")]
mod imp {
    use super::{Picked, image_from_item};

    pub(super) fn read(cx: &gpui::App) -> gpui::Task<Result<Option<Picked>, String>> {
        let task = cx.read_from_clipboard_async();
        cx.foreground_executor().spawn(async move {
            task.await
                .map(|item| item.and_then(image_from_item))
                .map_err(|error| error.to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::image_from_item;

    #[test]
    fn text_clipboard_is_not_an_image() {
        assert!(image_from_item(gpui::ClipboardItem::new_string("text".into())).is_none());
    }

    #[test]
    fn image_clipboard_becomes_a_picked_file() {
        let image = gpui::Image {
            format: gpui::ImageFormat::Png,
            bytes: vec![1, 2, 3],
            id: 0,
        };
        let picked = image_from_item(gpui::ClipboardItem::new_image(&image)).expect("image");
        assert_eq!(picked.file_name, "pasted.png");
        assert_eq!(picked.mime_type, "image/png");
        assert_eq!(picked.bytes, vec![1, 2, 3]);
    }
}
