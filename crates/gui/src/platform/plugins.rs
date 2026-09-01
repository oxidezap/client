//! Where this front end's plugins live, and whether it can add one.
//!
//! A plugin belongs to the daemon that runs it, so this is a question about
//! which daemon the window is talking to rather than about the window. A
//! desktop front end reaches `oxidezapd`, whose plugins are files in a folder
//! only the person at the machine can put them in; a page attached to one is
//! in exactly the same position, and gets that daemon's plugins whole. It is
//! only a page holding the session *itself* that has a folder of its own —
//! its origin's private filesystem — and is therefore the one front end that
//! can install anything.
//!
//! The daemon half of this is `daemon::plugins::start`, and the two have to
//! agree: this decides what is drawn, and that decides what is loaded.

/// Where the plugins this window can see come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Home {
    /// A folder beside the daemon. Nothing here can write to it — it is
    /// another process's directory, or another machine's — so the advice is
    /// to put a file in it and restart.
    Folder,
    /// This page's own storage, which it can put a module into itself.
    ///
    /// Only ever answered on the web, so a desktop build constructs it
    /// nowhere. Named with the reason rather than left to a crate-wide
    /// allowance: the enum is what makes the two front ends' answers one
    /// answer, and a variant that existed only on one target would put a
    /// `cfg` in every caller that matches on it.
    #[cfg_attr(not(target_family = "wasm"), allow(dead_code))]
    Page,
    /// This origin's storage, which this tab can write to — and a plugin host
    /// that is running in a *different* tab.
    ///
    /// The folder is one per origin and the host is one per account, so a
    /// window with no session of its own can install perfectly well and
    /// cannot start what it installed. Saying `Page` here was not a small
    /// inaccuracy: it told the person to reload *this* page, which reattaches
    /// to the same holder and loads nothing, so the plugin they had just
    /// added simply never appeared.
    #[cfg_attr(not(target_family = "wasm"), allow(dead_code))]
    AnotherTab,
}

impl Home {
    /// Whether this front end can install and remove plugins.
    #[must_use]
    pub const fn can_install(self) -> bool {
        matches!(self, Self::Page | Self::AnotherTab)
    }

    /// What to tell somebody looking at an empty list.
    #[must_use]
    pub const fn nothing_loaded(self) -> &'static str {
        match self {
            Self::Folder => "Drop a .wasm file in the plugins folder, then press Reload plugins",
            Self::Page => "Add a .wasm file below. It starts as soon as it is added.",
            Self::AnotherTab => {
                "Add a .wasm file below. It starts in the tab holding this account."
            }
        }
    }
}

/// Which of the two this window is looking at.
#[must_use]
pub fn home() -> Home {
    imp::home()
}

/// Choose a `.wasm` and install it, answering the id it claimed.
///
/// `Ok(None)` is nobody having chosen anything, which is not a failure and is
/// not worth a line on screen.
///
/// # Errors
///
/// The file could not be read, is not a name a plugin can have, or the
/// browser refused to keep it.
pub async fn install() -> Result<Option<String>, String> {
    imp::install().await
}

/// Every plugin id in this front end's own folder, loaded or not.
///
/// # Errors
///
/// There is no folder, or it could not be read.
pub async fn installed() -> Result<Vec<String>, String> {
    imp::installed().await
}

/// Take one out of this front end's own folder.
///
/// # Errors
///
/// There is no folder to take it out of, or the browser refused.
pub async fn uninstall(id: &str) -> Result<(), String> {
    imp::uninstall(id).await
}

#[cfg(not(target_family = "wasm"))]
mod imp {
    use super::Home;

    /// A desktop front end reaches `oxidezapd`, whose plugins are files.
    pub(super) fn home() -> Home {
        Home::Folder
    }

    /// Not this front end's to do: the folder belongs to the daemon, which
    /// may not even be on this machine. Present so the interface is one
    /// interface — the call sites ask [`Home::can_install`] first.
    pub(super) async fn install() -> Result<Option<String>, String> {
        Err("this front end cannot install plugins".to_owned())
    }

    /// See [`install`].
    pub(super) async fn uninstall(_id: &str) -> Result<(), String> {
        Err("this front end cannot remove plugins".to_owned())
    }

    /// See [`install`]. The daemon's folder is not this front end's to list —
    /// what it *runs* out of it arrives in the snapshot like everything else.
    pub(super) async fn installed() -> Result<Vec<String>, String> {
        Err("this front end has no plugin folder of its own".to_owned())
    }
}

#[cfg(target_family = "wasm")]
mod imp {
    use js_sys::Uint8Array;
    use wasm_bindgen::JsCast as _;
    use wasm_bindgen::prelude::Closure;
    use wasm_bindgen_futures::JsFuture;

    use super::Home;

    /// A page attached to a real daemon has that daemon's plugins: the web
    /// bridge hands `serve_client` the same host the socket does, so the
    /// interface, the approvals and the actions all travel the protocol they
    /// already travel — and the folder they came out of is that daemon's.
    /// A page holding the session itself has its own, and is asked the same
    /// way the session asks it, so the two cannot answer differently.
    pub(super) fn home() -> Home {
        match oxidezap_ipc::web::named_daemon() {
            oxidezap_ipc::web::NamedDaemon::Named(_) => Home::Folder,
            // No daemon named, and no session here either: this tab is a
            // front end onto another tab of the same origin. The folder is
            // still this origin's — installing writes it, and the write is
            // serialised by a lock the folder already takes — but the host
            // that would load it belongs to the tab holding the account.
            _ if !crate::session::this_tab_holds_the_account() => Home::AnotherTab,
            // Rejected is not "no daemon": the window is on the settled
            // refusal screen and is drawing no Settings at all. Answered as
            // `Page` rather than as a third case, because a case nothing can
            // reach is a case nobody maintains.
            _ => Home::Page,
        }
    }

    /// Ask the browser for a file, and put it in this origin's plugin folder.
    ///
    /// A file input rather than `showOpenFilePicker`, which is Chromium-only
    /// and needs a secure context the published page has but a developer's
    /// `trunk serve` may not. The element joins the document for the length
    /// of the gesture and is taken out again — a detached input's `click()`
    /// is ignored outright by some engines, and one that stays is a control
    /// the page grew and never lost.
    pub(super) async fn install() -> Result<Option<String>, String> {
        let Some(chosen) = choose().await else {
            return Ok(None);
        };
        let name = chosen.name();
        // Before it is read, not after. A `File` knows how long it is without
        // anybody asking for its bytes, and reading one that cannot be kept
        // costs the tab twice its size — the `ArrayBuffer` and the copy out
        // of it — to arrive at a refusal the size alone already decided. The
        // folder's own budget is still checked below, against what is
        // already in it; this is the half that can be answered for nothing.
        let size = chosen.size() as usize;
        let ceiling = oxidezap_daemon::plugins::web::MAX_TOTAL_BYTES;
        if size > ceiling {
            return Err(format!(
                "{name} is {size} bytes, past the {ceiling} a page holds in plugins"
            ));
        }
        let buffer = JsFuture::from(chosen.array_buffer())
            .await
            .map_err(|e| format!("that file could not be read ({e:?})"))?;
        let bytes = Uint8Array::new(&buffer).to_vec();
        oxidezap_daemon::plugins::web::install(&name, bytes)
            .await
            .map(Some)
    }

    pub(super) async fn uninstall(id: &str) -> Result<(), String> {
        oxidezap_daemon::plugins::web::uninstall(id).await
    }

    pub(super) async fn installed() -> Result<Vec<String>, String> {
        oxidezap_daemon::plugins::web::names().await
    }

    /// The file somebody picked, or `None` if they picked nothing.
    ///
    /// Two events, because a browser has two ways of ending this: `change`
    /// when a file was chosen, and `cancel` when the dialog was dismissed.
    /// Waiting only for the first leaves the task — and the closures it holds
    /// — alive for the life of the page every time somebody changes their
    /// mind.
    async fn choose() -> Option<web_sys::File> {
        let document = web_sys::window()?.document()?;
        let input: web_sys::HtmlInputElement =
            document.create_element("input").ok()?.dyn_into().ok()?;
        input.set_type("file");
        // A hint rather than a rule: every browser lets somebody switch the
        // filter off, and the name is checked again before anything is kept.
        input.set_accept(".wasm,application/wasm");
        let style = input.style();
        let _ = style.set_property("display", "none");
        let body = document.body()?;
        body.append_child(&input).ok()?;

        let (tx, rx) = futures_channel::oneshot::channel::<()>();
        let mut tx = Some(tx);
        let done = Closure::<dyn FnMut()>::new(move || {
            if let Some(tx) = tx.take() {
                let _ = tx.send(());
            }
        });
        let handler = done.as_ref().unchecked_ref();
        let _ = input.add_event_listener_with_callback("change", handler);
        let _ = input.add_event_listener_with_callback("cancel", handler);
        input.click();
        let _ = rx.await;
        let _ = body.remove_child(&input);

        input.files().and_then(|files| files.get(0))
    }
}
