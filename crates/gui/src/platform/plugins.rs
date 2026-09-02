//! Where this front end's plugins live, and how one is added.
//!
//! A plugin belongs to the daemon that runs it, and so does the folder it
//! comes out of: a directory beside `oxidezapd`, or a page's own origin
//! storage. That is the whole of what this file still answers — *where*, for
//! the sentence somebody reads — because adding one is no longer this side's
//! business at all. It is [`ClientRequest::InstallPlugin`], the module travels
//! through the media cache like any other payload too large for a frame, and
//! the daemon decides what a file may be a plugin under.
//!
//! It used to be otherwise, on one target. A page holding the session had the
//! daemon in its own address space, so the window called into it directly —
//! a second control channel beside the protocol, and one that existed nowhere
//! else, which is why the desktop had no way to install anything at all. The
//! rule it broke is the one that makes the two front ends one front end:
//! `gui` never depends on `session`, and the daemon it does depend on in a
//! page is reached the way every other daemon is.
//!
//! [`ClientRequest::InstallPlugin`]: oxidezap_ipc::ClientRequest::InstallPlugin

/// Where the plugins this window can see come from.
///
/// Every one of these can be installed into — the daemon does the writing —
/// so this decides what somebody is *told*, and nothing else. Where a module
/// is kept and what will start it are two different sentences, and the three
/// answers below are the three ways they combine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Home {
    /// A folder beside the daemon. Not this process's directory — it may not
    /// even be this machine's — which is exactly why installing goes through
    /// the daemon rather than through a path.
    Folder,
    /// This page's own storage, and the plugin host is in this page too.
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
    /// What to tell somebody looking at an empty list.
    #[must_use]
    pub const fn nothing_loaded(self) -> &'static str {
        match self {
            // The same sentence for both, because they are the same act now:
            // the file goes to whichever process holds the folder, and that
            // process is the one that starts it.
            Self::Folder | Self::Page => {
                "Add a .wasm file below. It starts as soon as it is added."
            }
            Self::AnotherTab => {
                "Add a .wasm file below. It starts in the tab holding this account."
            }
        }
    }
}

/// Which of the three this window is looking at.
#[must_use]
pub fn home() -> Home {
    imp::home()
}

/// A module somebody picked, ready to be staged.
pub struct Module {
    /// What the file was called where it was picked. The daemon reads the id
    /// off this, and it is a name rather than a path on both platforms.
    pub file_name: String,
    pub bytes: Vec<u8>,
}

/// Ask for a `.wasm` to install.
///
/// The window's own file chooser, which is the same one the composer's
/// paperclip uses: a desktop asks the operating system and a page asks the
/// document, and both hand back bytes because what happens next is a staged
/// upload either way. One path rather than two, where installing used to be
/// a file input written a second time beside the first.
///
/// `Ok(None)` is nobody having chosen anything, which is not a failure and is
/// not worth a line on screen.
///
/// # Errors
///
/// The chooser could not be opened, the file could not be read, or what came
/// back is not one `.wasm`.
pub fn choose(cx: &gpui::App) -> impl Future<Output = Result<Option<Module>, String>> + use<> {
    // The prompt is asked for here, because it belongs to the platform
    // window; everything after it is the same on both targets, and none of it
    // borrows the app. The same shape [`crate::platform::picker::choose`]
    // has, and for the same reason.
    let chosen = crate::platform::picker::choose(cx);
    async move { one_module(chosen.await?) }
}

/// The one module in what came back, or the sentence to show instead.
fn one_module(chosen: crate::platform::picker::Chosen) -> Result<Option<Module>, String> {
    if let Some(refusal) = chosen.refused.first() {
        return Err(refusal.clone());
    }
    let mut files = chosen.files;
    if files.len() > 1 {
        // Refused rather than quietly installing the first: a reload retires
        // every running plugin, so "add these three" is a different act from
        // three adds and is not one this asks for.
        return Err("Choose one .wasm at a time.".to_string());
    }
    let Some(file) = files.pop() else {
        return Ok(None);
    };
    // A `.wasm` by its name, asked here because this is where somebody can be
    // told — the same reason the size is asked before the bytes are read. It
    // is not the *rule*: what a file may be a plugin under is the daemon's,
    // which holds the folder and asks the host, and it is asked again there.
    if !file
        .file_name
        .rsplit_once('.')
        .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("wasm"))
    {
        return Err(format!("{} is not a .wasm module.", file.file_name));
    }
    Ok(Some(Module {
        file_name: file.file_name,
        bytes: file.bytes,
    }))
}

#[cfg(not(target_family = "wasm"))]
mod imp {
    use super::Home;

    /// A desktop front end reaches `oxidezapd`, whose plugins are files.
    pub(super) fn home() -> Home {
        Home::Folder
    }
}

#[cfg(target_family = "wasm")]
mod imp {
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
            // still this origin's — the request reaches the tab holding the
            // account, which is also the one that would run what it installs.
            _ if !crate::session::this_tab_holds_the_account() => Home::AnotherTab,
            // Rejected is not "no daemon": the window is on the settled
            // refusal screen and is drawing no Settings at all. Answered as
            // `Page` rather than as a third case, because a case nothing can
            // reach is a case nobody maintains.
            _ => Home::Page,
        }
    }
}
