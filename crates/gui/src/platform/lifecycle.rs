//! Noticing that the front end is going away.
//!
//! A desktop `main` is the teardown: it waits for [`shutdown::requested`],
//! and everything after that wait disconnects the session and closes SQLite.
//! A page has no `main` that ends — the start function returns and the
//! browser owns the loop — so nothing there ever asked, and the ordered
//! teardown the daemon library carries was unreachable from a tab.
//!
//! [`shutdown::requested`]: oxidezap_daemon::shutdown::requested

/// Ask the platform to say when the front end is closing.
///
/// Called once, before the window opens. On a desktop it does nothing: the
/// process already has an ending, and adding a second route to it is the
/// mistake the shutdown module exists to prevent.
pub fn watch_for_departure() {
    imp::watch_for_departure();
}

#[cfg(not(target_family = "wasm"))]
mod imp {
    /// `main` is the ending here, and it already waits.
    pub(super) fn watch_for_departure() {}
}

#[cfg(target_family = "wasm")]
mod imp {
    use std::cell::RefCell;

    use wasm_bindgen::JsCast as _;
    use wasm_bindgen::prelude::Closure;

    thread_local! {
        /// Held for the life of the page, which is exactly how long it is
        /// listening. Kept rather than forgotten so the listener can be
        /// described by something that owns it, and so a second call does
        /// not leave two.
        static LEAVING: RefCell<Option<Closure<dyn FnMut(web_sys::PageTransitionEvent)>>> =
            const { RefCell::new(None) };
    }

    pub(super) fn watch_for_departure() {
        let Some(window) = web_sys::window() else {
            return;
        };
        let leaving = Closure::<dyn FnMut(web_sys::PageTransitionEvent)>::new(
            move |event: web_sys::PageTransitionEvent| {
                // Not a page going into the back/forward cache. That one is
                // frozen and may be restored into the same session, so asking
                // it to shut down would end an account the person is about to
                // come back to.
                if event.persisted() {
                    return;
                }
                oxidezap_daemon::shutdown::request("the page is going away");
            },
        );
        if window
            .add_event_listener_with_callback("pagehide", leaving.as_ref().unchecked_ref())
            .is_err()
        {
            log::warn!("this page will not say when it is closing");
            return;
        }
        LEAVING.with(|held| *held.borrow_mut() = Some(leaving));
    }
}
