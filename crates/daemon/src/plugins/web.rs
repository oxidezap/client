//! A page's plugin folder, and the way a plugin acts from inside one.
//!
//! The desktop's answer to "where does a plugin come from" is a directory
//! only this user can write, and the answer to "what may it do" is a file
//! beside it. A page has neither, and the two halves are replaced separately
//! because they are different questions:
//!
//! * **The module** is megabytes and is read once at start, so it lives in
//!   OPFS — the browser's own origin-private filesystem, which is a real
//!   directory with real files and no path anyone outside this origin can
//!   name. The folder *is* the registry, exactly as it is on a desktop: the
//!   file's name is the plugin's id, and installing one is putting a file in
//!   it.
//! * **The approval and a plugin's settings** are small and are read and
//!   written from inside a synchronous wasm call, so they live in
//!   `localStorage` — see [`Origin`].
//!
//! Above those sits this half of the daemon's platform split — `start`,
//! `reload`, `detach`, `approve` and [`Bridge::ask`], the five things a page
//! answers differently from a desktop. What they have in common is that a
//! page has no thread to move work to: `spawn_blocking` there is not a slow
//! answer but a panic, so everything runs on the one loop the browser lends.
//!
//! What replaces `only_this_user_can_write` is the origin itself. There is no
//! second local account here and no mode to read: an origin's private
//! filesystem is reachable by that origin and nothing else, which is a
//! stronger sentence than the one a `0700` directory makes and is enforced by
//! the browser rather than by this code. What it does *not* answer is the
//! same thing a folder does not answer on a desktop — that the module is the
//! one the user meant — which is what the approval prompt is for.

use std::sync::Arc;

use js_sys::Uint8Array;
use wasm_bindgen::{JsCast as _, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    FileSystemDirectoryHandle, FileSystemGetDirectoryOptions, FileSystemGetFileOptions,
    FileSystemWritableFileStream,
};

use oxidezap_plugin_host::{Backing, MAX_PLUGINS, Module, Origin, Outcome, Plugins, Reloaded};

use super::{Bridge, publishing_to};
use crate::session_bridge::{Action, Commands as SessionCommands, SessionCommand};
use crate::state::StateHub;

/// See [`super::start`].
///
/// A page's plugins come out of its own origin: the modules from OPFS, the
/// approvals and each plugin's settings from `localStorage`. What a page
/// gives a plugin instead of a thread is a task on its own loop; see
/// `oxidezap_plugin_host::sched`. There is nothing to move off a worker here
/// and nowhere to move it to, so the whole of it is awaited inline.
pub(super) async fn start(hub: &Arc<StateHub>, commands: SessionCommands) -> Arc<Plugins> {
    let sink = publishing_to(hub);
    let modules = installed().await;
    Arc::new(
        Plugins::start(
            modules,
            Arc::new(Origin::storage()),
            Arc::new(Bridge { commands }),
            sink,
        )
        .await,
    )
}

/// See [`super::reload`].
pub(super) async fn reload(plugins: &Arc<Plugins>) -> Reloaded {
    // Handed over as a future rather than as values, and that is not style:
    // `Origin::storage()` *stamps* the origin's storage, retiring every
    // handle taken before it. `Plugins::reload` refuses a second reload while
    // one is running, so gathering these eagerly would let a refused call
    // retire the handle the surviving generation is about to be installed
    // with — every approval and settings write refused afterwards, and a
    // revoked grant left on disk to come back. A future does nothing until it
    // is polled, which is after the reservation.
    let host = Arc::clone(plugins);
    let plugins = &host;
    plugins
        .reload(|| async {
            // `discover` and not `installed`: the fallible one. A folder that
            // cannot be read is not an empty folder, and treating it as one
            // here would retire every healthy plugin and publish an empty set
            // over a transient storage error.
            let modules = discover().await.ok()?;
            // And the host is still this account's. `ForgetSession` can land
            // while that await is suspended, and a page rebuilds its whole
            // service in the same agent — so by the time this resumes, a
            // *replacement* host may already hold the newest storage handle.
            // Taking one here would retire it, and every approval and
            // settings write the new host makes would be refused until some
            // later reload happened to succeed. `reload` rechecks too, but
            // only after the stamp has moved, which is exactly too late.
            if plugins.is_retired() {
                return None;
            }
            // And the fresh handle only once there is something to install
            // with it: taking one retires every older handle, so a scan that
            // failed would leave the running generation writing through a
            // store it no longer owns.
            let state: Arc<dyn Backing> = Arc::new(Origin::storage());
            Some((modules, state))
        })
        .await
}

/// Where [`super::reload_in_background`] puts its work: the page's own event
/// loop, which is the only executor there is. Nothing here is `Send`, and
/// nothing needs to be — the task never leaves the agent that owns it.
pub(super) fn detach(work: impl std::future::Future<Output = ()> + 'static) {
    wasm_bindgen_futures::spawn_local(work);
}

/// See [`super::approve`].
///
/// Inline, and there is nowhere else it could go. `spawn_blocking` needs a
/// blocking pool, and a page's runtime has none — nor could it, since a
/// browser agent is one thread. What this costs is the write itself, which is
/// a `localStorage` set: the same call a plugin's own settings already make
/// from inside a wasm call. `async` with nothing to await, because the
/// desktop half awaits a blocking thread and one signature serves both.
pub(super) async fn approve(plugins: &Arc<Plugins>, plugin: String, approved: bool) -> bool {
    plugins.approve(&plugin, approved)
}

impl Bridge {
    /// Hand one action to the session, without waiting for what it made of
    /// it.
    ///
    /// The one place a page's plugin is weaker than a desktop's, and it is
    /// not a shortcut: the plugin's call is synchronous wasm on the *same*
    /// agent the bridge runs on, so waiting for the answer would be waiting
    /// for a task that cannot run until this call returns — a deadlock, not a
    /// delay. So a page's plugin gets the same "it was taken" a socket front
    /// end already lives with.
    ///
    /// What is still honest here is the refusal: a full command channel is a
    /// session that will not take this now, and a closed one is no session at
    /// all. Both are the answers a plugin acts on; only `Refused` for a
    /// command the daemon would have declined is lost, and that arrives in
    /// the event stream as it does for every other front end.
    pub(super) fn ask(&self, action: Action) -> Outcome {
        use tokio::sync::mpsc::error::TrySendError;

        // Dropped, not awaited. The command is answered on a channel nobody
        // is listening to, which the bridge already tolerates: every other
        // sender there is a connection that has gone.
        let (reply, _answer) = tokio::sync::oneshot::channel();
        match self.commands.try_send(SessionCommand { action, reply }) {
            Ok(()) => Outcome::Accepted,
            Err(TrySendError::Full(_)) => Outcome::Refused,
            Err(TrySendError::Closed(_)) => Outcome::NoSession,
        }
    }
}

/// What the folder is called inside this origin's filesystem.
///
/// A directory rather than a naming convention on the root, because the
/// SQLite VFS keeps its own pool in there and a page that listed the root
/// would find it.
const DIR: &str = "plugins";

/// The most this page will read out of that folder, across every module in
/// it.
///
/// The host bounds one module; this bounds the folder, and it exists because
/// a page reads them all before it starts any of them. A desktop does not
/// need it — there the bytes are opened one at a time and dropped after
/// instantiation — but nothing here can open a file lazily, since every read
/// in a browser is a promise and the host's loader is not async.
pub const MAX_TOTAL_BYTES: usize = 32 * 1024 * 1024;

/// The name one installation is serialized under.
///
/// A Web Lock and not a lock in this agent, because the folder is the
/// *origin's* and so is everyone who can write it: two tabs of the same
/// origin share this directory, and a per-agent mutex would leave each of
/// them weighing a folder the other is about to grow. The same API the
/// account's own claim is taken with, and the reason it is the right one
/// twice — a browser lock is the only kind that spans tabs.
///
/// Without `if_available`, deliberately: the second installation should
/// *wait* rather than be told to try again. Somebody who pressed Add twice
/// wants both files.
const INSTALL_LOCK: &str = "oxidezap-plugins-install";

/// Everything installed, ready to hand to the host.
///
/// Sorted by name, because the order plugins load in is the order their
/// buttons are drawn in, and a set that reshuffled between two visits would
/// move a control under somebody's hand.
///
/// Failure is an empty list and a line in the log: a page whose storage
/// refuses it still has an account to open, and a plugin folder is not worth
/// the session.
pub async fn installed() -> Vec<Module> {
    // A page that cannot read its own storage starts with no plugins, which
    // is the right answer at *start*: there is nothing running to lose, and a
    // daemon that would not come up because a folder was unreadable is a
    // daemon that would not come up. A reload asks the fallible one below,
    // because there the same fact means something else entirely.
    discover().await.unwrap_or_default()
}

/// The same, saying whether the folder could be read.
///
/// The distinction a reload lives on. `installed` turns an unreadable folder
/// into an empty one, and on a reload that would retire every healthy plugin
/// and publish an empty set — with nothing having been removed, nothing to
/// put it back, and a transient storage error indistinguishable from somebody
/// deleting everything.
///
/// # Errors
///
/// The origin's filesystem could not be opened, or the folder could not be
/// listed. A folder that is simply *not there* is `Ok` and empty: nobody has
/// installed anything, which is the ordinary page.
pub async fn discover() -> Result<Vec<Module>, String> {
    let dir = match folder(false).await {
        Ok(Some(dir)) => dir,
        // No folder is the ordinary case: nobody has installed anything.
        Ok(None) => return Ok(Vec::new()),
        Err(e) => {
            let why = described(e);
            log::warn!("no plugins: this page's storage is unreadable ({why})");
            return Err(why);
        }
    };
    let listed = match entries(&dir).await {
        Ok(names) => names,
        Err(e) => {
            let why = described(e);
            log::warn!("no plugins: cannot list this page's plugin folder ({why})");
            return Err(why);
        }
    };
    // Filtered, *then* truncated, which is the order the desktop's discovery
    // keeps: a directory holds whatever anybody put in it, and a name this
    // host cannot make a plugin id out of is not a plugin. Truncating the raw
    // listing spent a slot on each of them, so entries sorting early — a
    // stray file, a directory made through origin tooling — pushed real
    // modules out of the set that runs.
    let mut found: Vec<(String, String)> = listed
        .into_iter()
        .filter_map(|name| match plugin_id(&name) {
            Some(id) => Some((name, id)),
            None => {
                log::warn!("skipping {name}: its name is not a usable plugin id");
                None
            }
        })
        .collect();
    // Sorted by file name, because the order plugins load in is the order
    // their buttons are drawn in.
    found.sort();
    // And bounded before a byte is read, which is where the desktop bounds it
    // too and for the same reason: counting at the workers counted the
    // *successes*, so a folder of modules that each fail — after being read,
    // parsed and given their init fuel to refuse in — never reached the cap
    // at all. `Plugins::start` asks again; this is what keeps a folder of
    // tiny files from being read whole before it does.
    found.truncate(MAX_PLUGINS);

    let mut modules = Vec::new();
    let mut total = 0usize;
    for (name, id) in found {
        // What is left of the folder's budget, handed down so the size is
        // checked before the bytes are read rather than after. Checking the
        // total afterwards bounded what this *keeps* and not what it
        // allocates: one oversized file — written through origin tooling, or
        // left by a build that got away — would be copied into the tab whole
        // before anything refused it.
        let bytes = match read(&dir, &name, MAX_TOTAL_BYTES - total).await {
            Ok(bytes) => bytes,
            Err(e) => {
                log::warn!("not loading {id}: {e}");
                continue;
            }
        };
        total = total.saturating_add(bytes.len());
        modules.push(Module {
            id,
            open: Box::new(move || Ok(bytes)),
        });
    }
    Ok(modules)
}

/// Put `bytes` in the folder under `file_name`, and answer the id it claimed.
///
/// The name is the id, which is the desktop's rule and is why it is checked
/// here rather than at load: a file this page cannot name a plugin after is
/// one nothing will ever run, and telling somebody that at the moment they
/// chose it is the only useful moment.
///
/// The *folder's* budget is checked here too, and against what the folder
/// would become rather than against this module alone. `installed` stops
/// reading once the total is past [`MAX_TOTAL_BYTES`], so a second module
/// that fits on its own but not beside the first would be written, reported
/// as installed, and then silently skipped at every load after — an
/// installation notice for a plugin that never runs. Refused at the moment
/// somebody can still do something about it.
///
/// Weighing the folder and writing into it are one step, and
/// [`INSTALL_LOCK`] is what makes them one. Two installations overlap easily
/// — a second Add while the first file is still being written, or a second
/// tab of the same origin — and each would finish `occupied` against the same
/// total before either write landed, so two modules that do not fit together
/// would both be accepted and the second would never run.
///
/// # Errors
///
/// The name is not one a plugin may have, the folder has no room for it, or
/// the browser refused the write — a quota, or a mode with no storage.
pub async fn install(file_name: &str, bytes: Vec<u8>) -> Result<String, String> {
    let id = plugin_id(file_name)
        .ok_or_else(|| format!("`{file_name}` is not a name a plugin can have"))?;
    let name = format!("{id}.wasm");
    // Both halves, and neither swallowed: the outer answers whether the lock
    // ran the work at all, the inner what the work made of it.
    exclusively(INSTALL_LOCK, async move { place(name, bytes).await }).await??;
    Ok(id)
}

/// Weigh the folder and write into it. Runs under [`INSTALL_LOCK`].
async fn place(name: String, bytes: Vec<u8>) -> Result<(), String> {
    let dir = folder(true)
        .await
        .map_err(described)?
        .ok_or_else(|| "this page has no storage to keep a plugin in".to_owned())?;
    // How many, before how large. `installed` loads the first `MAX_PLUGINS`
    // by name and no more, so a folder already at the cap would take this
    // module, report it installed, and then never run one of them — which
    // one depending on where the new name sorts. Refused at the moment
    // somebody can still do something about it, exactly as the byte budget
    // is.
    let held = present(&dir, Some(&name)).await.map_err(described)?;
    if held >= MAX_PLUGINS {
        return Err(format!(
            "there is no room for it: {held} plugins already installed, which is the \
             {MAX_PLUGINS} this page loads. Remove one first."
        ));
    }
    // Everything except what is being replaced: reinstalling a plugin over
    // itself is not the folder growing, and counting the old copy would
    // refuse an update that fits perfectly well.
    let others = occupied(&dir, Some(&name)).await.map_err(described)?;
    let after = others.saturating_add(bytes.len());
    if after > MAX_TOTAL_BYTES {
        return Err(format!(
            "there is no room for it: {after} bytes of plugins, past the \
             {MAX_TOTAL_BYTES} this page loads. Remove one first."
        ));
    }
    write(&dir, &name, &bytes).await.map_err(described)
}

/// Run `work` while holding the origin-wide lock called `name`.
///
/// The browser holds a lock for exactly as long as the promise the callback
/// returns stays pending, so the work goes *inside* the callback and its
/// answer comes back out on a channel.
///
/// The closure is owned by a detached task rather than by this future, which
/// is the same rule the account's claim keeps and for the same reason: a
/// `Closure` freed while the lock manager still holds a reference is a panic
/// rather than a missed call, and the caller here is a UI task somebody can
/// navigate away from. It also means a write already under way finishes even
/// if nobody is left waiting for the answer, which is the right end for a
/// half-written module.
async fn exclusively<T: 'static>(
    name: &str,
    work: impl std::future::Future<Output = T> + 'static,
) -> Result<T, String> {
    let Some(locks) = web_sys::window().map(|window| window.navigator().locks()) else {
        // No lock manager at all — a context with no `navigator`, or a
        // browser older than the API. Nothing to serialize with, so the work
        // runs unguarded: the alternative is refusing to install anything on
        // a browser that can still hold the file perfectly well.
        log::warn!("this browser has no Web Locks; installing without serializing");
        return Ok(work.await);
    };

    let (tell, told) = futures_channel::oneshot::channel::<T>();
    let carried = std::cell::RefCell::new(Some((work, tell)));
    let callback = wasm_bindgen::prelude::Closure::<dyn FnMut(JsValue) -> js_sys::Promise>::new(
        move |_lock: JsValue| {
            let taken = carried.borrow_mut().take();
            wasm_bindgen_futures::future_to_promise(async move {
                if let Some((work, tell)) = taken {
                    let _ = tell.send(work.await);
                }
                Ok(JsValue::UNDEFINED)
            })
        },
    );
    let request = locks.request(name, callback.as_ref().unchecked_ref());
    let name = name.to_owned();
    wasm_bindgen_futures::spawn_local(async move {
        // The closure lives here, and nowhere the caller can drop it. The
        // request's promise settles when the callback's does, which is after
        // the work has finished and answered.
        let _held = callback;
        if let Err(e) = JsFuture::from(request).await {
            log::warn!("the {name} lock was not granted: {}", described(e));
        }
    });
    told.await
        .map_err(|_| "the browser did not run that installation".to_owned())
}

/// How many plugins the folder already holds, ignoring `replacing`.
async fn present(
    dir: &FileSystemDirectoryHandle,
    replacing: Option<&str>,
) -> Result<usize, JsValue> {
    Ok(entries(dir)
        .await?
        .iter()
        .filter(|name| replacing != Some(name.as_str()) && plugin_id(name).is_some())
        .count())
}

/// What the folder already holds, in bytes, ignoring `replacing`.
///
/// A file's size without reading it: a handle answers a `File`, and a `File`
/// knows how long it is before anybody asks for its bytes.
async fn occupied(
    dir: &FileSystemDirectoryHandle,
    replacing: Option<&str>,
) -> Result<usize, JsValue> {
    let mut total = 0usize;
    for name in entries(dir).await? {
        if replacing == Some(name.as_str()) || plugin_id(&name).is_none() {
            continue;
        }
        let handle: web_sys::FileSystemFileHandle = JsFuture::from(
            dir.get_file_handle_with_options(&name, &FileSystemGetFileOptions::new()),
        )
        .await?
        .dyn_into()?;
        let file: web_sys::File = JsFuture::from(handle.get_file()).await?.dyn_into()?;
        // `size` is a `f64` because every length in the web platform is;
        // saturating rather than wrapping, since what this feeds is a budget.
        total = total.saturating_add(file.size() as usize);
    }
    Ok(total)
}

/// Take one out of the folder.
///
/// By the name the folder actually holds, and not by one rebuilt from the id.
/// `plugin_id` accepts an uppercase extension — a file picker hands back
/// whatever the operating system called it — so a module written as
/// `autoreply.WASM` is listed, loaded and drawn as `autoreply`, and removing
/// `autoreply.wasm` would name an entry that is not there. OPFS is
/// case-sensitive, so every Remove on such a module answered `NotFoundError`
/// while the plugin went on running.
///
/// # Errors
///
/// The browser refused, which for a removal is either no storage or a handle
/// something else is holding open.
pub async fn uninstall(id: &str) -> Result<(), String> {
    let Some(dir) = folder(false).await.map_err(described)? else {
        return Ok(());
    };
    let held = entries(&dir)
        .await
        .map_err(described)?
        .into_iter()
        .find(|name| plugin_id(name).as_deref() == Some(id));
    // Nothing under that id is nothing to remove, which is the answer a
    // second press deserves rather than an error about a file the first press
    // took away.
    let Some(held) = held else {
        return Ok(());
    };
    JsFuture::from(dir.remove_entry(&held))
        .await
        .map(|_| ())
        .map_err(described)
}

/// The ids currently installed, whether or not they loaded.
///
/// What Settings lists beside the running plugins: a module that traps on
/// load is one somebody has to be able to remove, and it publishes no surface
/// to remove it from.
///
/// # Errors
///
/// The folder could not be read.
pub async fn names() -> Result<Vec<String>, String> {
    let Some(dir) = folder(false).await.map_err(described)? else {
        return Ok(Vec::new());
    };
    let mut ids: Vec<String> = entries(&dir)
        .await
        .map_err(described)?
        .iter()
        .filter_map(|name| plugin_id(name))
        .collect();
    ids.sort();
    Ok(ids)
}

/// The id a file carries: `autoreply.wasm` is `autoreply`.
///
/// The same rule the host holds a desktop file to, asked here so a page
/// cannot install something it would then silently skip.
fn plugin_id(file_name: &str) -> Option<String> {
    let stem = file_name.strip_suffix(".wasm").or_else(|| {
        // A browser's file picker hands back whatever the operating system
        // called it, and an uppercase extension is a file like any other.
        let (stem, ext) = file_name.rsplit_once('.')?;
        ext.eq_ignore_ascii_case("wasm").then_some(stem)
    })?;
    oxidezap_plugin_host::plugin_id_is_usable(stem).then(|| stem.to_owned())
}

/// The plugin folder inside this origin's filesystem, creating it only when
/// something is about to be put in it.
///
/// `Ok(None)` is "there is nowhere to look", which is what a page that has
/// never installed a plugin gets and is not a failure.
async fn folder(create: bool) -> Result<Option<FileSystemDirectoryHandle>, JsValue> {
    let Some(window) = web_sys::window() else {
        return Ok(None);
    };
    let root: FileSystemDirectoryHandle =
        JsFuture::from(window.navigator().storage().get_directory())
            .await?
            .dyn_into()?;
    let options = FileSystemGetDirectoryOptions::new();
    options.set_create(create);
    match JsFuture::from(root.get_directory_handle_with_options(DIR, &options)).await {
        Ok(handle) => Ok(Some(handle.dyn_into()?)),
        // A folder nobody has made yet, which the browser reports as a
        // `NotFoundError` rather than as an empty directory.
        //
        // *That* exception and not any of them. Answering absence to every
        // failure made a transient one — a permission the browser withheld, a
        // file sitting where the directory goes — read as "no plugins are
        // installed": `names` would then hide every one of them and
        // `uninstall` would report success without removing anything, leaving
        // the module to run again at the next reload.
        Err(e) if !create && is_missing(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Whether a browser's refusal was "there is nothing there".
///
/// By the exception's `name`, which is what the File System Access API is
/// specified to set, rather than by its message, which is the engine's to
/// word.
fn is_missing(e: &JsValue) -> bool {
    e.dyn_ref::<web_sys::DomException>()
        .is_some_and(|e| e.name() == "NotFoundError")
}

/// Every file name in `dir`.
///
/// Through the handle's own async iterator, which is the only listing a
/// browser gives — there is no `read_dir` that answers at once.
///
/// Each step is read with `Reflect::get` rather than cast to
/// `js_sys::IteratorNext`, and that is the whole of this module's history in
/// production. `IteratorNext` is declared in js-sys with no `is_type_of`, so
/// wasm-bindgen checks a cast to it with `instanceof IteratorNext` — against
/// a global no engine defines. The shim's own try/catch turns the resulting
/// `ReferenceError` into `false`, so `dyn_into` does not fail loudly: it
/// answers `Err` with the value handed back, *always*, whatever the object
/// is. Listing therefore failed on its first step for every page ever
/// served, and with it installing, listing and removing — reported as
/// `JsValue(Object({"done":true}))`, which is the perfectly good iterator
/// result the cast had just refused.
///
/// A `{done, value}` record is a shape rather than a type — nothing in any
/// browser is an instance of anything here — so reading the two fields is
/// not a workaround for the cast, it is what the cast was pretending to be.
/// `a_folder_lists_what_was_put_in_it` is what would have said so; it runs
/// in a browser, because nothing else can.
async fn entries(dir: &FileSystemDirectoryHandle) -> Result<Vec<String>, JsValue> {
    let iterator = dir.keys();
    let mut names = Vec::new();
    loop {
        let step = JsFuture::from(iterator.next()?).await?;
        if js_sys::Reflect::get(&step, &JsValue::from_str("done"))?.is_truthy() {
            return Ok(names);
        }
        if let Some(name) = js_sys::Reflect::get(&step, &JsValue::from_str("value"))?.as_string() {
            names.push(name);
        }
    }
}

/// One file's bytes, but only if `room` is left for them.
///
/// The size is asked of the `File` before `array_buffer` is, which is the
/// same order the picker asks it in and for the same reason: a file too large
/// to keep is not worth reading, and here it is worse than a wasted read —
/// this runs before the page has drawn anything, so an oversized module in
/// the folder would exhaust the tab at startup and take Settings, and the
/// Remove button in it, down with it. A `File` knows how long it is before
/// anybody asks for its bytes.
async fn read(dir: &FileSystemDirectoryHandle, name: &str, room: usize) -> Result<Vec<u8>, String> {
    let bytes = async {
        let handle: web_sys::FileSystemFileHandle = JsFuture::from(
            dir.get_file_handle_with_options(name, &FileSystemGetFileOptions::new()),
        )
        .await?
        .dyn_into()?;
        let file: web_sys::File = JsFuture::from(handle.get_file()).await?.dyn_into()?;
        let size = file.size() as usize;
        if size > room {
            return Ok::<_, JsValue>(Err(size));
        }
        let buffer = JsFuture::from(file.array_buffer()).await?;
        Ok(Ok(Uint8Array::new(&buffer).to_vec()))
    }
    .await;
    match bytes.map_err(described)? {
        Ok(bytes) => Ok(bytes),
        Err(size) => Err(format!(
            "it is {size} bytes and only {room} are left to read"
        )),
    }
}

/// Replace one file's bytes.
///
/// A writable stream truncates on open and the rename a desktop write ends
/// with has no equivalent here, so a page killed mid-write leaves a short
/// file. That is survivable for the one thing written this way: a truncated
/// module fails to parse and the plugin does not load, where a truncated
/// *approval* would be a permission read wrong — which is why the approvals
/// are in `localStorage`, whose `setItem` either replaces the value or
/// throws.
async fn write(dir: &FileSystemDirectoryHandle, name: &str, bytes: &[u8]) -> Result<(), JsValue> {
    let options = FileSystemGetFileOptions::new();
    options.set_create(true);
    let handle: web_sys::FileSystemFileHandle =
        JsFuture::from(dir.get_file_handle_with_options(name, &options))
            .await?
            .dyn_into()?;
    let stream: FileSystemWritableFileStream =
        JsFuture::from(handle.create_writable()).await?.dyn_into()?;
    // Copied into a JS-owned array first. `write_with_u8_array` takes a
    // mutable slice of *our* linear memory, and the write is a promise: a
    // memory that grows while it is outstanding moves the buffer under it.
    let held = Uint8Array::new_with_length(u32::try_from(bytes.len()).unwrap_or(u32::MAX));
    held.copy_from(bytes);
    JsFuture::from(stream.write_with_buffer_source(&held)?).await?;
    JsFuture::from(stream.close()).await?;
    Ok(())
}

/// What a browser said, in words, for the one log line each caller writes.
fn described(e: JsValue) -> String {
    e.dyn_ref::<js_sys::Error>()
        .map(|e| String::from(e.message()))
        .unwrap_or_else(|| format!("{e:?}"))
}

#[cfg(test)]
mod tests;
