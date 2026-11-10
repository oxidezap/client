//! The browser document's language, for accessibility tools around the canvas.

pub fn apply(code: &str) {
    imp::apply(code);
}

#[cfg(not(target_family = "wasm"))]
mod imp {
    pub(super) fn apply(_: &str) {}
}

#[cfg(target_family = "wasm")]
mod imp {
    pub(super) fn apply(code: &str) {
        if let Some(root) = web_sys::window()
            .and_then(|window| window.document())
            .and_then(|document| document.document_element())
        {
            let _ = root.set_attribute("lang", code);
        }
    }
}
