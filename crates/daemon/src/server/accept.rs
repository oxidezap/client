//! The half of the server that needs an operating system.
//!
//! A directory to claim, a lock to hold, a listener to accept on, and the
//! loop that hands each accepted stream to `super::serve_client`. This is
//! the module's gated half, and the gate is on the `mod` line next door
//! rather than on every item in it, which is what it used to be.
//!
//! Nothing about the protocol lives here. The *transport* — how a byte stream
//! is obtained on this platform — is `listener/`'s; what this file adds is the
//! process around it: one daemon per user, an admission cap, and a task per
//! connection.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use oxidezap_ipc::{ProtocolError, endpoint_path, lock_path, media_dir, state_dir};
use tokio::io::{AsyncRead, AsyncWrite};

use super::{ClientSlots, MAX_CLIENTS, error_frame, serve_client, write_line};
use crate::listener::Listener;
use crate::session_bridge::Commands;
use crate::state::StateHub;

/// This process's claim on being *the* daemon for this user.
///
/// Taken before anything touches the account. Holding it is what makes a
/// second daemon fail fast instead of racing the first.
pub struct Claim {
    path: PathBuf,
    _lock: StartupLock,
}

/// Prepare the socket directory and take the per-user lock.
///
/// Separate from [`run`], and called first, for two reasons that both come
/// down to ordering:
///
/// * The directory has to exist, and be verified as ours, *before* the lock
///   file inside it is opened. Opening the lock first fails with `ENOENT` on
///   a first start, and under the `TMPDIR` fallback it would also create a
///   path before the checks that decide whether that path is safe.
/// * The lock has to be held before the session starts. The socket is only
///   the visible half of "one daemon per user"; the real invariant is one
///   WhatsApp session over one SQLite file. A second process that opened the
///   store and connected before discovering the lock was taken would have
///   already broken it.
pub fn claim() -> Result<Claim> {
    let path = endpoint_path().context("no per-user directory to place the endpoint in")?;
    let dir = state_dir().context("no per-user directory for the daemon's own state")?;
    prepare_state_dir(&dir)?;
    let lock = acquire_startup_lock(&lock_path().context("no per-user directory for the lock")?)?;
    // After the lock and not before it, unlike the directory above — that one
    // is where the lock file lives, so it has to exist first. This one is a
    // *live* daemon's cache until the lock says otherwise: preparing it sweeps
    // the writes nobody came back for, and a second process losing the race
    // would otherwise unlink a download the first one is part way through and
    // fail the rename it is about to do.
    prepare_media_dir();
    Ok(Claim { path, _lock: lock })
}

/// Serve until the future is dropped.
///
/// Borrows the claim rather than taking it: this future is a `select!` branch
/// and can be dropped while the session is still disconnecting, and the lock
/// has to outlive that. Handing it over here would release it mid-teardown,
/// which is exactly the window a second daemon must not find open.
pub async fn run(
    claim: &Claim,
    hub: Arc<StateHub>,
    plugins: Arc<oxidezap_plugin_host::Plugins>,
    commands: Commands,
    slots: ClientSlots,
) -> Result<()> {
    let path = claim.path.clone();
    let mut listener = Listener::bind(&path)?;
    log::info!("listening on {}", path.display());

    loop {
        let stream = match listener.accept().await {
            Ok(stream) => stream,
            // Per-connection failures, not listener failures: the peer went
            // away between the SYN and the accept, or the process is briefly
            // out of descriptors. Tearing down the WhatsApp session over one
            // of these would turn a transient condition into an outage, and a
            // supervisor restarting us would land in the same state.
            Err(e) if is_transient_accept_error(&e) => {
                log::warn!("skipping a connection we could not accept: {e}");
                // Without this, an EMFILE that persists spins the loop at
                // full speed; the descriptors it is waiting on are freed by
                // other tasks, which need to be scheduled.
                oxidezap_session::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            }
            Err(e) => return Err(e).context("accepting a client"),
        };

        let Ok(slot) = Arc::clone(&slots).try_acquire_owned() else {
            tokio::spawn(reject(stream));
            continue;
        };

        let hub = Arc::clone(&hub);
        let plugins = Arc::clone(&plugins);
        let commands = commands.clone();
        // Per-connection task: one slow or malformed client cannot hold up
        // the accept loop or any other client.
        tokio::spawn(async move {
            if let Err(e) = serve_client(stream, hub, plugins, commands).await {
                log::debug!("client disconnected: {e}");
            }
            drop(slot);
        });
    }
}

/// Whether an `accept` failure describes one connection rather than the
/// listener.
fn is_transient_accept_error(e: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    matches!(
        e.kind(),
        ErrorKind::ConnectionAborted | ErrorKind::Interrupted | ErrorKind::WouldBlock
    ) || matches!(e.raw_os_error(), Some(EMFILE | ENFILE))
}

/// Out of descriptors, for this process and for the machine. Spelled out
/// because neither has an `std::io::ErrorKind`: both land in
/// `Uncategorized`, which is unstable to match on.
const EMFILE: i32 = 24;
const ENFILE: i32 = 23;

/// The refusal, as a frame, for a transport that has to deliver it itself.
///
/// Public because the web bridge refuses the same way and for the same
/// reason: a refused client should learn why rather than watch its connection
/// drop. The socket listener writes it onto the stream; the web bridge has to
/// complete a WebSocket upgrade first, so it needs the frame rather than the
/// writing.
///
/// # Errors
///
/// The frame could not be serialized.
pub(crate) fn too_many_clients_frame() -> Result<String> {
    error_frame(None, ProtocolError::TooManyClients { limit: MAX_CLIENTS })
}

/// Tell a client we are full, then close.
///
/// Spawned rather than written inline: the accept loop must not wait on a
/// peer. The task is still bounded — one small frame into a socket nobody has
/// had a chance to fill, then done — so a refused client costs a write, not a
/// slot.
pub(super) async fn reject<S: AsyncRead + AsyncWrite + Send + 'static>(stream: S) {
    log::warn!("refusing a client: already serving {MAX_CLIENTS}");
    let (_, mut writer) = tokio::io::split(stream);
    if let Ok(frame) = error_frame(None, ProtocolError::TooManyClients { limit: MAX_CLIENTS }) {
        let _ = write_line(&mut writer, &frame).await;
    }
}

/// An exclusive lock on this user's daemon, released when the file closes.
pub(super) struct StartupLock {
    _file: std::fs::File,
}

/// Take the per-user startup lock, or report who already holds it.
///
/// `flock` rather than a pid file: the kernel releases it when the process
/// dies however it dies, so a crashed daemon leaves nothing to clean up and
/// no stale pid to misread.
#[cfg(unix)]
pub(super) fn acquire_startup_lock(path: &Path) -> Result<StartupLock> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;

    // rustix rather than a hand-rolled `extern "C"`: the same syscall,
    // without an `unsafe` block and without redeclaring `LOCK_EX`/`LOCK_NB`
    // as local constants that nothing checks against the platform.
    if let Err(e) = rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        anyhow::bail!(
            "another daemon holds {} ({e}); refusing to start a second session",
            path.display()
        );
    }

    Ok(StartupLock { _file: file })
}

/// The same exclusion without `flock`, which Windows does not have.
///
/// Opening with no sharing is the platform's own way to say "only me": a
/// second daemon's open fails while the first holds the handle, and the
/// kernel closes it however the first dies — which is the property the lock
/// was chosen for.
#[cfg(windows)]
pub(super) fn acquire_startup_lock(path: &Path) -> Result<StartupLock> {
    use std::os::windows::fs::OpenOptionsExt;

    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .share_mode(0)
        .open(path)
        .map_err(|e| {
            anyhow::anyhow!(
                "another daemon holds {} ({e}); refusing to start a second session",
                path.display()
            )
        })?;
    Ok(StartupLock { _file: file })
}

/// Neither, which is not a platform this daemon runs on today. Spelled out
/// so a new target fails here, with a sentence, rather than at a missing
/// symbol.
#[cfg(not(any(unix, windows)))]
pub(super) fn acquire_startup_lock(_path: &Path) -> Result<StartupLock> {
    anyhow::bail!("no way to take a startup lock on this platform")
}

/// Make the directory the socket lives in ours alone.
///
/// The socket carries control of a WhatsApp session, and under the `TMPDIR`
/// fallback its directory sits at a predictable path in a world-writable
/// place. The check itself is shared with the media cache next door; what is
/// specific here is what a directory that *was* open means: another local
/// account could have left something inside under a name this daemon is about
/// to use — a `daemon.sock` in front of the bind, a `daemon.lock` held open,
/// a `media` symlink pointing at a directory of their own. Refusing to start
/// is a bad outcome; opening the account's photo cache through somebody
/// else's symlink is a worse one, so what could not be ours is removed rather
/// than inherited.
pub(super) fn prepare_state_dir(dir: &Path) -> Result<()> {
    if crate::private_dir::prepare(dir, "the socket")? == crate::private_dir::Found::WasOpen {
        crate::private_dir::drop_foreign_entries(dir)?;
    }
    Ok(())
}

/// And the cache one level down, on the one path that always runs.
///
/// The media directory used to be made by whoever got there first, and only
/// the daemon's own `put` asked whether it was ours: a front end's `stage` and
/// the web bridge's upload both created it with `create_dir_all`, so an
/// account that stages a voice note and never caches a download kept the
/// umask's mode under a directory of predictable names. Making it here means
/// it exists, private, before any of the three can be the one to create it.
///
/// A warning rather than a refusal, unlike the socket's directory above: this
/// is the layer that means nobody else *gets* to create the cache, and every
/// writer still prepares it before it writes — so a daemon that cannot make
/// this one has media that fails and a session that works, which is the better
/// of the two outcomes to hand somebody whose disk is full.
fn prepare_media_dir() {
    let Some(dir) = media_dir() else {
        return;
    };
    if let Err(e) = crate::media::prepare_cache_dir(&dir) {
        log::warn!("the media cache is not usable: {e}");
    }
}
