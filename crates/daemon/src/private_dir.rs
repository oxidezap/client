//! One answer to "is this ours, and ours alone".
//!
//! The daemon writes three things under its own directory that carry the
//! account: the socket, which is control of the session; the media cache,
//! which is a copy of every photo, video and document that has passed through
//! it; and the web bridge's token, which is a bearer credential for both.
//! Under `XDG_RUNTIME_DIR` the parent is already private per user; the
//! `TMPDIR` fallback is not, and neither is a directory an older version left
//! behind at a looser mode. So all of them go through here rather than one
//! being checked carefully and the next being created blindly.
//!
//! Two halves, and they answer the same question at two scales. [`prepare`]
//! and [`drop_foreign_entries`] are about the directory; [`read_private`],
//! [`write_private`] and [`not_ours`] are about one file inside it, which is
//! what [`web_token`] is written through. The token lives here rather than
//! beside the listener that checks it because it is a file in the per-user
//! directory rather than anything about a transport — the bridge reads it and
//! compares it, and creating it is this module's business. See /AGENTS.md on
//! why `listener/` holds transports and nothing else.

use std::path::Path;

use anyhow::{Context, Result};

/// What the directory was when this process found it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Found {
    /// Created now, or already private.
    Private,
    /// Ours, but reachable by somebody else until a moment ago. Tightening
    /// closes the door; it says nothing about what is already inside.
    WasOpen,
}

/// Create `dir` private, or establish that an existing one is safe to use.
///
/// Refuses anything that is not a real directory owned by this user, because
/// the two candidates for what else it could be are a symlink pointing
/// somewhere its author can read and a directory somebody else created at a
/// path they could predict. `symlink_metadata`, not `metadata`: the latter
/// answers for the target and misses exactly that substitution.
///
/// A directory that is ours but too permissive is tightened rather than
/// refused — the common case is an earlier version of this daemon — and the
/// caller is told, because a `chmod` now only closes the door behind whatever
/// is already in the room.
#[cfg(unix)]
pub(crate) fn prepare(dir: &Path, purpose: &str) -> Result<Found> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => return Ok(Found::Private),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("creating {}", dir.display())),
    }

    let meta =
        std::fs::symlink_metadata(dir).with_context(|| format!("inspecting {}", dir.display()))?;
    if !meta.is_dir() {
        anyhow::bail!(
            "{} exists but is not a directory; refusing to keep {purpose} there",
            dir.display()
        );
    }
    if meta.uid() != current_uid() {
        anyhow::bail!(
            "{} is owned by uid {}, not by us; refusing to keep {purpose} there",
            dir.display(),
            meta.uid()
        );
    }

    let mode = meta.permissions().mode() & 0o777;
    if mode == 0o700 {
        return Ok(Found::Private);
    }
    log::warn!("tightening {} from {mode:o} to 700", dir.display());
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("restricting {}", dir.display()))?;
    Ok(Found::WasOpen)
}

/// Windows has no mode to read; what stands in for it is where the directory
/// is, under the profile's own ACL.
#[cfg(not(unix))]
pub(crate) fn prepare(dir: &Path, _purpose: &str) -> Result<Found> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(Found::Private)
}

/// Remove the entries in `dir` that this daemon could not have written.
///
/// For a directory that was reachable by another local account: tightening it
/// says nothing about what was left inside while it was open, and every name
/// under it is predictable — the socket, the lock, and media keys derived
/// from content the account already published.
///
/// Two kinds go. A symlink, because it is never ours under any of those names
/// and following one is how a planted entry becomes a file the daemon writes
/// through. And anything owned by somebody else, which is the other half of
/// the same sentence and the one with teeth: a `daemon.lock` another account
/// holds open is a daemon that never starts, and a `daemon.sock` they are
/// listening on is a bind that never happens and a front end that connects to
/// them instead. Owned by *us* is the test that keeps this off a live sibling
/// daemon's own socket and lock, which are the two entries here worth
/// protecting.
///
/// Reported rather than fatal: a directory that cannot be swept is one whose
/// contents were about to be trusted, and the caller decides which of those
/// is worse.
#[cfg(unix)]
pub(crate) fn drop_foreign_entries(dir: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        // About the link rather than through it, for the reason `prepare`
        // reads it that way: the target's owner says nothing about who may
        // put a different file there.
        let meta = std::fs::symlink_metadata(&path)
            .with_context(|| format!("inspecting {}", path.display()))?;
        let ours = !meta.file_type().is_symlink() && meta.uid() == current_uid();
        if ours {
            continue;
        }
        log::warn!(
            "removing {}, which this daemon did not put there",
            path.display()
        );
        let removed = if meta.file_type().is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        removed.with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn drop_foreign_entries(_dir: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn current_uid() -> u32 {
    rustix::process::getuid().as_raw()
}

/// How long a drawn token is, in characters.
///
/// 24 bytes as lowercase hex. Named rather than spelled twice: the draw and
/// the check below have to agree, and a check that had drifted would either
/// refuse every token or accept a truncated one.
const TOKEN_CHARS: usize = 48;

/// Whether this is a value this daemon could have written.
///
/// The whole of the shape, because the whole of it is what makes the token
/// unguessable: a prefix of one is a shorter secret, not a valid one.
fn is_a_token(value: &str) -> bool {
    value.len() == TOKEN_CHARS
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The bridge's shared secret, created on first use and reused after.
///
/// Reused rather than redrawn per run so a bookmarked URL keeps working
/// across restarts — a token nobody can remember is one that gets turned off.
/// Written into the same per-user directory as the socket and the lock, with
/// no access for anyone else, because the whole point of it is that another
/// account on this machine cannot read it.
///
/// # Errors
///
/// No per-user directory, or the file could not be read or written.
pub fn web_token() -> Result<String> {
    let path = oxidezap_ipc::web_token_path().context("no per-user directory for the web token")?;

    if let Some(existing) = read_private(&path)? {
        let existing = existing.trim();
        if is_a_token(existing) {
            return Ok(existing.to_string());
        }
        if !existing.is_empty() {
            // Ours by owner and mode, and still not a token. A partial write
            // is how that happens — a full disk answers `write_all` with an
            // error after some of the bytes have landed — and what it leaves
            // is a *short* credential, which is the one kind of malformed
            // value that is worse than none: the endpoint's whole admission
            // check is this string, and another local account can reach the
            // port and try. Redrawn rather than refused, because the file is
            // this user's own and a daemon that will not start over a
            // truncated byte is a daemon nobody can recover.
            log::warn!(
                "{} did not hold a well-formed token; drawing a new one",
                path.display()
            );
        }
    }

    // 192 bits, hex. Not a password: nobody types it, it is pasted, so it is
    // sized to be unguessable rather than to be short.
    let mut bytes = [0u8; 24];
    getrandom::fill(&mut bytes).context("no randomness for the web token")?;
    let mut drawn = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(drawn, "{byte:02x}");
    }

    write_private(&path, &drawn)
        .with_context(|| format!("writing the web token to {}", path.display()))?;
    Ok(drawn)
}

/// What is wrong with this file, if anything, in the one sense that matters.
///
/// The three things "we wrote this" means: a regular file, owned by this
/// user, readable by nobody else. One place decides it, because the read side
/// and the write side have to agree — a check that admitted on one and not
/// the other would be a token written into a file the next run refuses, or
/// worse, the reverse.
///
/// Asked of a `Metadata` rather than a path so it can be asked of an open
/// descriptor: what is checked has to be what is used, or the answer is about
/// a file that was there a moment ago.
#[cfg(unix)]
fn not_ours(about: &std::fs::Metadata) -> Option<&'static str> {
    use std::os::unix::fs::MetadataExt as _;

    let us = rustix::process::getuid().as_raw();
    if !about.is_file() {
        Some("not a regular file")
    } else if about.uid() != us {
        Some("owned by another user")
    } else if about.mode() & 0o077 != 0 {
        Some("readable by other users")
    } else {
        None
    }
}

/// Read the token back, refusing anything that is not the file we wrote.
///
/// A bearer credential is only worth the guarantee that nobody else can name
/// it, and following a symlink hands that guarantee to whoever planted the
/// link: the directory is `0700` today, but an installation that predates
/// that — or one whose state directory somebody widened — can already have a
/// `web.token` pointing at a file another account controls. Read through it
/// and the daemon comes up with a bearer token that account knows, which is
/// the whole session.
///
/// So the link is not followed, and the file it would have been is checked
/// for the three things "we wrote this" means: a regular file, owned by this
/// user, readable by nobody else. Anything else is an error rather than a
/// token redrawn over it — writing would follow the same link, which is the
/// worse half of the same problem, and a state directory in that condition is
/// something a person has to look at.
///
/// # Errors
///
/// The path is there and is not ours. Absent is not an error: that is the
/// first run, and the caller draws one.
#[cfg(unix)]
fn read_private(path: &std::path::Path) -> Result<Option<String>> {
    use std::io::Read as _;

    let opened = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    );
    let fd = match opened {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        // `ELOOP` is precisely the case this exists for: something is there
        // and it is a symlink. Named rather than folded into the rest,
        // because the rest are ordinary I/O failures and this one is not.
        Err(rustix::io::Errno::LOOP) => anyhow::bail!(
            "{} is a symbolic link. The web token is a bearer credential and this one would be \
             somebody else's file; remove it and start again.",
            path.display()
        ),
        Err(e) => return Err(anyhow::Error::new(e).context(format!("opening {}", path.display()))),
    };

    let mut file = std::fs::File::from(fd);
    let about = file
        .metadata()
        .with_context(|| format!("reading the type and owner of {}", path.display()))?;
    if let Some(wrong) = not_ours(&about) {
        anyhow::bail!(
            "{} is {wrong}. The web token is a bearer credential for this account's WhatsApp \
             session; remove it and start again.",
            path.display()
        );
    }

    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .with_context(|| format!("reading {}", path.display()))?;
    Ok(Some(contents))
}

/// The same, where the directory is already inside the user's own profile.
///
/// There is no other account to defend against on the path a Windows profile
/// takes, which is the same reason [`write_private`] has nothing to set there.
#[cfg(not(unix))]
fn read_private(path: &std::path::Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::Error::new(e).context(format!("reading {}", path.display()))),
    }
}

/// Create the token file readable by nobody else.
///
/// The mode is set as the file is created rather than after: a token that is
/// briefly world-readable is a token another account had a moment to read.
/// Which is also why the create is exclusive. `create(true)` opens whatever
/// is already at the path, and `mode` is only consulted when a file is made —
/// so a regular file another account planted between [`read_private`]
/// answering "nothing there" and this call is a file we would open, leave at
/// its owner's mode, and write this session's bearer token into. `O_NOFOLLOW`
/// does not cover it: nothing about that is a symlink.
///
/// The path can legitimately exist, though — the empty file a previous run
/// left, which is the case the caller redraws over — so `EEXIST` is not the
/// end. It reopens without creating and asks the *descriptor* the three
/// questions [`not_ours`] asks, which is the only form of the question that
/// cannot be answered about a different file than the one written to.
#[cfg(unix)]
fn write_private(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    // `O_NOFOLLOW` throughout, for the reason [`read_private`] does not
    // follow one either: writing through a planted link would put this
    // account's bearer token into a file somebody else can read, which is
    // worse than reading theirs.
    let nofollow = rustix::fs::OFlags::NOFOLLOW.bits() as i32;
    let made = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(nofollow)
        .open(path);
    let mut file = match made {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Not truncating on the way in: what is there is unexamined until
            // the descriptor answers for itself, and destroying it first
            // would be doing an attacker's file a favour as readily as our
            // own.
            let file = std::fs::OpenOptions::new()
                .write(true)
                .truncate(false)
                .custom_flags(nofollow)
                .open(path)?;
            let about = file.metadata()?;
            if let Some(wrong) = not_ours(&about) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!(
                        "{} is {wrong}. The web token is a bearer credential for this account's \
                         WhatsApp session; remove it and start again.",
                        path.display()
                    ),
                ));
            }
            file.set_len(0)?;
            file
        }
        Err(e) => return Err(e),
    };
    file.write_all(contents.as_bytes())
}

/// The same, where the directory is already inside the user's own profile.
#[cfg(not(unix))]
fn write_private(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    std::fs::write(path, contents)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "oxidezap-private-dir-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// A directory another local account could reach is one they could put a
    /// symlink in, under a name this daemon is about to write through — and
    /// tightening the mode only closes the door behind it.
    #[test]
    fn a_directory_that_was_open_does_not_keep_what_was_left_in_it() {
        let dir = scratch("open");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        let planted = dir.join("media");
        std::os::unix::fs::symlink(std::env::temp_dir(), &planted).unwrap();
        let ours = dir.join("daemon.lock");
        std::fs::write(&ours, b"").unwrap();

        assert_eq!(prepare(&dir, "the socket").unwrap(), Found::WasOpen);
        drop_foreign_entries(&dir).unwrap();

        assert!(!planted.exists() && planted.symlink_metadata().is_err());
        assert!(ours.exists(), "what this daemon writes is left alone");
        // Including the two entries that are not symlinks and stop a daemon
        // dead: a lock somebody else holds, and a socket they are listening
        // on. Ours by uid is what tells them apart, so a live sibling
        // daemon's own files are never the ones removed.
        assert!(dir.join("daemon.lock").exists());
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The substitution the check exists for: `Path::is_dir` follows a link
    /// and answers about the target, which says nothing about who may put a
    /// different file there.
    #[test]
    fn a_symlink_is_not_a_directory_we_may_use() {
        let dir = scratch("link");
        let target = scratch("link-target");
        std::fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, &dir).unwrap();

        assert!(prepare(&dir, "cached media").is_err());
        std::fs::remove_file(&dir).unwrap();
        std::fs::remove_dir_all(&target).unwrap();
    }

    /// The ordinary case stays ordinary: made private, and second time round
    /// nothing is disturbed.
    #[test]
    fn a_private_directory_is_used_as_it_is() {
        let dir = scratch("private");
        assert_eq!(prepare(&dir, "the socket").unwrap(), Found::Private);
        let ours = dir.join("daemon.sock");
        std::fs::write(&ours, b"").unwrap();
        assert_eq!(prepare(&dir, "the socket").unwrap(), Found::Private);
        assert!(ours.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(test)]
mod token_tests {
    use super::*;

    /// What the token file has to be before it is believed.
    ///
    /// The link case is the one with teeth: a state directory that predates
    /// the `0700` rule, or one somebody widened, can already hold a
    /// `web.token` pointing somewhere another account writes — and following
    /// it would hand that account a bearer token for this WhatsApp session.
    #[cfg(unix)]
    #[test]
    fn a_token_file_that_is_not_ours_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join(format!("oxidezap-token-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a directory to test in");

        // Absent is not a failure: it is the first run.
        assert!(
            read_private(&dir.join("missing"))
                .expect("absent is not an error")
                .is_none()
        );

        // Ours, and private: read back verbatim.
        let ours = dir.join("web.token");
        write_private(&ours, "0123456789abcdef").expect("write the token");
        assert_eq!(
            read_private(&ours).expect("ours reads back").as_deref(),
            Some("0123456789abcdef")
        );

        // A link, however inviting what it points at.
        let planted = dir.join("planted");
        std::fs::write(&planted, "attacker-known").expect("the file it points at");
        let link = dir.join("linked.token");
        std::os::unix::fs::symlink(&planted, &link).expect("plant the link");
        assert!(read_private(&link).is_err(), "a symlink must be refused");
        // And the write refuses it too, which is the half that would have
        // leaked our own token rather than adopted theirs.
        assert!(write_private(&link, "ours").is_err());
        assert_eq!(
            std::fs::read_to_string(&planted).expect("read what it points at"),
            "attacker-known",
            "the token was written through the link"
        );

        // Ours, but not private.
        let open = dir.join("open.token");
        write_private(&open, "0123456789abcdef").expect("write the token");
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o644)).expect("widen it");
        assert!(
            read_private(&open).is_err(),
            "a readable token must be refused"
        );

        // A directory is not a token either.
        assert!(read_private(&dir).is_err());

        // The one the exclusive create is for: a regular file another
        // account got to the path first, which is neither a symlink nor
        // anything `read_private` was given a chance to see. Writing into it
        // would hand them this session's bearer token, and `mode` would not
        // have applied — it is only consulted for a file being made.
        let raced = dir.join("raced.token");
        std::fs::write(&raced, "").expect("plant the file");
        std::fs::set_permissions(&raced, std::fs::Permissions::from_mode(0o666))
            .expect("as they would leave it");
        assert!(
            write_private(&raced, "0123456789abcdef").is_err(),
            "the token was written into a file anyone can read"
        );
        assert_eq!(
            std::fs::read_to_string(&raced).expect("read it back"),
            "",
            "the token reached it anyway"
        );

        // And the ordinary reason the path is already there: the empty file a
        // previous run left, which is ours and is redrawn over.
        let empty = dir.join("empty.token");
        write_private(&empty, "").expect("leave an empty one");
        write_private(&empty, "0123456789abcdef").expect("redraw over it");
        assert_eq!(
            read_private(&empty).expect("ours reads back").as_deref(),
            Some("0123456789abcdef")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A token this daemon drew is one it accepts, and a remnant is not.
    ///
    /// The truncation case is the one with teeth: a full disk answers
    /// `write_all` with an error after some of the bytes have landed, and
    /// what is left behind is ours by owner and mode, non-empty, and short —
    /// which the old check read as a perfectly good credential. The endpoint
    /// has nothing else to admit on, so a two-character token is a port any
    /// local account can guess its way into.
    #[test]
    fn only_a_whole_token_is_believed() {
        let mut bytes = [0u8; 24];
        getrandom::fill(&mut bytes).expect("randomness");
        let mut drawn = String::new();
        for byte in bytes {
            use std::fmt::Write as _;
            let _ = write!(drawn, "{byte:02x}");
        }
        assert!(is_a_token(&drawn), "a drawn token must read back as one");

        for wrong in [
            "",
            // A prefix, which is what a partial write leaves.
            &drawn[..2],
            &drawn[..TOKEN_CHARS - 1],
            // Longer than one, which is a file somebody appended to.
            &format!("{drawn}0"),
            // The right length, wrong alphabet: uppercase is not what
            // `{:02x}` writes, and neither is anything else.
            &drawn.to_uppercase(),
            &"g".repeat(TOKEN_CHARS),
        ] {
            assert!(!is_a_token(wrong), "{wrong:?} was taken for a token");
        }
    }
}
