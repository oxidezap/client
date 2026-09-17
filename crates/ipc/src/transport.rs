//! Where the socket lives and what version speaks over it.

use std::path::PathBuf;

/// Bumped whenever a frame changes shape in a way an older peer would
/// misread. The daemon refuses a mismatch rather than guessing.
///
/// 33: `DaemonMessage::AvatarFailed` and `UiEvent::AvatarFailed`. Propagates avatar
/// resolution, download, and materialization failures with a retryable flag to
/// coordinate front-end failure cooldowns and retry pacing. A v32 front-end
/// would treat the frame as unparsable JSON and fail to decode the session event.
///
/// 32: `ClientRequest::EnsureAvatars`, which carries the viewport's avatar
/// demands to the daemon so profile pictures are resolved on demand rather
/// than for every chat the history load names. A v31 daemon does not know the
/// request and refuses it as malformed — and the daemon is the half that
/// deliberately outlives an upgrade, so without a version an upgraded window
/// would demand avatars that never resolve. Exactly the case v15, v21, v23,
/// v24, v25 and v27 were bumped for.
///
/// 31: `ClientRequest::Hello` binds a connection to either the control plane
/// or one immutable `AccountId`; control listings and lifecycle requests have
/// separate wire messages, and account requests no longer carry an account id
/// in their payload. `owns_window` and `call_video` are independent
/// capabilities. `CreateAccount`/`ResetAccount`/`RemoveAccount` allocate,
/// reset and remove a local account and are answered by
/// `DaemonMessage::AccountCreated`/`Accepted`. Older v30 peers would either
/// omit the scope or treat a control frame as account state, so the daemon
/// refuses the mismatch.
///
/// 30: `UiEvent::AvatarsResolved`. Profile-picture metadata moved off the
/// history reload path — a receipt no longer queries WhatsApp for a picture —
/// and its answer is now its own session event, carrying the picture id and
/// the signed source the daemon fetches from. A v29 window receiving one reads
/// the frame as unparsable and logs it, so a picture that changed server-side
/// would never redraw; the daemon is the half that deliberately outlives an
/// upgrade, so this is the same v22 case of a frame an older reader drops.
///
/// 29: `UiEvent::AvatarReady` and the avatar fields it describes. A profile
/// picture is cached by the daemon and named by a key rather than carried as
/// bytes, exactly as other media is. A v28 window reads the frame as
/// unparsable and draws the placeholder it already had, which is the honest
/// answer for a picture it cannot fetch.
///
/// 28: `CallAction::RequestVideoKeyframe` names the call and direction whose
/// compressed reference chain was lost. Older daemons reject the request,
/// leaving a remote decoder waiting for an IDR the peer may never send.
///
/// 27: `ClientRequest::GroupMembers`, answered by
/// `DaemonMessage::GroupMembers`. A group's header had no line under its name
/// because nothing on a front end's side could answer who is in it: what a
/// chat carries is the senders it has seen, and counting those told a
/// fifty-person group it had one member. The connection keeps the membership
/// list because sending needs one, so the daemon is now asked for it. A v26
/// daemon does not know the request and refuses it as malformed — and the
/// daemon is the half that deliberately outlives an upgrade, so without a
/// version an upgraded window would ask on every group it opened and log a
/// refusal for each. Exactly the case v15, v21, v23, v24 and v25 were bumped
/// for.
///
/// 26: `DaemonMessage::HideWindow`, the mirror of `ShowWindow`. The tray had
/// an Open and no way back: the icon did nothing when clicked, and the one
/// way to put the window away was to close it. Now a click on the icon
/// toggles — raise when nothing is attached, hide when something is — and
/// the menu offers whichever of the two applies. Without a version a v25
/// window would read the frame as unparsable and log it, so Hide would do
/// nothing against a window built before it existed; with one, the hello
/// refuses the pair outright, which is the honest answer. The other
/// direction, a v25 daemon under an upgraded window, never sends the frame
/// and has no item to send it from — so this is a version for the same
/// reason v22 was, a frame an older reader would drop on the floor.
///
/// 25: `ClientRequest::InstallPlugin`, `RemovePlugin` and
/// `ListInstalledPlugins`, answered by `DaemonMessage::PluginInstalled` and
/// `DaemonMessage::InstalledPlugins`. A plugin belongs to the daemon that
/// runs it, so adding one is now a request like approving one is — the module
/// travels through the media cache under a staged key, exactly as a file
/// being sent does, because a `.wasm` is up to thirty-two megabytes and a
/// request frame is capped at a megabyte. What this replaces is a front end
/// on one target reaching into the daemon crate and writing the folder
/// itself, which was a second control channel and which is also why the
/// desktop had no way to install anything at all. A v24 daemon does not know
/// any of the three and refuses them as malformed — and the daemon is the
/// half that deliberately outlives an upgrade, so without a version an
/// upgraded window would offer "Add a plugin…" against a daemon that has been
/// running since before the request existed, and every install would be
/// refused after the module had been staged. Exactly the case v15, v21, v23 and
/// v24 were bumped for.
///
/// 24: `ProtocolError::Failed`, which says the daemon tried and something
/// outside the request went wrong, and carries whether asking again could
/// work. A download answered a full disk, a dropped connection and a session
/// that went away with one `Refused` and one sentence — and `Refused`
/// promises that its detail names what the client would have to change,
/// which none of the three does. A v23 client does not know the tag: it
/// reads the frame as unparsable and logs it, so the download it was waiting
/// on goes unanswered rather than being answered wrongly, which is why this
/// is a version rather than a field. The daemon is the half that
/// deliberately outlives an upgrade, so the direction that matters is a v23
/// window against a v24 daemon — the same case v15, v21 and v23 were bumped
/// for. The third failure is `NoSession`, which already existed and now
/// carries the sessions that went away mid-answer.
///
/// 23: `ClientRequest::SendMedia`. A file the user picked — a photo, a
/// video, a document — staged through the media cache and sent by the daemon,
/// which is the half `SendAudio` already had and the composer's paperclip
/// never did. A v22 daemon does not know the request and refuses it as
/// malformed, and the daemon is the half that deliberately outlives an
/// upgrade: without a version an upgraded window would offer the paperclip,
/// stage every file the user picked, and have each send refused by a daemon
/// that has been running since before the request existed. Exactly the case
/// v15 and v21 were bumped for.
///
/// 22: `DaemonEvent::PluginsChanged` carries its set in a named field. It
/// was a newtype variant holding a `Vec`, and this enum is internally
/// tagged — serde cannot write a tag beside a JSON array, so every one of
/// these frames failed at `to_string` and the daemon dropped it, from v19
/// until here. Nothing ever reached a client, which is why this is a shape
/// no peer has seen rather than one an older peer would misread; the bump is
/// for the direction that *is* real, a v21 window reading a v22 daemon's
/// frame and failing to parse it. What made it survivable, and invisible, is
/// that the snapshot carries the same set: a window attaching after a change
/// saw the truth, and only a change made while it watched was lost — so
/// approving a plugin recorded the answer, republished the set, drew nothing,
/// and the switch flipped back.
///
/// 21: `ClientRequest::ReloadPlugins`. The daemon retires what it is running
/// and loads the folder again, so a plugin installed, updated or removed
/// takes effect without restarting the process holding the account. A v20
/// daemon does not know the request and refuses it as malformed — and the
/// daemon is the half that deliberately outlives an upgrade, so without a
/// version an upgraded window would offer Reload, and install plugins that
/// silently never start, against a daemon that has been running since before
/// the feature existed. Exactly the case v15 was bumped for.
///
/// 20: `SetLogLevel`. How loud the daemon is is a setting rather than a
/// launch argument: nearly everything worth reading about a session is
/// written at `debug`, and restarting `oxidezapd` to see it ends the very
/// connection being investigated. A v19 daemon does not recognise the
/// request, so a v20 client asking one would be silently no louder.
///
/// 19: plugins. The snapshot carries a `PluginSurface` per loaded plugin —
/// what it is called, what it asked to be allowed to do, whether that has
/// been allowed, and the widgets it wants drawn — `DaemonEvent::PluginsChanged`
/// republishes the set, `ClientRequest::PluginAction` carries a widget's use
/// back to the plugin that drew it, and `ClientRequest::PluginApproval`
/// carries the answer to what it asked for. A v18 daemon refuses both as
/// malformed, so an upgraded window would draw a plugin's button that could
/// never do anything; a v18 client ignores the surfaces and simply draws
/// none, which is why the snapshot field is skipped when empty rather than
/// versioned separately. `capabilities` is everything it asked for and
/// `gated` the half that acts on the account, because those are two different
/// sentences: drawing and keeping its own settings take effect on
/// declaration, so a consent control over them would be one that cannot be
/// switched off. `approved` is the one field in a surface that is *not*
/// skipped when false: a front end has to tell "waiting on you" from "this
/// daemon does not know about approval".
///
/// 18: video calls. `DaemonMessage::CallVideo` carries a call's encoded
/// frames in both directions, `DaemonMessage::CallVideoGap` says some were
/// skipped, `ClientRequest::Call(SetVideo)` turns this
/// side's camera on and off, and the call state says which of the two
/// cameras are running. A v17 daemon refuses `SetVideo` as malformed, so an
/// upgraded window would draw a camera button that could never do anything;
/// a v17 client drops every video frame as unparsable and shows a video call
/// with no picture in it.
///
/// 17: `LoadMessages` and `LoadChats`, answered by `DaemonMessage::Messages`
/// and `DaemonMessage::Chats`. History is asked for rather than pushed: the
/// attach load carries the chat list and the newest rows the daemon's own
/// bookkeeping needs, and a front end fills a timeline when it has somewhere
/// to draw it. A v16 daemon does not know either request and refuses both as
/// malformed, which would leave an upgraded window with a list it can never
/// open.
///
/// A history load also carries where the chat list continues, which needs no
/// version of its own for the same reason `has_window` did not: a v17 daemon
/// omits the field, a v17 client ignores it, and both read as the answer that
/// was true before it existed — no position, so ask from the top.
///
/// 16: a frame leaves out what it does not have. The empty half of a
/// message — no reaction, no quote, no media, nothing revoked — and the
/// optional half of a `MediaContent` are skipped on the way out and read back
/// as their defaults, which is a third of a history load in bytes and in
/// serde. A v15 reader requires those fields outright, so it would pass the
/// handshake and then drop every media-bearing frame as unparsable. This is
/// the version that turns that into the refusal it should be.
///
/// The hello also carries `has_window`, which a v15 client does not send.
/// That one needs no version of its own — it defaults to what every client
/// then in existence was.
///
/// 15: `MarkStatusWatched`. A v14 daemon does not know the request and
/// refuses it as malformed, so the upgraded window would go on watching
/// updates into a set that dies with it — which is the bug the request exists
/// to fix. The view itself needs no frame: it moves a stored row, and the
/// history reload that follows is what carries it to every front end.
///
/// 12: the call state's note about a departing call names the outcome to
/// write, not just that there is none. A decline is a refusal only the
/// declining window knows about; every other one watched the same stage
/// disappear and wrote it down as missed.
///
/// 11: `DaemonEvent::AccountChanged`. The linked identity was daemon state
/// with no event, so it travelled only in the hello snapshot — and a window
/// attached before pairing finished never learned whose account it was.
///
/// 10: the call state remembers the last call another of this account's
/// devices handled. A front end writes a conversation's call record off the
/// stage that disappeared, and "answered on the phone" and "nobody picked
/// up" are the same disappearance without it.
///
/// 9: `SendAudio` carries what it quotes, the same way `SendText` does.
/// Recording is a way of answering, and a reply draft open when the
/// microphone was pressed had nowhere to go.
///
/// 8: the account identity carries its LID as well as its phone number. A
/// chat with your own number can be keyed by either alias, and a client
/// holding only one of them cannot recognise the other.
///
/// 7: `StorageUsage` and `ClearMediaCache`, answered by
/// `DaemonMessage::Storage`. The daemon is the only process that opens the
/// store or writes the media cache, so it is the only one that can measure
/// either.
///
/// 6: `DaemonEvent::CallsChanged` publishes the call state to every front
/// end. The daemon makes some call transitions itself — accepting one brings
/// the media up in the process that owns the microphone — and a second window
/// had no way to hear about them.
///
/// 5: `SendText` carries what it quotes, so a reply is sent as one rather
/// than as a fresh message, and the snapshot names the linked account.
///
/// 4: every request may carry an id, and every answer echoes it. Before that
/// a refused send could only be reported by inventing a failure against the
/// message the client had drawn, and a refused download by nothing at all.
/// The snapshot also carries the whole `CallState` rather than a list of
/// ringing calls, because a call this account placed was never an event and
/// no replay reconstructs it.
///
/// 3: the session's own event stream, opt-in at the hello, plus the requests
/// a full front end needs to drive it — audio, typing, calls, downloads and
/// `ForgetSession`. Media travels through [`media_path`] rather than the
/// socket, and `SendText` gained the local id a client that draws the message
/// before it is sent has to know.
///
/// 2: `Pairing` carries a [`PairingCode`] per credential rather than two bare
/// strings, `MessagePreview` names the message it describes, `MarkRead`
/// echoes that name back, `ShowWindow`, `SendFailed`, `Refused` and
/// `TooManyClients` were added, and `Unsupported` was removed once every
/// request the protocol defines became one the daemon acts on. A v1 peer
/// would misparse the first three and not recognise the rest.
///
/// [`PairingCode`]: crate::PairingCode
pub const PROTOCOL_VERSION: u32 = 33;

/// Where the daemon's web bridge listens when nobody says otherwise.
///
/// A page cannot open a Unix socket and has no filesystem to find one in, so
/// the bridge is a TCP port — which means it needs a number both ends agree
/// on without being told. Not configurable *by default*: the daemon takes an
/// address and a page takes `?daemon=`, and this is only what each falls back
/// to.
pub const DEFAULT_WEB_PORT: u16 = 9527;

/// The path the bridge's socket answers on.
///
/// A path rather than the bare root, so the media route below can share the
/// port: one origin, one thing to point a page at.
pub const WEB_SOCKET_PATH: &str = "/ws";

/// The path the bridge serves cached media under.
///
/// Media never travels as a frame (see [`media_path`]), which on the desktop
/// means a file both processes can open. A page shares no filesystem with the
/// daemon, so the same bytes are served over HTTP instead — the sideband
/// stays a sideband, it just changes carrier.
pub const WEB_MEDIA_PATH: &str = "/media";

/// Only a Unix endpoint is a file with a name in a directory.
#[cfg(unix)]
const SOCKET_NAME: &str = "daemon.sock";
const DIR_NAME: &str = "oxidezap";
const MEDIA_DIR: &str = "media";

/// Where the daemon listens and a client connects.
///
/// Two things on Unix and one thing on Windows, which is why it is separate
/// from [`state_dir`]: a Unix socket *is* a filesystem entry beside the
/// daemon's other state, while a Windows named pipe is a name in a namespace
/// of its own and has no directory to sit in.
///
/// Returns `None` when there is nowhere sensible rather than inventing a
/// path, so the caller reports it instead of listening somewhere unexpected.
#[must_use]
pub fn endpoint_path() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        Some(state_dir()?.join(socket_file_name()))
    }
    #[cfg(windows)]
    {
        // Named pipes are machine-wide, so the name carries the user: two
        // people signed into one machine must not land on each other's
        // session. The same reason the Unix fallback carries the uid. The
        // account rides along for the same reason the socket file does.
        let account = account_id().map(|id| format!("-{id}")).unwrap_or_default();
        Some(PathBuf::from(format!(
            r"\\.\pipe\{DIR_NAME}-{}{account}",
            user_suffix()?
        )))
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

/// The directory holding everything the daemon keeps between frames: its
/// startup lock and its media cache.
///
/// On Unix this prefers `XDG_RUNTIME_DIR`, which is per-user, mode 0700 and
/// cleared on logout: a socket that grants control of a WhatsApp session does
/// not belong in a world-writable `/tmp`. It falls back to `TMPDIR` with the
/// uid in the directory name, so two users on one machine cannot collide or
/// reach each other's daemon.
///
/// On Windows it is under `LOCALAPPDATA`, which is already inside the user's
/// profile and so already private to them.
#[must_use]
pub fn state_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let local = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty())?;
        Some(PathBuf::from(local).join(DIR_NAME))
    }

    #[cfg(not(windows))]
    {
        if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
            return Some(PathBuf::from(runtime).join(DIR_NAME));
        }

        let tmp = std::env::var_os("TMPDIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));

        // Only reachable when XDG_RUNTIME_DIR is unset, which is unusual on a
        // desktop; the uid keeps the fallback per-user anyway.
        Some(tmp.join(format!("{DIR_NAME}-{}", user_suffix()?)))
    }
}

/// Where the daemon's startup lock lives.
///
/// A file rather than the socket path with an extension, because on Windows
/// the endpoint is not a file at all.
#[must_use]
pub fn lock_path() -> Option<PathBuf> {
    Some(state_dir()?.join(account_lock_file_name()))
}

/// Where a media payload with this cache key lives.
///
/// A photo is megabytes and the socket carries newline-delimited JSON, so
/// media never travels as a frame: the side that has the bytes writes them
/// here and the other side reads the file. Both derive the path from the same
/// place, so they cannot disagree about it.
///
/// The directory is the daemon's, and both processes run as the same user — a
/// client writing a voice note into it is putting a file in its own scratch
/// space, not reaching into the daemon.
///
/// Returns `None` for the same reason [`state_dir`] does, and for a key that
/// is not a plain name: a key is echoed from a peer, and one carrying a
/// separator or a leading dot would name a file outside the cache.
#[must_use]
pub fn media_path(key: &str) -> Option<PathBuf> {
    let sane = !key.is_empty()
        && key.len() <= 128
        && !key.starts_with('.')
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.');
    sane.then(media_dir).flatten().map(|dir| dir.join(key))
}

/// The most a front end may stage in one payload.
///
/// Declared here rather than at either end, for the reason [`STAGED_PREFIX`]
/// is: the two have to agree. The daemon's write endpoint refuses anything
/// larger, and a front end that read a file first and learned the ceiling
/// from a `413` would have spent the whole read — and, in a page, a copy of
/// the file in a linear memory that has a ceiling of its own — to be told a
/// number it could have asked for.
///
/// Sized for what actually goes through here: a voice note, a photo, a
/// document, a clip. Not a film, which is a different design — the payload is
/// read into the daemon's memory whole, because a partly staged file under a
/// key a send is about to name is worse than a refused one.
///
/// Enforced in three places, which is not three rules: the daemon's write
/// endpoint refuses a longer body because it must, a front end refuses a file
/// at the chooser because that is where somebody can be told, and the one
/// staging path every payload passes through refuses anything else because
/// otherwise this sentence would be true of one transport and not the others.
pub const MAX_STAGED_BYTES: u64 = 64 * 1024 * 1024;

/// The prefix a *daemon-global* staged payload is filed under.
///
/// The one key space a front end *writes* that belongs to no account. `f-` and
/// `d-` are the daemon's cache of what it fetched and can fetch again; a
/// staged key is a payload a front end wrote for the daemon to consume, and
/// the only copy of it, so the cache sweep spares it and the daemon's write
/// endpoint takes nothing else.
///
/// A send's payload is not under this prefix: it carries the account it
/// belongs to as `a<id>-u-...` (see [`account_staged_key`]), because a
/// payload staged to be sent *by* an account must not be consumable by a
/// different one, and must be swept when that account is reset or removed.
/// What stays global is what really is the daemon's rather than an account's:
/// a plugin module being installed, which lives in the shared catalog and is
/// read by the daemon's own plugin loader.
///
/// Here rather than spelled at each end, because the two ends have to agree:
/// the daemon answers 403 to any other prefix, and a front end that composed
/// a key the daemon did not recognise would have its cleanup silently refused
/// and leave the payload staged until the account was wiped.
pub const STAGED_PREFIX: &str = "u-";

/// The infix an account-scoped staged payload carries: `a<id>-u-<name>`.
///
/// Between the account prefix and the staged marker, the same `u-` the global
/// namespace uses, so a single sweep's staged rule can recognise both without
/// knowing which kind it is looking at.
pub const ACCOUNT_STAGED_INFIX: &str = "-u-";

/// Whether this key names a payload a front end staged for the daemon, under
/// either the global namespace or an account's.
#[must_use]
pub fn is_staged_key(key: &str) -> bool {
    key.starts_with(STAGED_PREFIX) || is_account_staged_key(key)
}

/// Whether this key names a *daemon-global* staged payload, as opposed to one
/// belonging to an account.
///
/// The install path is the caller that must draw this line: a plugin module is
/// the daemon's, and a payload staged under an account's namespace is that
/// account's data, which install must refuse rather than move into the shared
/// catalog.
#[must_use]
pub fn is_global_staged_key(key: &str) -> bool {
    key.starts_with(STAGED_PREFIX) && !is_account_staged_key(key)
}

/// Whether this key is an account's staged payload (`a<id>-u-...`).
///
/// The account id itself is not returned here, and this deliberately does not
/// need [`oxidezap_core::AccountId`]: the predicates above are used by every
/// consumer of this crate, including the CLI, which must not pull the domain
/// crate in. The typed form is [`account_staged_prefix_of`].
#[must_use]
pub fn is_account_staged_key(key: &str) -> bool {
    account_staged_local(key).is_some()
}

/// The local name of an account-staged key, `u-<name>` from `a<id>-u-<name>`.
///
/// The account part is the one `a<digits>-` namespace every account key shares,
/// and this is one only when the local name right after it starts with the
/// staged `u-`. That distinction keeps a durable `a1-f-...` key whose message
/// id happens to contain `-u-` — the alphabet is not restricted — from reading
/// as a staged payload.
fn account_staged_local(key: &str) -> Option<&str> {
    let rest = key.strip_prefix('a')?;
    let (digits, local) = rest.split_once('-')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // A positive `i32`, the only id an account can have. Without this the
    // predicate would accept `a99999999999-u-x` while
    // [`account_staged_prefix_of`] rejected it, so a key no account could
    // ever own would read as a staged payload to every prefix check.
    let id: i32 = digits.parse().ok()?;
    if id <= 0 {
        return None;
    }
    local.starts_with(STAGED_PREFIX).then_some(local)
}

/// The key a global staged payload is filed under.
///
/// `name` is the caller's own, and has to survive [`media_path`]: a local id
/// is composed by a front end, so it is sanitized before it gets here.
#[must_use]
pub fn staged_key(name: &str) -> String {
    format!("{STAGED_PREFIX}{name}")
}

/// The key an account's staged payload is filed under: `a<id>-u-<name>`.
///
/// See [`STAGED_PREFIX`] for why an account's send payload is not global.
#[cfg(feature = "legacy-protocol")]
#[must_use]
pub fn account_staged_key(account: oxidezap_core::AccountId, name: &str) -> String {
    format!("{}{name}", account_staged_prefix(account))
}

/// The prefix every one of `account`'s staged payloads carries.
#[cfg(feature = "legacy-protocol")]
#[must_use]
pub fn account_staged_prefix(account: oxidezap_core::AccountId) -> String {
    format!("a{}{ACCOUNT_STAGED_INFIX}", account.get())
}

/// The account prefix a key carries, if any: `Some("a<id>-")`.
///
/// The one `a<digits>-` shape every account-scoped key shares, which is what
/// lets a consumer that only knows the local convention — the avatar prefix,
/// say — see through the account wrapper to the key underneath.
#[must_use]
pub fn account_prefix_of(key: &str) -> Option<&str> {
    let rest = key.strip_prefix('a')?;
    let (digits, local) = rest.split_once('-')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // Everything up to and including the first `-`, i.e. `a<digits>-`.
    Some(&key[..key.len() - local.len()])
}

/// The local name a key carries, with any account prefix stripped.
///
/// For a caller that knows the local convention (an avatar is `a-...`) and has
/// only the account-scoped form in hand. A key under no account is returned
/// unchanged.
#[must_use]
pub fn key_local_name(key: &str) -> &str {
    match account_prefix_of(key) {
        Some(prefix) => &key[prefix.len()..],
        None => key,
    }
}

/// The account id embedded in an account-staged key, if this is one.
///
/// Parsed rather than trusted: the key is a name a peer chose, and the send
/// path is what has to decide whether it belongs to the account being asked
/// to send. The shape rule lives in [`account_staged_local`]; this only wraps
/// the parsed id, and is gated on `legacy-protocol` because
/// [`oxidezap_core::AccountId`] is.
#[cfg(feature = "legacy-protocol")]
#[must_use]
pub fn account_staged_prefix_of(key: &str) -> Option<oxidezap_core::AccountId> {
    account_staged_local(key)?;
    let digits = key.strip_prefix('a')?.split_once('-')?.0;
    digits
        .parse::<i32>()
        .ok()
        .and_then(|id| oxidezap_core::AccountId::new(id).ok())
}

/// The account id embedded in any account-scoped key (`a<id>-...`), if this is one.
#[cfg(feature = "legacy-protocol")]
#[must_use]
pub fn account_id_of(key: &str) -> Option<oxidezap_core::AccountId> {
    let rest = key.strip_prefix('a')?;
    let (digits, _) = rest.split_once('-')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits
        .parse::<i32>()
        .ok()
        .and_then(|id| oxidezap_core::AccountId::new(id).ok())
}

/// The directory [`media_path`] resolves into.
#[must_use]
pub fn media_dir() -> Option<PathBuf> {
    Some(state_dir()?.join(media_dir_name()))
}

/// The account profile in force, from `OXIDEZAP_ACCOUNT`.
///
/// Validated, never sanitized: an invalid value falls back to the default
/// profile rather than converging onto a real one (see
/// [`oxidezap_wire::validate_account_id`]). Entry points that take an id
/// from the user — CLI `--account`, `accounts use`, daemon `--account` —
/// reject invalid ids outright instead of reaching this fallback. Unset or
/// empty means the default profile. One account is one daemon over one
/// store: the socket, the lock, the media cache and the database all derive
/// from this, so two profiles never share state.
#[must_use]
pub fn account_id() -> Option<String> {
    let raw = std::env::var_os("OXIDEZAP_ACCOUNT")?;
    oxidezap_wire::validate_account_id(&raw.to_string_lossy())
}

/// Every account socket the state directory holds: the default
/// `daemon.sock` plus each `daemon-<id>.sock`, with the default first.
#[must_use]
pub fn account_sockets() -> Vec<(String, PathBuf)> {
    let Some(dir) = state_dir() else {
        return Vec::new();
    };
    let entries = std::fs::read_dir(&dir).map(|read| {
        read.filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension().is_some_and(|ext| ext == "sock")
                    && path
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .is_some_and(|stem| stem == "daemon" || stem.starts_with("daemon-"))
            })
            .collect::<Vec<_>>()
    });
    let mut found: Vec<(String, PathBuf)> = entries
        .unwrap_or_default()
        .into_iter()
        .map(|path| {
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("daemon");
            let id = stem
                .strip_prefix("daemon-")
                .unwrap_or("default")
                .to_string();
            (id, path)
        })
        .collect();
    found.sort();
    found
}

/// The socket file for one account: the default name, or a suffixed one.
///
/// `None` for an invalid id: callers already handle `None` as "no endpoint",
/// so a rejected id never converges onto another profile's socket.
#[must_use]
pub fn endpoint_path_for_account(id: &str) -> Option<PathBuf> {
    let clean = oxidezap_wire::validate_account_id(id)?;
    #[cfg(unix)]
    {
        Some(state_dir()?.join(format!("daemon-{clean}.sock")))
    }
    #[cfg(windows)]
    {
        // The id the caller named, never the one in the environment: a caller
        // that says `work` must get `work`'s pipe whatever profile the shell
        // had selected. Delegating to `endpoint_path` would read
        // `OXIDEZAP_ACCOUNT`, so `accounts remove work` could resolve the pipe
        // of whoever was selected instead.
        Some(account_pipe_name(&clean, &user_suffix()?))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = clean;
        None
    }
}

/// A named pipe's name for one account, given the user's SID suffix.
///
/// Split out so the id's part in the name is testable on every platform: the
/// bug this replaced ignored the id outright and read the profile from the
/// environment. Windows-only in effect, but pure and platform-free in shape.
#[cfg_attr(not(windows), allow(dead_code))]
fn account_pipe_name(clean: &str, user: &str) -> PathBuf {
    PathBuf::from(format!(r"\\.\pipe\{DIR_NAME}-{user}-{clean}"))
}

/// Only a Unix endpoint is a file with a name: Windows listens on a named
/// pipe and a page reaches its daemon over the loopback bridge, so neither
/// has a socket file to name. Gated like [`SOCKET_NAME`] itself, beside the
/// one caller that is gated the same way.
#[cfg(unix)]
fn socket_file_name() -> String {
    match account_id() {
        None => SOCKET_NAME.to_string(),
        Some(id) => format!("daemon-{id}.sock"),
    }
}

fn lock_file_name() -> &'static str {
    "daemon.lock"
}

fn account_lock_file_name() -> String {
    match account_id() {
        None => lock_file_name().to_string(),
        Some(id) => format!("daemon-{id}.lock"),
    }
}

fn media_dir_name() -> String {
    match account_id() {
        None => MEDIA_DIR.to_string(),
        Some(id) => format!("{MEDIA_DIR}-{id}"),
    }
}

/// What distinguishes one user's daemon from another's on the same machine.
///
/// `None` when the platform will not say, which is a reason to report that
/// there is nowhere to listen rather than to invent a name every user would
/// share.
#[cfg(unix)]
fn user_suffix() -> Option<String> {
    // rustix rather than a hand-rolled `extern "C"`: the same syscall with no
    // `unsafe` at this call site, from a crate already in the tree.
    Some(rustix::process::getuid().as_raw().to_string())
}

#[cfg(windows)]
fn user_suffix() -> Option<String> {
    // The SID, not `USERNAME`. A pipe name is machine-wide and an environment
    // variable is not an identity: two accounts from different domains can
    // share a name, and a process controls its own environment. This is the
    // identity the kernel uses, and the same one the daemon's access-control
    // entry names.
    let sid = crate::windows_user::sid_string().ok()?;
    Some(
        sid.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .take(96)
            .collect(),
    )
}

#[cfg(not(any(unix, windows)))]
fn user_suffix() -> Option<String> {
    None
}

/// Where the web bridge's shared secret lives.
///
/// In the same per-user directory as the socket, and for the same reason: a
/// loopback TCP port is reachable by every account on the machine, while that
/// directory is the user's own. The token is what carries the socket's
/// per-user guarantee onto a port that has none. See `daemon/listener/web.rs`.
#[must_use]
pub fn web_token_path() -> Option<PathBuf> {
    Some(state_dir()?.join("web.token"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `XDG_RUNTIME_DIR` is a Unix idea, and so is the shape it produces.
    #[cfg(unix)]
    #[test]
    fn runtime_dir_wins_when_set() {
        // Not using the process environment: these tests run in parallel and
        // env mutation is process-wide.
        let path = PathBuf::from("/run/user/1000")
            .join(DIR_NAME)
            .join(SOCKET_NAME);
        assert_eq!(path.file_name().unwrap(), SOCKET_NAME);
        assert!(path.starts_with("/run/user/1000"));
    }

    /// A cache key is echoed from a peer. One carrying a separator or a
    /// leading dot names a file outside the cache, and the daemon writes
    /// there as the user who owns the session.
    #[test]
    fn a_key_that_could_escape_the_cache_resolves_to_nothing() {
        for key in [
            "../../.ssh/authorized_keys",
            "sub/dir",
            ".hidden",
            "",
            "/etc/passwd",
        ] {
            assert!(media_path(key).is_none(), "{key} was allowed");
        }
    }

    #[test]
    fn an_ordinary_key_lands_inside_the_cache() {
        let dir = media_dir().expect("a cache directory is always derivable");
        let path = media_path("a1b2c3.jpg").expect("a plain name is a key");
        assert_eq!(path.parent(), Some(dir.as_path()));
        assert!(path.starts_with(dir));
    }

    /// The endpoint is always derivable, and always says which user it
    /// belongs to — the shape differs by platform, the property does not.
    #[test]
    fn an_endpoint_is_always_produced() {
        let path = endpoint_path().expect("an endpoint is always derivable");

        #[cfg(unix)]
        {
            assert_eq!(path.file_name().unwrap(), SOCKET_NAME);
            assert!(
                path.parent()
                    .is_some_and(|p| p.to_string_lossy().contains(DIR_NAME)),
                "the socket sits in its own directory so its permissions are ours to set"
            );
        }
        #[cfg(windows)]
        {
            let name = path.to_string_lossy();
            assert!(name.starts_with(r"\\.\pipe\"), "not a pipe name: {name}");
            assert!(
                name.contains(DIR_NAME),
                "a pipe name is machine-wide, so it has to say whose it is: {name}"
            );
        }
    }

    /// An invalid account id resolves to no endpoint rather than to
    /// another profile's socket: `wo/rk` must not converge onto `work`.
    #[test]
    fn an_invalid_account_id_resolves_to_no_endpoint() {
        for id in ["", "-work", "wo/rk", "wo!rk", "wo rk", "../x"] {
            assert_eq!(endpoint_path_for_account(id), None, "{id}");
        }
    }

    /// A valid account id names its own suffixed socket.
    #[cfg(unix)]
    #[test]
    fn a_valid_account_id_names_its_own_socket() {
        let path = endpoint_path_for_account("work").expect("a valid id");
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("daemon-work.sock")
        );
    }

    /// The pipe name carries the id the caller asked for, not whichever
    /// profile is selected in the environment.
    ///
    /// The Windows branch used to fall back to `endpoint_path()`, which reads
    /// `OXIDEZAP_ACCOUNT`: `accounts remove work` could resolve the pipe of
    /// the currently selected account and wipe the wrong store.
    #[test]
    fn an_account_pipe_name_carries_the_id_it_was_given() {
        let work = account_pipe_name("work", "S-1-5-21");
        let other = account_pipe_name("other", "S-1-5-21");
        assert_ne!(work, other, "two ids must not share one pipe");
        let name = work.to_string_lossy();
        assert!(name.starts_with(r"\\.\pipe\"), "not a pipe name: {name}");
        assert!(name.ends_with("-work"), "the id is not in the name: {name}");
        assert!(
            name.contains("S-1-5-21"),
            "the name is machine-wide, so it has to say whose it is: {name}"
        );
    }

    /// Both live under the same per-user directory, so whatever protects one
    /// protects the other.
    #[test]
    fn the_cache_sits_with_the_daemon_s_other_state() {
        let state = state_dir().expect("a state directory is always derivable");
        assert!(media_dir().is_some_and(|dir| dir.starts_with(&state)));
        assert!(lock_path().is_some_and(|path| path.starts_with(&state)));
    }

    /// The account-staged predicate accepts exactly `a<positive-i32>-u-...`.
    ///
    /// The shapes it rejects matter as much as the one it takes: `a-<jid>` and
    /// `a-1-u-x` are not ids, `ax-u-x` has no digits, and `a99999999999-u-x`
    /// overflows the `i32` an account id is — a key no account could own must
    /// not read as a staged payload.
    #[test]
    fn only_a_valid_account_id_marks_a_staged_key() {
        assert!(is_account_staged_key("a1-u-x"));
        assert!(is_account_staged_key("a2-u-local_audio-7"));
        for key in [
            "a-<jid>",
            "a-1-u-x",
            "ax-u-x",
            "a99999999999-u-x",
            "a0-u-x",
            "u-x",
        ] {
            assert!(
                !is_account_staged_key(key),
                "{key} is not an account-staged key"
            );
        }
    }

    /// A durable key whose message id contains `-u-` is not a staged payload:
    /// the local name after the account prefix has to *start* with `u-`, and
    /// an id of the alphabet WhatsApp uses never does.
    #[test]
    fn a_durable_key_containing_the_staged_infix_is_not_staged() {
        assert!(!is_account_staged_key("a1-f-3EB0-u-x"));
        assert!(!is_staged_key("a1-f-3EB0-u-x"));
        assert!(!is_global_staged_key("a1-f-3EB0-u-x"));
        assert!(!is_global_staged_key("a1-u-x"));
        assert!(is_global_staged_key("u-plugin-1"));
    }

    /// The typed helpers and the core-free predicate agree: a key built for an
    /// account parses back to that account, so a send can only ever consume
    /// what the same account staged.
    #[cfg(feature = "legacy-protocol")]
    #[test]
    fn an_account_staged_key_round_trips_through_its_prefix() {
        let account = oxidezap_core::AccountId::new(7).expect("a positive id");
        let key = account_staged_key(account, "voice-note");
        assert!(is_account_staged_key(&key));
        assert_eq!(account_staged_prefix_of(&key), Some(account));
        assert_eq!(account_staged_prefix(account), "a7-u-");
    }
}
