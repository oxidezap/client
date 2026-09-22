//! Reading media from the platform clipboard.
//!
//! One question — what did somebody copy — and one answer per platform. A
//! copied picture arrives as image bytes on every desktop; files copied in
//! the system's file manager arrive as paths on macOS and Windows, which is
//! the only way a video ever reaches the composer through the clipboard —
//! nobody copies video *bytes*. Linux reports neither file list (both its
//! backends declare `text/uri-list` without reading it), so file
//! copy-paste degrades there and the drag-and-drop path covers it.
//!
//! A page is its own story: pastes arrive through the document's `paste`
//! event (see [`crate::platform::drop`]'s listener), which needs no
//! permission prompt. The asynchronous `navigator.clipboard.read()` API this
//! used to rely on does, and gpui's own paste handler already consumed the
//! event by the time it answers — so this half answers nothing and the event
//! listener owns web pastes outright.

use crate::platform::picker::{Chosen, Picked};

/// Read clipboard media, if the clipboard holds any.
///
/// Images are converted synchronously; copied files are read off the UI
/// thread, the way the file chooser reads them. Text is not media and reads
/// back empty, which the caller treats as "nothing pasted" rather than a
/// failure.
pub fn read(cx: &gpui::App) -> gpui::Task<Result<Chosen, String>> {
    imp::read(cx)
}

/// An image entry as a file that can be sent.
fn image_as_picked(image: gpui::Image) -> Result<Picked, String> {
    let file_name = format!("pasted.{}", image.format.extension());
    if let Some(reason) = crate::platform::picker::unsendable(&file_name, image.bytes.len() as u64)
    {
        return Err(reason);
    }
    Ok(Picked {
        file_name,
        mime_type: image.format.mime_type().to_string(),
        bytes: image.bytes,
    })
}

/// Sort an item's entries into images and copied file paths.
///
/// Text entries are skipped, not refused: pasting text is the composer's own
/// job, and a clipboard holding both words and a picture pastes the words
/// through the field while the picture waits in the confirmation modal.
fn split_entries(item: gpui::ClipboardItem) -> (Vec<Picked>, Vec<std::path::PathBuf>, Vec<String>) {
    let mut images = Vec::new();
    let mut paths = Vec::new();
    let mut refused = Vec::new();
    for entry in item.into_entries() {
        match entry {
            gpui::ClipboardEntry::Image(image) => match image_as_picked(image) {
                Ok(picked) => images.push(picked),
                Err(reason) => refused.push(reason),
            },
            gpui::ClipboardEntry::ExternalPaths(external) => {
                paths.extend(external.paths().iter().cloned());
            }
            gpui::ClipboardEntry::String(_) => {}
        }
    }
    (images, paths, refused)
}

#[cfg(not(target_family = "wasm"))]
mod imp {
    use super::{Chosen, split_entries};

    pub(super) fn read(cx: &gpui::App) -> gpui::Task<Result<Chosen, String>> {
        let Some(item) = cx.read_from_clipboard() else {
            return gpui::Task::ready(Ok(Chosen::default()));
        };
        let (images, paths, refused) = split_entries(item);
        if paths.is_empty() {
            return gpui::Task::ready(Ok(Chosen {
                files: images,
                refused,
            }));
        }
        // Copied files are real file I/O and read like dropped ones: off
        // the thread that draws the window.
        cx.background_executor().spawn(async move {
            let mut chosen = crate::platform::picker::read_paths(&paths);
            chosen.files.splice(0..0, images);
            chosen.refused.splice(0..0, refused);
            Ok(chosen)
        })
    }
}

#[cfg(target_family = "wasm")]
mod imp {
    use super::Chosen;

    pub(super) fn read(_cx: &gpui::App) -> gpui::Task<Result<Chosen, String>> {
        // Answered by the document `paste` listener instead: it reads the
        // event's files directly — every kind, including videos gpui's own
        // paste handler skips — with no permission prompt. The asynchronous
        // clipboard API would ask for one and answer with what the event
        // already gave, so asking is strictly worse.
        gpui::Task::ready(Ok(Chosen::default()))
    }
}

#[cfg(test)]
mod tests {
    use super::split_entries;

    #[test]
    fn text_clipboard_holds_no_media() {
        let (images, paths, refused) =
            split_entries(gpui::ClipboardItem::new_string("text".into()));
        assert!(images.is_empty());
        assert!(paths.is_empty());
        assert!(refused.is_empty());
    }

    #[test]
    fn image_clipboard_becomes_a_picked_file() {
        let image = gpui::Image {
            format: gpui::ImageFormat::Png,
            bytes: vec![1, 2, 3],
            id: 0,
        };
        let (images, paths, refused) = split_entries(gpui::ClipboardItem::new_image(&image));
        assert!(paths.is_empty());
        assert!(refused.is_empty());
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].file_name, "pasted.png");
        assert_eq!(images[0].mime_type, "image/png");
        assert_eq!(images[0].bytes, vec![1, 2, 3]);
    }

    #[test]
    fn copied_file_paths_sort_into_paths() {
        let path = std::path::PathBuf::from("/tmp/clipe.mp4");
        let item = gpui::ClipboardItem {
            entries: vec![gpui::ClipboardEntry::ExternalPaths(gpui::ExternalPaths(
                vec![path.clone()].into_iter().collect(),
            ))],
        };
        let (images, paths, refused) = split_entries(item);
        assert!(images.is_empty());
        assert!(refused.is_empty());
        assert_eq!(paths, vec![path]);
    }
}
