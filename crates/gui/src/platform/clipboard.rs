//! Reading an image from the platform clipboard.

use crate::platform::picker::Picked;

/// Read the clipboard image, if the clipboard contains one.
pub fn read(cx: &gpui::App) -> gpui::Task<Result<Option<Picked>, String>> {
    imp::read(cx)
}

fn image_from_item(item: gpui::ClipboardItem) -> Result<Option<Picked>, String> {
    let image = item.into_entries().find_map(|entry| match entry {
        gpui::ClipboardEntry::Image(image) => Some(image),
        _ => None,
    });
    let Some(image) = image else {
        return Ok(None);
    };
    let mime_type = image.format.mime_type().to_string();
    let file_name = format!("pasted.{}", image.format.extension());
    if let Some(reason) = crate::platform::picker::unsendable(&file_name, image.bytes.len() as u64)
    {
        return Err(reason);
    }
    Ok(Some(Picked {
        file_name,
        mime_type,
        bytes: image.bytes,
    }))
}

#[cfg(not(target_family = "wasm"))]
mod imp {
    use super::{Picked, image_from_item};

    pub(super) fn read(cx: &gpui::App) -> gpui::Task<Result<Option<Picked>, String>> {
        gpui::Task::ready(cx.read_from_clipboard().map_or(Ok(None), image_from_item))
    }
}

#[cfg(target_family = "wasm")]
mod imp {
    use super::{Picked, image_from_item};

    pub(super) fn read(cx: &gpui::App) -> gpui::Task<Result<Option<Picked>, String>> {
        let task = cx.read_from_clipboard_async();
        cx.foreground_executor().spawn(async move {
            task.await
                .and_then(|item| item.map_or(Ok(None), image_from_item))
                .or_else(|_| Ok(None))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::image_from_item;

    #[test]
    fn text_clipboard_is_not_an_image() {
        assert!(
            image_from_item(gpui::ClipboardItem::new_string("text".into()))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn image_clipboard_becomes_a_picked_file() {
        let image = gpui::Image {
            format: gpui::ImageFormat::Png,
            bytes: vec![1, 2, 3],
            id: 0,
        };
        let picked = image_from_item(gpui::ClipboardItem::new_image(&image))
            .unwrap()
            .expect("image");
        assert_eq!(picked.file_name, "pasted.png");
        assert_eq!(picked.mime_type, "image/png");
        assert_eq!(picked.bytes, vec![1, 2, 3]);
    }
}
