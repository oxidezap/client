//! A desktop's plugin folder, and the way a plugin acts from inside one.
//!
//! The two questions the page's half answers, answered here with a filesystem
//! and a thread pool under them: where a module comes from — a directory only
//! this user can write — and where the work runs.
//!
//! That second one is the whole of this file. Loading reads up to
//! `MAX_MODULE_BYTES` off the disk, validates it and runs its `oxi_init`,
//! none of which yields; recording an approval writes a file and renames it.
//! Both go to [`tokio::task::spawn_blocking`] rather than parking a runtime
//! worker, and both read what came back rather than dropping it — a loader
//! that panicked and was reported as having worked is the failure each of the
//! comments below is about.

use std::sync::Arc;

use oxidezap_plugin_host::{Outcome, Plugins, Reloaded, Sink};

use super::{Bridge, publishing_to};
use crate::session_bridge::{Action, CommandOutcome, Commands as SessionCommands, SessionCommand};
use crate::state::StateHub;

/// See [`super::start`].
///
/// Off the runtime's thread, for the reason in this module's header: done
/// inline it parks a runtime worker for as long as the folder takes, before
/// the daemon has bound anything. Awaited rather than detached, because the
/// session must not start until the plugins subscribed to messages are there
/// to receive them.
pub(super) async fn start(hub: &Arc<StateHub>, commands: SessionCommands) -> Arc<Plugins> {
    let sink = publishing_to(hub);
    let fallback = (publishing_to(hub), commands.clone());
    tokio::task::spawn_blocking(move || load(sink, commands))
        .await
        .unwrap_or_else(|e| {
            // With the daemon's own sink and bridge, not a discarding pair.
            // This host used to be a dead end and is not one any more: a
            // reload can put real plugins into it once whatever made the
            // loader panic has been taken out of the folder, and one built to
            // publish nowhere would run them with their interface discarded
            // and every command answering `NoSession` — while reporting the
            // reload as having worked.
            log::error!("the plugin loader did not finish: {e}");
            let (sink, commands) = fallback;
            Arc::new(Plugins::none(sink, Arc::new(Bridge { commands })))
        })
}

/// The scan itself, on the blocking thread [`start`] put it on.
fn load(sink: Sink, commands: SessionCommands) -> Arc<Plugins> {
    let Some(dir) = oxidezap_plugin_host::default_dir() else {
        log::debug!("no per-user data directory, so no plugins");
        return Arc::new(Plugins::none(sink, Arc::new(Bridge { commands })));
    };
    // Not the daemon's `state_dir`: that one prefers XDG_RUNTIME_DIR, which
    // is cleared on logout, and a permission answer that does not survive a
    // logout is a prompt asked forever.
    let state_dir = oxidezap_plugin_host::default_state_dir();
    Arc::new(Plugins::load(
        &dir,
        state_dir.as_deref(),
        Arc::new(Bridge { commands }),
        sink,
    ))
}

/// See [`super::reload`].
///
/// The mirror of [`start`], down to where the work happens: the scan reads
/// files and runs each `oxi_init`, all of it synchronous, so it goes to a
/// blocking thread as well.
pub(super) async fn reload(plugins: &Arc<Plugins>) -> Reloaded {
    let Some(dir) = oxidezap_plugin_host::default_dir() else {
        log::debug!("no per-user data directory, so nothing to reload");
        return Reloaded::Kept(0);
    };
    let state_dir = oxidezap_plugin_host::default_state_dir();
    let plugins = Arc::clone(plugins);
    tokio::task::spawn_blocking(move || plugins.reload_from_dir(&dir, state_dir.as_deref()))
        .await
        .unwrap_or_else(|e| {
            // Not a reload that installed nothing: a reload that did not
            // happen. The live set is whatever it was — the reservation guard
            // gives the slot back however the loader ends — and saying
            // "0 running" here put a successful-looking count directly under
            // the error.
            log::error!("the plugin loader did not finish: {e}");
            Reloaded::Failed
        })
}

/// Where [`super::reload_in_background`] puts its work: the daemon's runtime,
/// which is work-stealing, so what it is handed has to be [`Send`].
pub(super) fn detach(work: impl std::future::Future<Output = ()> + Send + 'static) {
    drop(tokio::spawn(work));
}

/// See [`super::approve`].
///
/// A write and a rename, which is disk I/O and does not belong on a runtime
/// worker. The answer is read, not dropped: a panic in there left the client
/// acknowledged for a permission the disk never received, with Settings
/// drawing a state nothing had recorded and no line in the log.
pub(super) async fn approve(plugins: &Arc<Plugins>, plugin: String, approved: bool) -> bool {
    let plugins = Arc::clone(plugins);
    match tokio::task::spawn_blocking(move || plugins.approve(&plugin, approved)).await {
        Ok(recorded) => recorded,
        Err(e) => {
            log::error!("recording a plugin approval failed: {e}");
            false
        }
    }
}

/// See [`super::install`].
///
/// Off the runtime's thread, like every other filesystem answer here: this
/// creates a directory, lists it and writes up to thirty-two megabytes.
///
/// The folder is created the way everything else this daemon owns is —
/// private to this account — because that is not a nicety here but the
/// condition on plugins running at all: the host refuses to load anything out
/// of a directory another local account can write, since an approval is
/// recorded against a plugin's id rather than against its bytes. Installing
/// into a folder nothing would load from is the one outcome worth refusing
/// outright rather than reporting as success.
pub(super) async fn install(id: String, bytes: Vec<u8>) -> Result<String, String> {
    let Some(dir) = oxidezap_plugin_host::default_dir() else {
        return Err("there is no per-user data directory to keep a plugin in".to_owned());
    };
    // Before the thread, because it is the answer the length alone already
    // decides: the loader reads a module's size before it opens the file and
    // skips anything past this, so a larger one would be written, reported as
    // installed, and then silently never run.
    if bytes.len() > oxidezap_plugin_host::MAX_MODULE_BYTES {
        return Err(format!(
            "it is {} bytes, past the {} a plugin may be",
            bytes.len(),
            oxidezap_plugin_host::MAX_MODULE_BYTES
        ));
    }
    match tokio::task::spawn_blocking(move || place(&dir, &id, &bytes).map(|()| id)).await {
        Ok(placed) => placed,
        Err(e) => {
            log::error!("installing a plugin did not finish: {e}");
            Err("that plugin could not be installed".to_owned())
        }
    }
}

/// One installation at a time, whatever asked for it.
///
/// The page's half takes a Web Lock and says why: weighing a folder and
/// writing into it are one step, and two of them overlapping each finish
/// counting before either write lands. The same is true here — two front ends
/// share one daemon, each install runs on a blocking thread of its own, and a
/// double press of Add is two — so the same rule needs the same lock. This
/// one spans this process rather than the origin, which is the whole of the
/// folder's reach: a desktop's plugin directory is written by one daemon.
///
/// Held across the listing, the cap check and the write, and released by the
/// guard however the work ends.
static INSTALLING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Weigh the folder and write into it, on the blocking thread.
fn place(dir: &std::path::Path, id: &str, bytes: &[u8]) -> Result<(), String> {
    // Poisoning is not a reason to refuse: what a panicking installer can
    // have left behind is a `.part` file, which nothing loads and the next
    // install replaces.
    let _installing = INSTALLING.lock().unwrap_or_else(|e| e.into_inner());
    // The parent first: `prepare` creates one directory and this is two deep
    // under the data directory on a machine that has never run a plugin.
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    if crate::private_dir::prepare(dir, "plugins").map_err(|e| e.to_string())?
        == crate::private_dir::Found::WasOpen
    {
        // Tightened, and said: closing the door now says nothing about what
        // is already in the room, and what is in this room is code this
        // daemon runs.
        log::warn!(
            "{} was reachable by other accounts on this machine; it is now private, but check \
             what is in it",
            dir.display()
        );
    }
    // How many, before the write. The loader takes the first `MAX_PLUGINS` by
    // name and no more, so a folder already at the cap would take this module,
    // report it installed, and then never run one of them — which one
    // depending on where the new name sorts. The page's half refuses at the
    // same point and for the same sentence.
    let held = listed(dir)?;
    if !held.iter().any(|other| other == id) && held.len() >= oxidezap_plugin_host::MAX_PLUGINS {
        return Err(format!(
            "there is no room for it: {} plugins already installed, which is the {} this daemon \
             loads. Remove one first.",
            held.len(),
            oxidezap_plugin_host::MAX_PLUGINS
        ));
    }
    // Written beside the name and renamed onto it, so the name holds a whole
    // module or nothing: a reload can run at any moment, including this one,
    // and a partly written file is one the loader reads, refuses and logs
    // about a plugin nobody has broken. The temporary is dotted and carries
    // no `.wasm`, which is what keeps a scan that catches it mid-write from
    // taking it for a module.
    let target = dir.join(format!("{id}.wasm"));
    let partial = dir.join(format!(".{id}.wasm.part"));
    // Cleaned up however this ends, including part-way through the write: a
    // temporary left behind is one nothing will ever load and nothing will
    // ever take away, since it carries neither the extension a scan looks for
    // nor a name a removal would name.
    let written = write_private(&partial, bytes)
        .map_err(|e| format!("{}: {e}", partial.display()))
        .and_then(|()| {
            std::fs::rename(&partial, &target).map_err(|e| format!("{}: {e}", target.display()))
        });
    if let Err(e) = written {
        let _ = std::fs::remove_file(&partial);
        return Err(e);
    }
    // The bytes are flushed by the write; the directory entry that gives them
    // their name is separate metadata, and a machine that lost power between
    // the two would come back with a plugin the person was told they had.
    if let Err(e) = oxidezap_plugin_host::sync_dir(dir) {
        log::warn!("{} was not flushed after an install: {e}", dir.display());
    }
    Ok(())
}

/// Create the file with a mode only this account can write, and fill it.
///
/// The umask is not enough: this is a module the daemon will execute, and the
/// loader refuses one any other account could rewrite — so a file created at
/// whatever the umask allowed would be an install that silently never loads.
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// See [`super::uninstall`].
///
/// By the name the folder actually holds rather than by one rebuilt from the
/// id: the loader accepts an uppercase extension, so a module written as
/// `autoreply.WASM` is loaded and drawn as `autoreply`, and removing
/// `autoreply.wasm` would name an entry that is not there.
pub(super) async fn uninstall(id: &str) -> Result<(), String> {
    let Some(dir) = oxidezap_plugin_host::default_dir() else {
        return Ok(());
    };
    let id = id.to_owned();
    match tokio::task::spawn_blocking(move || remove(&dir, &id)).await {
        Ok(removed) => removed,
        Err(e) => {
            log::error!("removing a plugin did not finish: {e}");
            Err("that plugin could not be removed".to_owned())
        }
    }
}

fn remove(dir: &std::path::Path, id: &str) -> Result<(), String> {
    // Under the same lock the install takes: a removal listing the folder
    // while an install is renaming into it would answer about a folder
    // neither of them is looking at.
    let _installing = INSTALLING.lock().unwrap_or_else(|e| e.into_inner());
    let mut took = false;
    for name in entries(dir)? {
        if super::plugin_id(&name).as_deref() != Some(id) {
            continue;
        }
        match std::fs::remove_file(dir.join(&name)) {
            Ok(()) => took = true,
            // Not there is not a failure: something else took it, which is
            // the state the caller asked for.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("{name}: {e}")),
        }
    }
    // The removal is a directory entry like the rename is, and it has to
    // survive a power loss for the same reason: a plugin that came back after
    // one is a plugin somebody stopped and did not. Only where something was
    // really taken — a folder that is not there has no entry to flush, and
    // asking anyway is a warning on every removal a fresh machine makes.
    if took && let Err(e) = oxidezap_plugin_host::sync_dir(dir) {
        log::warn!("{} was not flushed after a removal: {e}", dir.display());
    }
    Ok(())
}

/// See [`super::names`].
pub(super) async fn names() -> Result<Vec<String>, String> {
    let Some(dir) = oxidezap_plugin_host::default_dir() else {
        // Nowhere to look is not a folder that could not be read: nobody has
        // installed anything and nobody can, which the caller draws as an
        // empty list rather than as a failure.
        return Ok(Vec::new());
    };
    match tokio::task::spawn_blocking(move || listed(&dir)).await {
        Ok(listed) => listed,
        Err(e) => {
            log::error!("listing the plugin folder did not finish: {e}");
            Err("the plugin folder could not be read".to_owned())
        }
    }
}

/// Every id the folder holds, whether or not it loaded.
///
/// Not the loader's own discovery, which answers what will *run*: a directory
/// another account can write loads nothing, and a module that refuses to
/// parse publishes nothing — and both are exactly the file somebody needs to
/// see listed so they can take it out again.
fn listed(dir: &std::path::Path) -> Result<Vec<String>, String> {
    Ok(entries(dir)?
        .iter()
        .filter_map(|name| super::plugin_id(name))
        .collect())
}

/// What the folder holds, by name.
///
/// A folder that is not there is empty; one that cannot be *read* is an
/// error, and the difference is the whole reason this answers a `Result`. A
/// failed read drawn as an empty folder is a settings pane that takes away
/// the Remove button under somebody's hand.
fn entries(dir: &std::path::Path) -> Result<Vec<String>, String> {
    let read = match std::fs::read_dir(dir) {
        Ok(read) => read,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    };
    let mut names = Vec::new();
    for entry in read {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        // Files, which is what the loader's own discovery keeps. A
        // *directory* called `autoreply.wasm` would otherwise be listed as an
        // installed plugin with a Remove button, and every press of it would
        // fail on unlinking a directory — a row nobody could ever clear.
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        if let Some(name) = entry.file_name().to_str() {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

impl Bridge {
    /// Hand one action to the session and wait for what it made of it.
    ///
    /// Blocking, on the plugin's own thread, and that is the point: the
    /// answer *is* what the plugin gets out of the call, and a queue would
    /// hand it back the same "it was taken" a socket front end already has to
    /// live with. Nothing on the daemon's side waits for this — the plugin
    /// thread is the only one parked — and a plugin parked here is one whose
    /// own queue is filling, which the host already has a rule for.
    pub(super) fn ask(&self, action: Action) -> Outcome {
        let (reply, answer) = tokio::sync::oneshot::channel();
        if self
            .commands
            .blocking_send(SessionCommand { action, reply })
            .is_err()
        {
            // The bridge is gone: the daemon is shutting down, which is not a
            // refusal of this particular command.
            return Outcome::NoSession;
        }
        match answer.blocking_recv() {
            Ok(CommandOutcome::Accepted) => Outcome::Accepted,
            Ok(CommandOutcome::NoSession(_)) | Err(_) => Outcome::NoSession,
            // The plugin ABI has one word for both, and this is the honest
            // half of it: a plugin's command did not run. Widening its
            // `Outcome` is an ABI change, and no plugin retries anything.
            Ok(CommandOutcome::Refused(_) | CommandOutcome::Busy(_)) => Outcome::Refused,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{listed, place, remove};

    /// The smallest module a wasm host will parse: the magic and the version.
    ///
    /// Enough for everything here, which is about the folder rather than about
    /// what is in it — the host's own tests cover loading, and these are the
    /// desktop twin of `plugins/web/tests.rs`.
    const MODULE: &[u8] = b"\0asm\x01\0\0\0";

    fn scratch(what: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "oxidezap-plugins-{what}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// The whole of what a desktop front end could not do until now.
    ///
    /// It is not a gap in the interface: a window that asked was answered
    /// "this front end cannot install plugins", because installing meant
    /// writing the folder and only the one front end that *was* the daemon
    /// had one to write. The folder is the daemon's on both platforms, so
    /// this is the daemon's answer on both.
    #[test]
    fn a_module_is_installed_listed_and_removed() {
        let dir = scratch("round-trip");
        place(&dir, "autoreply", MODULE).expect("it installs");
        assert_eq!(listed(&dir).expect("the folder lists"), vec!["autoreply"]);
        assert_eq!(
            std::fs::read(dir.join("autoreply.wasm")).expect("the file is there"),
            MODULE,
            "the bytes under the name are the ones that were staged"
        );

        remove(&dir, "autoreply").expect("it is removed");
        assert!(
            listed(&dir).expect("the folder still lists").is_empty(),
            "and the folder is empty again"
        );
        // Twice is not a failure: a second press deserves the answer the
        // first one produced rather than an error about a file it took away.
        remove(&dir, "autoreply").expect("removing nothing is nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The condition on a plugin running at all, and so on installing one
    /// being worth anything: the host refuses to load out of a directory
    /// another local account can write, because an approval is recorded
    /// against an id rather than against bytes.
    #[cfg(unix)]
    #[test]
    fn what_is_installed_is_what_the_host_will_load() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = scratch("private");
        place(&dir, "autoreply", MODULE).expect("it installs");

        let mode = |path: &std::path::Path| {
            std::fs::metadata(path)
                .expect("it is there")
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(&dir), 0o700, "the folder is this account's alone");
        assert_eq!(
            mode(&dir.join("autoreply.wasm")) & 0o022,
            0,
            "and so is the module: a file anybody else may rewrite is one the \
             loader skips, so installing it would report a plugin that never runs"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A second install of the same id replaces rather than duplicates, and
    /// the name it lands under is the id's — whatever the file was called.
    #[test]
    fn reinstalling_replaces() {
        let dir = scratch("replace");
        place(&dir, "twice", MODULE).expect("installed once");
        let longer = [MODULE, b"\0\0\0\0"].concat();
        place(&dir, "twice", &longer).expect("installed again");
        assert_eq!(listed(&dir).expect("listed"), vec!["twice"]);
        assert_eq!(
            std::fs::read(dir.join("twice.wasm")).expect("the file is there"),
            longer
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A folder that could not be read is not an empty folder. Drawn as one,
    /// it takes away the Remove button somebody was about to press — so the
    /// two answers stay apart all the way up to the pane.
    #[test]
    fn a_folder_that_is_not_there_is_empty_and_one_that_is_a_file_is_an_error() {
        let missing = scratch("missing");
        assert!(
            listed(&missing).expect("absence is an answer").is_empty(),
            "nobody has installed anything, which is the ordinary account"
        );

        let blocked = scratch("blocked");
        std::fs::create_dir_all(blocked.parent().expect("a parent")).expect("a parent");
        std::fs::write(&blocked, b"not a directory").expect("something in the way");
        assert!(listed(&blocked).is_err(), "and this is not absence");
        let _ = std::fs::remove_file(&blocked);
    }

    /// A directory is not a module, whatever it is called. Listed as one it
    /// would draw a Remove button whose every press fails on unlinking a
    /// directory — a row nobody could clear.
    #[test]
    fn a_directory_named_like_a_module_is_not_one() {
        let dir = scratch("not-a-module");
        place(&dir, "real", MODULE).expect("it installs");
        std::fs::create_dir(dir.join("impostor.wasm")).expect("a directory in the folder");
        assert_eq!(listed(&dir).expect("the folder lists"), vec!["real"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The id rule is the host's, asked before a byte is written: a file
    /// this daemon cannot name a plugin after is one nothing would ever load.
    #[tokio::test]
    async fn a_name_that_is_not_an_id_is_refused() {
        for name in ["../escape.wasm", "notwasm.txt", "two words.wasm", ".wasm"] {
            assert!(
                super::super::install(name, MODULE.to_vec()).await.is_err(),
                "{name} was accepted"
            );
        }
    }
}
