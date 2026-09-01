//! A folder of `.wasm` files, and the permissions that make it trustworthy.
//!
//! A desktop's plugins are files somebody dropped in a directory, which is
//! the whole of the installation story and the whole of the problem: an
//! approval is recorded against a plugin's id and mask rather than its bytes
//! — deliberately, so an update does not re-ask — so *who else can put a
//! file at that name* is the question every check below answers. Scanning,
//! reading and naming are the easy half; the mode and owner checks around
//! them are why this is one place rather than three.
//!
//! Two axes meet here and only one of them is above this file. Whether there
//! is a filesystem at all is answered by which of `native.rs` and `web.rs`
//! the parent module compiles; whether that filesystem has uids and modes is
//! answered by `#[cfg(unix)]` *inside* the functions that ask, because a
//! Windows plugin directory sits under `%LOCALAPPDATA%` and its ACL is the
//! profile's. Flattening the two into one predicate would put "there is no
//! disk" and "there are no modes" behind the same name, and they are not the
//! same answer.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use portable_atomic::AtomicU32;

use crate::{
    Backing, Commands, MAX_MODULE_BYTES, MAX_PLUGINS, Module, Nowhere, Plugins, Reloaded, Sink,
    approvals, plugin_id_is_usable, reload, store,
};

impl Plugins {
    /// Load every `.wasm` in `dir`.
    ///
    /// A missing directory is not an error: the ordinary machine has no
    /// plugins, and a daemon that refused to start over an absent folder
    /// would be a daemon that refused to start.
    ///
    /// `state_dir` is where a plugin's own settings live, one document per
    /// plugin. `None` runs them with memory-only storage, which is what a
    /// test wants and what a machine with no writable home gets.
    #[must_use]
    pub fn load(
        dir: &Path,
        state_dir: Option<&Path>,
        commands: Arc<dyn Commands>,
        sink: Sink,
    ) -> Self {
        // The approvals live beside the plugins themselves and never in a
        // plugin's key-value store: one that could write its own approval has
        // none.
        let state: Arc<dyn Backing> = match usable_state_dir(state_dir) {
            Some(dir) => Arc::new(store::Files::at(dir)),
            None => Arc::new(Nowhere),
        };
        // At the first load an unreadable folder is no plugins, which is what
        // it has always been: nothing is running to lose, and a daemon that
        // would not come up over a directory is a daemon that would not come
        // up. A *reload* asks the same function and treats `None` differently,
        // which is the whole reason it answers one.
        let modules = modules_in(dir).unwrap_or_default();
        // Driven here rather than propagated: a desktop's loader owns the
        // thread it runs on — `plugins::start` puts it on `spawn_blocking` —
        // so there is nothing for it to yield to and `breathe` is a no-op.
        // What the `async` shape buys is the page, where the same loop has to
        // hand the browser a turn between modules.
        futures_lite::future::block_on(Self::start(modules, state, commands, sink))
    }

    /// Read `dir` again and replace what is running with what is in it now.
    ///
    /// [`Plugins::reload`] above a filesystem, exactly as [`Plugins::load`]
    /// is [`Plugins::start`] above one — the same scan, the same id rules,
    /// the same backing rebuilt from the same path, so a reloaded folder and
    /// a freshly started one cannot disagree about what they found.
    ///
    /// Blocking: it reads every module and runs each `oxi_init`. The caller
    /// is `daemon::plugins::reload`, which puts it where `load` goes.
    pub fn reload_from_dir(&self, dir: &Path, state_dir: Option<&Path>) -> Reloaded {
        futures_lite::future::block_on(self.reload(|| async move {
            // Asked again on every scan rather than settled once: a directory
            // that was private at startup may not be now, and the answer to
            // that is the store the answers are then kept in — or refused,
            // which `rebind` turns into every plugin being unapproved again.
            // The folder first, and the state directory only once there is
            // going to be a reload. `None` here is a folder that could not be
            // *read*, which leaves the running set alone rather than
            // replacing it with an empty one — not the same as a folder that
            // is absent, or one this host refuses to trust, both of which are
            // answers a reload should act on.
            //
            // Asking in this order matters because the two questions are not
            // independent. `usable_state_dir` is re-asked on every scan, and
            // its refusal is meant to take effect — grants cleared, storage
            // dropped — which only happens at the install. Deciding it first
            // and then abandoning the reload left that refusal discovered and
            // not applied, with the running generation carrying on through
            // the very directory just declared unsafe. Nothing is asked about
            // it unless the answer is going to be used.
            let modules = modules_in(dir)?;
            let state: Arc<dyn Backing> = match usable_state_dir(state_dir) {
                Some(dir) => Arc::new(store::Files::at(dir)),
                None => Arc::new(Nowhere),
            };
            Some((modules, state))
        }))
    }
}

/// Every `.wasm` in `dir` that could be a plugin, as modules nobody has
/// opened yet.
///
/// One scan, called by the first load and by every reload: a second one would
/// be a second answer to which names are ids and in what order they are
/// taken, which is exactly the kind of disagreement `MAX_PLUGINS` truncating
/// a folder makes visible.
pub fn modules_in(dir: &Path) -> Option<Vec<Module>> {
    Some(
        discover(dir)?
            .into_iter()
            .filter_map(|path| {
                let Some(id) = plugin_id(&path) else {
                    log::warn!(
                        "skipping {}: its name is not a usable plugin id",
                        path.display()
                    );
                    return None;
                };
                Some(Module {
                    id,
                    open: Box::new(move || read_module(&path)),
                })
            })
            .collect(),
    )
}

/// Every `.wasm` in `dir`, in a stable order.
///
/// Sorted by name, because the order plugins load in is the order their
/// buttons are drawn in, and a set that reshuffled between two starts would
/// move a control under somebody's hand.
pub fn discover(dir: &Path) -> Option<Vec<PathBuf>> {
    // A directory anybody else can write is one where the file that runs
    // tomorrow is not the file that was approved today. Approval is recorded
    // against a plugin's id and mask rather than its bytes — deliberately, so
    // an update does not re-ask — which is exactly what makes a replaceable
    // file dangerous: another local account dropping its own `autoreply.wasm`
    // there inherits whatever the owner once agreed to. Refused whole rather
    // than per file, because a writable directory is one where a *new* name
    // can appear too.
    // Absence first, and silently: the ordinary machine has no plugin
    // directory at all, and answering that with a warning about other users
    // being able to write it reports a vulnerability that does not exist, on
    // every start, to everyone who has never installed a plugin. The check
    // below cannot tell the two apart on its own — it reads metadata, and a
    // directory that is not there has none.
    match dir.try_exists() {
        Ok(true) => {}
        Ok(false) => return Some(Vec::new()),
        Err(e) => {
            // `exists()` folds this into `false`, which is the one answer it
            // must not be: a parent directory that cannot be read, or any
            // other metadata error, would be reported as a folder with no
            // plugins in it — and a reload takes that for an answer and
            // retires every healthy plugin. `try_exists` is the same question
            // with the third outcome kept.
            log::warn!("cannot tell whether {} is there: {e}", dir.display());
            return None;
        }
    }
    if !only_this_user_can_write(dir) {
        log::warn!(
            "not loading any plugins from {}: it can be written by other users on this \
             machine, and a plugin's approval is recorded against its name rather than \
             its contents",
            dir.display()
        );
        // An answer, not a failure: a directory somebody else can write is
        // one whose plugins this host will not run, and a reload finding that
        // *should* stop them.
        return Some(Vec::new());
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            // Said, and told apart from the two answers above. A directory
            // that is not there and one this host refuses to trust are both
            // "no plugins", deliberately; one it cannot *read* is not an
            // answer at all, and a reload that took it for one would retire
            // every healthy plugin over a transient error.
            log::warn!("cannot read {}: {e}", dir.display());
            return None;
        }
    };
    // Every entry, or none of them. `filter_map(Result::ok)` here was the
    // same conflation the `read_dir` above stopped making one level up: an
    // entry this host could not read is not an entry that is not there, and a
    // reload that took a short listing for the folder would retire a healthy
    // plugin over a transient error, with nothing removed and nothing to put
    // it back.
    let entries: Vec<std::fs::DirEntry> = match entries.collect::<Result<_, _>>() {
        Ok(entries) => entries,
        Err(e) => {
            log::warn!("cannot read an entry of {}: {e}", dir.display());
            return None;
        }
    };
    let mut found: Vec<PathBuf> = entries
        .into_iter()
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("wasm"))
        })
        .filter(|p| p.is_file())
        // And each module, for the same reason: a directory only this user
        // may write can still hold a file somebody else may, through a mode
        // set by hand or a copy that carried one.
        .filter(|p| {
            only_this_user_can_write(p) || {
                log::warn!(
                    "skipping {}: it can be written by other users on this machine",
                    p.display()
                );
                false
            }
        })
        .collect();
    found.sort();
    // Bounded here rather than where a worker starts. Counting the workers
    // counted the *successes*, so a folder of modules that each fail — after
    // being read, parsed, instantiated and given two hundred million fuel to
    // refuse in — never reached the cap at all, and the daemon did all of
    // that with its socket still closed. Truncated after the sort, so which
    // ones run is the answer discovery always gives: the first `MAX_PLUGINS`
    // by name.
    found.truncate(MAX_PLUGINS);
    Some(found)
}

/// One module's bytes, bounded before the file is opened.
///
/// The size is asked of the *file* rather than of what was read: the bytes,
/// and everything wasmi allocates parsing them, are spent before the store —
/// and so before its limiter — exists. One downloaded file with an enormous
/// section would otherwise exhaust the daemon during startup and take the
/// account down with it.
fn read_module(path: &Path) -> anyhow::Result<Vec<u8>> {
    use anyhow::Context as _;

    let size = std::fs::metadata(path)
        .with_context(|| format!("reading {}", path.display()))?
        .len();
    if size > MAX_MODULE_BYTES as u64 {
        return Err(anyhow::anyhow!(
            "it is {size} bytes, past the {MAX_MODULE_BYTES} a plugin may be"
        ));
    }
    std::fs::read(path).with_context(|| format!("reading {}", path.display()))
}

/// Whether only this account can change what is at `path`.
///
/// Mode *and* owner: a file another user owns is one they may rewrite
/// whatever its permission bits say, and a mode that grants group or world
/// write is one anybody in that group may rewrite whatever owns it. Answering
/// `false` when the metadata cannot be read at all is the safe direction —
/// this decides whether to run somebody's code.
///
/// A symlink is refused outright rather than followed, because following one
/// answers about the wrong thing: the target can be owned by this user and
/// `0644` and still sit in a directory somebody else may write, and a file in
/// such a directory is one they can unlink and replace whatever its own mode
/// says. The replacement would then inherit the id's recorded approval. What
/// it would take to allow the link is a verdict on the target's directory,
/// and on that directory's directory — a walk to the root, with a race at
/// every step — where the rule that a module is a file is one line and
/// checkable. Loading from somewhere else is what `OXIDEZAP_PLUGIN_DIR` is
/// for, and it is checked the same way.
///
/// Nothing to check off unix: a Windows plugin directory sits under
/// `%LOCALAPPDATA%`, whose ACL is the profile's, and this process has no
/// business inventing a second answer to a question the ACL already answers.
pub fn only_this_user_can_write(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        // `symlink_metadata` first, and about the link rather than through
        // it: `metadata` would answer for the target and say nothing about
        // who can put a different file there.
        let Ok(link) = std::fs::symlink_metadata(path) else {
            return false;
        };
        if link.file_type().is_symlink() {
            return false;
        }
        let Ok(meta) = std::fs::metadata(path) else {
            return false;
        };
        // Root owning it is the ordinary case for a system-wide install, and
        // root can rewrite anything anyway.
        let ours = meta.uid() == current_uid() || meta.uid() == 0;
        ours && meta.mode() & 0o022 == 0
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        true
    }
}

/// This process's real user id.
#[cfg(unix)]
fn current_uid() -> u32 {
    // The same syscall the daemon and the IPC crate make, from the same
    // crate: no `unsafe` at this call site, and one dependency fewer in a
    // crate that otherwise has none of either.
    rustix::process::getuid().as_raw()
}

/// Remove only the record of what the user allowed.
///
/// The fallback for an account reset whose directory removal did not go
/// through. What survives a partial wipe is inherited by whoever pairs next,
/// and the half that must not survive is this one: a plugin's leftover
/// settings are the old account's data, but a leftover approval is a plugin
/// acting on a *new* account under permission given for the old one.
pub fn forget_approvals(state_dir: &Path) -> std::io::Result<()> {
    std::fs::remove_file(state_dir.join(approvals::FILE_NAME))?;
    // The same reason a revocation's rename is flushed: unlinking removes a
    // directory entry, which is metadata POSIX says nothing about the timing
    // of. Losing power after this returned and before the entry reached the
    // disk brings `approvals.json` back — and the caller has by then wiped
    // the credentials, so what comes back is the old account's grants over
    // whoever pairs next. Flushed here rather than by the caller, because
    // this function's answer is what "retired" means.
    sync_dir(state_dir)
}

/// The state directory, if this daemon can make it its own.
///
/// Asked *before* anything is read out of it, and answering `None` is what
/// makes the check worth anything: `approvals.json` says what each plugin may
/// do to the account, so a directory another local account can write is one
/// where that file says what somebody else decided. Reading it first and
/// tightening the mode afterwards — which is what this used to do — put the
/// mask in memory before the permissions changed, and a repair that failed
/// was a line in a log that nothing acted on.
///
/// `None` fails closed rather than refusing to run the plugins: they draw and
/// keep settings in memory, and everything that touches the account is
/// unapproved until somebody says yes in this session. A plugin that cannot
/// store a preference is a smaller problem than one acting on a permission
/// nobody here granted.
pub fn usable_state_dir(dir: Option<&Path>) -> Option<&Path> {
    let dir = dir?;
    if let Err(e) = create_private_dir(dir) {
        log::warn!(
            "not using {}: {e}. Plugin settings will not survive a restart, and \
             permissions must be granted again — a directory this daemon cannot \
             make private is one whose recorded approvals are not the user's.",
            dir.display()
        );
        return None;
    }
    // The directory itself, asked the same question the plugin directory is
    // asked. `create_dir_all` and `set_permissions` both follow a symlink, so
    // a link left where the state directory goes had this daemon tighten and
    // then write into somebody else's directory — approvals and every
    // plugin's settings with it. The forged `approvals.json` is still barred
    // by the owner check below, so what this closes is the redirection of the
    // writes rather than a way to grant a capability.
    if !only_this_user_can_write(dir) {
        log::warn!(
            "not using {}: it is a symlink, or a directory another user on this machine \
             can write. Plugin settings will not survive a restart, and permissions must \
             be granted again.",
            dir.display()
        );
        return None;
    }
    // Creating it is not the whole question. A directory that was *already*
    // there, group- or world-writable, is one another local account may have
    // put an `approvals.json` into before this daemon started — and a
    // `chmod` now only closes the door behind whatever is already inside. So
    // the file is asked about too, and asked after the directory has been
    // shut: what survives is a file this user owns in a directory only this
    // user can write, or nothing.
    let approvals = dir.join(approvals::FILE_NAME);
    if approvals.exists() && !only_this_user_can_write(&approvals) {
        log::warn!(
            "{} could have been written by another user on this machine; every plugin \
             permission will be asked for again",
            approvals.display()
        );
        // Removed rather than merely ignored: leaving it hands the next start
        // the same forged answer, and this daemon has no way to tell that
        // start what it saw.
        if let Err(e) = std::fs::remove_file(&approvals) {
            log::warn!(
                "and {} could not be removed ({e}); running without a state directory",
                approvals.display()
            );
            return None;
        }
        let _ = sync_dir(dir);
    }
    Some(dir)
}

/// Make a directory only this user can enter.
///
/// A plugin's store holds whatever it kept — an autoreply's list of who it
/// has already answered is a list of people — and the approvals beside it say
/// what the machine's owner agreed to. Under the ordinary `022` umask
/// `create_dir_all` would leave both at `0755`, readable by every local
/// account, which is not what "per-user state" means anywhere else in this
/// daemon. Repaired as well as created, because a directory from an earlier
/// version is one somebody already has.
///
/// A link is refused before anything is written rather than after: both calls
/// below follow one, so a link planted where this directory goes had the
/// daemon set the mode of whatever it named, chosen by whoever planted it,
/// and only then find out. Asking first means the refusal costs nothing; the
/// check that the directory is this user's still runs afterwards, because
/// nothing here can close the gap between a question and a `chmod`.
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(dir).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "it is a symlink",
        ));
    }
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Write a file only this user can read.
///
/// The mode is set on *creation* rather than afterwards, so there is no
/// instant in which the contents exist at `0644`. Both stores write through a
/// temporary file and a rename, and a rename carries the mode with it.
///
/// Created *exclusively*, which is the half that is about somebody else. The
/// state directory is made private before it is read, and a `chmod` does not
/// empty it: an entry another local user left there while it was writable
/// survives, and `create(true)` opens it — following a symlink to wherever it
/// points, with the mode ignored because the file already exists. So a
/// planted `approvals.json.<pid>.<thread>.tmp` would have this truncate
/// whatever the daemon's user can write. `create_new` refuses any existing
/// entry, symlink included; the one honest way to meet one is a temporary
/// file from a previous process that shared this pid and thread id and died
/// mid-write, which is why the entry is unlinked once and the create tried
/// again. Unlinked rather than opened, because removing a symlink removes the
/// link and not what it names — and a second refusal is left to fail, since
/// something is racing this directory and losing a preference beats writing
/// through it.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;

    let mut file = match create_private(path) {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            std::fs::remove_file(path)?;
            create_private(path)?
        }
        other => other?,
    };
    file.write_all(bytes)?;
    file.sync_all()
}

/// Create `path`, failing if anything is already there.
fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)
}

/// Make a rename or an unlink in `dir` survive losing power.
///
/// Syncing a temporary file persists its *contents*; the directory entry that
/// gives it its name — or the removal of one — is separate metadata, and POSIX
/// says nothing about when that reaches the disk. So a machine that loses
/// power after a revocation's rename can come back with the previous
/// `approvals.json` and the capability it granted, which is not the narrow
/// window it looks like: nothing bounds how long the entry sits unflushed.
///
/// Fallible, and its callers fail closed on it. Logging and carrying on was
/// the wrong answer for the write this exists to protect: a withdrawal that
/// reported success while the entry was still only in memory is a permission
/// the next start hands back. On anything but unix there is nothing to do and
/// nothing that can fail — a rename there is not a directory entry this
/// process can flush.
pub fn sync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// The id a file carries: `autoreply.wasm` is `autoreply`.
///
/// The filesystem is the registry, which is what "drop it in a folder" means.
/// Restricted to what can appear in a log line, a settings row and a file name
/// without ambiguity — an id is also the stem of the plugin's own settings
/// file, so one containing a separator would name a path of its own choosing.
pub fn plugin_id(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    plugin_id_is_usable(stem).then(|| stem.to_owned())
}

/// Where plugins are looked for, unless the daemon is told otherwise.
///
/// `OXIDEZAP_PLUGIN_DIR` wins, which is what a developer building one uses
/// and what keeps a test from needing a home directory at all.
#[must_use]
pub fn default_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("OXIDEZAP_PLUGIN_DIR") {
        return Some(PathBuf::from(dir));
    }
    data_dir().map(|d| d.join("oxidezap").join("plugins"))
}

/// Where a plugin's own settings and the user's permission answers live.
///
/// Beside the plugins themselves rather than in the daemon's `state_dir`,
/// which on Linux prefers `XDG_RUNTIME_DIR` — a directory documented as
/// cleared on logout. A socket belongs there; a permission answer recorded so
/// it survives a restart, and a plugin's settings defined to outlive the
/// daemon, do not: both would silently disappear on the next login and every
/// prompt would be asked again.
///
/// A sibling of the plugin directory and never inside it: what a plugin may
/// do is not a file a user drops in a folder.
#[must_use]
pub fn default_state_dir() -> Option<PathBuf> {
    data_dir().map(|d| d.join("oxidezap").join("plugin-state"))
}

/// The root under which the plugins and their state live.
///
/// On Windows this is `%LOCALAPPDATA%` and deliberately not `%APPDATA%`,
/// which is the same choice `oxidezap-session` makes for the store — and it
/// has to be the same one. A roaming profile follows the user to another
/// machine, so approvals kept there arrive beside a daemon holding a
/// *different* paired account, where a plugin with the matching id and mask
/// is allowed to act under consent given for an account that is not this
/// one. The plugins travel with it, so the file and the module it names
/// would both be there. Everything in here is account-scoped; none of it may
/// roam.
fn data_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        let not_empty = |v: std::ffi::OsString| (!v.is_empty()).then_some(PathBuf::from(v));
        std::env::var_os("LOCALAPPDATA")
            .and_then(not_empty)
            .or_else(|| {
                std::env::var_os("USERPROFILE")
                    .and_then(not_empty)
                    .map(|profile| profile.join("AppData").join("Local"))
            })
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
    }
}

/// Wait for a reload that is part-way through, having already told it to
/// stop.
///
/// A generation being built is a local until it is installed, so
/// `shutdown` cannot reach it — and on a desktop what happens next is the
/// account's data being deleted, with that set's workers alive and their
/// settings writes still to come. `retired` is raised before this, so the
/// loader abandons the rest of the folder at the next module boundary and
/// retires what it started; this is the wait for that to have happened.
///
/// Polled rather than signalled, deliberately: this runs once in the life
/// of a process, has exactly one waiter, and is bounded by one module's
/// load — a condvar would be more machinery than the thing it waits for.
///
/// And it does not give up. A deadline here was the wrong instinct twice
/// over: `MAX_LOAD_TIME` is checked *between* modules, so it does not
/// bound the open, the validation and the `oxi_init` this is most likely
/// to be waiting on, and returning early hands the wipe a set of workers
/// that are still running — which is the whole thing this exists to
/// prevent. It is also what the code beside it already does: `retire`
/// joins every worker's thread with no deadline either, because a
/// teardown that proceeds without them is worse than one that takes a
/// moment. What bounds it instead is `Reservation`, which gives the slot
/// back however the reload ends, an unwinding loader included.
pub fn wait_for_any_reload(word: &AtomicU32) {
    while word.load(Ordering::SeqCst) != reload::IDLE {
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}
