//! The daemon, from this side of the socket.
//!
//! Deliberately shaped like the `WhatsAppClient` it replaces: same method
//! names, same arguments, same fire-and-forget for everything whose outcome
//! arrives as an event. The front end above it does not know which side of a
//! socket the session is on, which is why moving it there changed almost
//! nothing in the app.
//!
//! No async runtime where there is a thread to spare: one thread reads frames
//! and one lock serializes writes, which is all a newline-delimited protocol
//! over a local socket needs. The runtime the old client owned existed for the
//! network, and the network is now somebody else's problem.
//!
//! # Two transports, one protocol
//!
//! A page has no socket to open and no thread to park, so it reaches the same
//! daemon over a WebSocket and reads it on a task. That difference is confined
//! to [`native`] and [`web`], which are a thread and a callback around the same
//! four things: [`frames`], which is what a frame *means*; [`sink`], which is
//! where events go; [`media`], which is where the bytes a frame only names come
//! from; and [`attach`], which is the hello every transport says and the loop
//! the ones that read on a task share. Everything a caller of this module
//! touches — every method on [`Session`] — is written once and never learns
//! which side it is on.

mod attach;
#[cfg(target_family = "wasm")]
mod embedded;
mod frames;
mod media;
#[cfg(not(target_family = "wasm"))]
mod native;
mod sink;
#[cfg(target_family = "wasm")]
mod tab;
#[cfg(target_family = "wasm")]
mod web;

/// Whether the session this window talks to is in this tab.
///
/// One bit, and it is the front end's only piece of "which arrangement am I
/// in" — the interface never asks it, and the one thing that does is the
/// plugin pane: a folder is one per origin, a host is one per account, and a
/// window with no session of its own can write to the first and cannot start
/// anything in the second. See `platform::plugins::home`.
///
/// An atomic rather than a `thread_local`, because it is read from the render
/// pass and written from whichever task settled the connection, and false
/// until something says otherwise: a window that has not attached to anything
/// yet holds no account, which is the answer that withholds rather than the
/// one that promises.
///
/// Web-only, and so are the two functions below, because the question is:
/// a desktop front end reaches a daemon in another *process* and never holds
/// a session whatever happens, so there is nothing here for it to ask. Left
/// ungated, all three are dead code on that target — which `-D warnings`
/// rightly refuses, and which a check of the wasm target alone cannot see.
#[cfg(target_family = "wasm")]
static HOLDS_THE_ACCOUNT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether this tab is the one running the session.
#[cfg(target_family = "wasm")]
#[must_use]
pub fn this_tab_holds_the_account() -> bool {
    HOLDS_THE_ACCOUNT.load(std::sync::atomic::Ordering::Relaxed)
}

/// Say which arrangement this window ended up in.
///
/// Called by each of the ways a connection is made, at the moment it is made
/// — not before, because a tab that is still asking has not settled anything.
#[cfg(target_family = "wasm")]
pub(crate) fn note_account_is_here(here: bool) {
    HOLDS_THE_ACCOUNT.store(here, std::sync::atomic::Ordering::Relaxed);
}

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use log::error;
use oxidezap_core::{CallState, Chat, ChatMessage, DownloadableMedia, QuotedMessage, UiEvent};
use oxidezap_ipc::{CallAction, ClientRequest, Link, PageCursor, Request, RequestId};
// The payload structs the protocol declares, named rather than glob-imported
// so `Typing` and `Download` read at the call site as what they are: the
// request's own payload, built here and moved onto the wire unchanged.
use oxidezap_ipc::{
    Download, LoadChats, LoadMessages, MarkRead, MarkStatusWatched, SendAudio, SendMedia, SendText,
    Typing,
};
use portable_atomic::AtomicU64;
use tokio::sync::oneshot;

use self::media::MediaCache;
use self::sink::EventSink;
pub use self::sink::Events;

/// What this account occupies on disk, as the daemon measured it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StorageUsage {
    pub database_bytes: u64,
    pub media_bytes: u64,
    pub media_files: u64,
}

/// A file on its way out, as this side knows it.
///
/// What the picker read plus the two things the composer decides: what it is
/// being sent *as*, and the line typed beside it. Everything else about the
/// file — how big the picture is, what its thumbnail looks like — is worked
/// out where the bytes end up, which is the daemon.
pub struct Attachment {
    pub bytes: Vec<u8>,
    pub kind: oxidezap_core::OutgoingMedia,
    pub mime_type: String,
    pub file_name: String,
    pub caption: Option<String>,
}

/// What the reader hands the front end.
///
/// Wider than `UiEvent`, and deliberately not part of it: the session says
/// what happened to the account, while a window request and the call state a
/// client attached to are things the *daemon* says to a front end. Keeping
/// them apart is what stops the session's vocabulary from growing terms only
/// one transport uses.
pub use oxidezap_core::Fault;

pub enum FromDaemon {
    /// Something the session said.
    Session(Box<UiEvent>),
    /// The calls in progress at the moment this client attached.
    ///
    /// State rather than a replay: a call this account placed was never an
    /// event — the front end that dialled built it locally — so there is
    /// nothing to replay it from.
    Calls(Box<CallState>),
    /// Who this device is linked as, at the moment this client attached.
    Account(Option<oxidezap_ipc::AccountIdentity>),
    /// Every plugin the daemon has loaded, and what each wants drawn.
    ///
    /// State, and whole every time: a plugin published its interface when it
    /// started and nothing replays that, so a window attaching later has only
    /// the set the daemon holds. Republished in full whenever any of them
    /// changes, which is what spares this side from merging deltas.
    Plugins(Vec<oxidezap_core::PluginSurface>),
    /// Somebody asked for a front end to come forward.
    ShowWindow,
    /// This connection is over, and why — in terms the screen can draw
    /// rather than one line of prose for three different endings.
    Ended(Fault),
    /// One page of a conversation, for the timeline that asked for it.
    ///
    /// Older rows than the window holds, or the first ones it has: which of
    /// the two is the *window's* business, because the cursor it asked with
    /// is the position it is filling in.
    Messages {
        jid: String,
        messages: Vec<ChatMessage>,
        /// Where to continue, or `None` at the start of the conversation.
        next: Option<PageCursor>,
    },
    /// One page of the chat list, for the list that asked for it.
    Chats {
        chats: Vec<Chat>,
        /// Where to continue, or `None` at the end of the list.
        next: Option<PageCursor>,
    },
    /// A page that never arrived, so whoever is waiting can stop waiting.
    ///
    /// Named by what was asked for rather than carrying an error: a timeline
    /// that keeps a request in flight forever is one that never asks again,
    /// which is worse than an empty page.
    PageLost {
        /// The conversation, or `None` for a page of the chat list.
        jid: Option<String>,
    },
    /// A picture is waiting in [`Session::call_frames`].
    ///
    /// A nudge rather than the frame itself: pictures are held in a slot per
    /// direction, where the newest replaces the last, and this channel is
    /// deep enough that carrying them would let a stalled window accumulate
    /// gigabytes of obsolete video ahead of the state frames behind it.
    CallFrames,
    /// A status view the daemon did not record after all.
    ///
    /// The ring was taken down the moment the update was opened, before the
    /// daemon had answered — which is right, because a view that has to wait
    /// for a round trip flickers. When the answer is a refusal, the honest
    /// thing is to put it back: nothing durable exists, and the next restart
    /// would bring the ring back anyway, with nothing having said why.
    StatusViewLost(Vec<String>),
}

/// Why something a front end asked for did not happen.
///
/// `detail` is the sentence somebody reads. `retryable` is the bit no
/// sentence carries, and the only one a caller can act on: whether asking the
/// same thing again could work at all. Nothing on this side can work it out —
/// a full disk and a dropped connection are both "the download failed" from
/// here — so the daemon says which, through
/// [`ProtocolError::Failed`](oxidezap_ipc::ProtocolError::Failed), and this
/// is where that lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub detail: String,
    /// Whether the same request, sent again, could succeed.
    pub retryable: bool,
}

impl Failure {
    /// Something that could work if it were asked again — a dropped
    /// connection, a network that was busy.
    pub fn worth_retrying(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
            retryable: true,
        }
    }

    /// Something that would fail the same way every time it was asked.
    pub fn permanent(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
            retryable: false,
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl From<&oxidezap_ipc::ProtocolError> for Failure {
    /// What the daemon said, plus what a front end may do about it.
    ///
    /// Only one of these variants carries the answer; the rest are read off
    /// what the variant *means*. A refusal is about the request, so the same
    /// request is refused the same way; a malformed frame and a version
    /// mismatch are about this build, which asking twice does not change. The
    /// account being unreachable is the one that reads the other way round
    /// from how it sounds: it is a state that ends — the app is already
    /// reconnecting — and the download the person asked for works on the
    /// next tap, so telling them it never will is the worse of the two
    /// wrong answers. Too many clients clears the same way.
    ///
    /// The daemon's own `detail` rather than the whole `Display`, where there
    /// is one: `thiserror` writes the variant in front of it, and a notice
    /// built from that reads "Could not download that image: failed:
    /// connection reset by peer". The prefix is for a log line, which is
    /// where the full error still goes.
    fn from(error: &oxidezap_ipc::ProtocolError) -> Self {
        use oxidezap_ipc::ProtocolError as E;
        let (detail, retryable) = match error {
            E::Failed { detail, retryable } => (detail.clone(), *retryable),
            E::NoSession { detail } => (detail.clone(), true),
            E::Refused { detail } | E::Malformed { detail } => (detail.clone(), false),
            E::TooManyClients { .. } => (error.to_string(), true),
            E::VersionMismatch { .. } => (error.to_string(), false),
        };
        Self { detail, retryable }
    }
}

/// What to do when a request comes back, by the id it was sent under.
///
/// The daemon answers everything under the id it was asked with, so this is
/// the whole of the front end's bookkeeping: a download hands bytes to whoever
/// is waiting, and a send that was refused becomes the failure the message it
/// drew is already able to render.
enum Awaiting {
    Download(oneshot::Sender<Result<std::sync::Arc<Vec<u8>>, Failure>>),
    /// What this account occupies on disk, for the Storage pane.
    Storage(oneshot::Sender<StorageUsage>),
    /// A message drawn before it was sent. On refusal it has to stop being
    /// pending, and the front end is the side that knows which bubble it is.
    Send {
        chat_jid: String,
        local_id: String,
        /// The key a recording was staged under for this request, if there
        /// is one. `media::take` is what normally removes it — and that only
        /// runs if the daemon actually acts on the request, so a send that
        /// never got that far would leave the payload behind and every retry
        /// would stage another.
        ///
        /// A key rather than a path: where the payload lives is the cache's
        /// business, and one of the two caches has no paths at all.
        staged: Option<String>,
    },
    /// Status updates this window has already drawn as watched. A refusal has
    /// to reach them, or the ring stays down over a view nothing recorded.
    StatusView {
        message_ids: Vec<String>,
    },
    /// A request whose whole answer is that it was done. The daemon acts on
    /// `ClearMediaCache` before it acknowledges, so the ack is the moment the
    /// files are gone and the moment it is worth measuring again.
    Acted(oneshot::Sender<()>),
    /// A page of history. The timeline holds a request in flight so it does
    /// not ask twice for the same one, and a refusal is what releases it.
    Page {
        /// The conversation, or `None` for a page of the chat list.
        jid: Option<String>,
    },
}

impl Awaiting {
    /// Whether nobody is listening for this any more.
    fn is_abandoned(&self) -> bool {
        match self {
            Self::Download(tx) => tx.is_closed(),
            Self::Storage(tx) => tx.is_closed(),
            Self::Acted(tx) => tx.is_closed(),
            // Nor is a ring that has already been taken down: the window is
            // showing the update as watched and only an answer can correct it.
            Self::StatusView { .. } => false,
            // A drawn message is never abandoned: the bubble stays on screen
            // until something resolves it.
            Self::Send { .. } => false,
            // Nor is a page: the view that asked for it is holding a request
            // open and only an answer closes it.
            Self::Page { .. } => false,
        }
    }

    /// Report that this never happened, in the terms its caller understands.
    ///
    /// Takes no cache: a staged payload is dropped by whoever holds one, which
    /// is [`Session`] on the paths that run before a request leaves and the
    /// reader on the paths that run after. Both call [`Self::staged_key`]
    /// first.
    fn failed(self, failure: &Failure, events: Option<&EventSink>) {
        let detail = failure.detail.as_str();
        match self {
            // The only caller that reads more than the sentence: whether
            // asking again could work decides what the person is told to do
            // about it. See [`Failure`].
            Self::Download(tx) => {
                let _ = tx.send(Err(failure.clone()));
            }
            // Dropping the sender is the failure: the pane it feeds shows
            // what it knows and says the rest is unavailable.
            Self::Storage(tx) => {
                log::debug!("storage query failed: {detail}");
                drop(tx);
            }
            // Same shape: the caller waits on the channel, and a dropped
            // sender is what tells it the thing did not happen.
            Self::Acted(tx) => {
                log::warn!("a request was refused: {detail}");
                drop(tx);
            }
            Self::Send {
                chat_jid,
                local_id,
                staged: _,
            } => {
                // The message is already drawn; without this it stays pending
                // forever.
                if let Some(events) = events {
                    let _ = events.send(FromDaemon::Session(Box::new(UiEvent::SendFailed {
                        chat_jid,
                        message_id: local_id,
                        reason: detail.to_string(),
                    })));
                }
            }
            // The view that asked is waiting on this and will not ask again
            // until it hears something.
            Self::Page { jid } => {
                log::warn!("a page of history did not arrive: {detail}");
                if let Some(events) = events {
                    let _ = events.send(FromDaemon::PageLost { jid });
                }
            }
            // The ring is already down over these. Nothing durable exists, so
            // the window has to be told to put it back rather than showing a
            // watched update that comes back new on the next start.
            Self::StatusView { message_ids } => {
                log::warn!("a status view was not recorded: {detail}");
                if let Some(events) = events {
                    let _ = events.send(FromDaemon::StatusViewLost(message_ids));
                }
            }
        }
    }

    /// What this request staged, so whoever gives up on it can drop it.
    ///
    /// Nothing will ever read those bytes once the request is not going to
    /// run, and every retry would stage another copy.
    fn staged_key(&self) -> Option<&str> {
        match self {
            Self::Send { staged, .. } => staged.as_deref(),
            _ => None,
        }
    }
}

/// The write half, and the one place it can be given up.
///
/// A [`Link`] is a clone of the connection's writer, and the connection lives
/// exactly as long as the last clone of it does — on a named pipe that is not
/// a figure of speech: the pipe breaks when the last handle to it closes, and
/// cancelling the read at the other end disconnects nothing. While the writer
/// was one field of the session, "the last clone" was that field and drop
/// order was the whole of the argument. A [`SessionHandle`] every part of the
/// window can hold means there are now several, and which one goes last is
/// nobody's decision.
///
/// So the link is held behind one shared slot, and [`Session`]'s drop empties
/// it. Every clone is left holding an empty slot, the writer is released at
/// the moment the owner goes rather than whenever the last borrower does, and
/// a send made through a stale handle afterwards is refused here instead of
/// reaching a daemon the window has already given up on.
#[derive(Clone, Default)]
struct Wire(Arc<Mutex<Option<Link>>>);

impl Wire {
    fn new(link: Link) -> Self {
        Self(Arc::new(Mutex::new(Some(link))))
    }

    /// Send one frame, or say that this connection is over.
    ///
    /// The link is cloned out from under the lock and written to outside it:
    /// a write can block for as long as the transport's own write timeout,
    /// and this lock is on the path of [`Self::close`] — a session dropped
    /// while a send was in flight would otherwise wait for that write before
    /// it could give the writer up.
    fn send_line(&self, frame: &[u8]) -> std::io::Result<()> {
        let link = self.0.lock().unwrap_or_else(|e| e.into_inner()).clone();
        match link {
            Some(link) => link.send_line(frame),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "this connection to the daemon has been closed",
            )),
        }
    }

    /// Give the write half up, whoever else is still holding a handle.
    ///
    /// A send already on its way out holds a clone of its own and finishes:
    /// that frame is one the daemon was going to be sent anyway, and the
    /// writer goes as the call returns.
    fn close(&self) {
        drop(self.0.lock().unwrap_or_else(|e| e.into_inner()).take());
    }
}

type Pending = Arc<Mutex<HashMap<RequestId, Awaiting>>>;

/// Frames waiting for a staged payload ahead of them.
///
/// `serve_client` reads frames in arrival order and the request id only
/// correlates the *answer*, so the order they are written in is the order
/// things happen in. A staged send is written from its upload's completion,
/// which means anything sent meanwhile would otherwise overtake it: finish a
/// voice note, type a line, and the recipient reads the line first.
///
/// So while a staging is in flight everything queues behind it. On a desktop
/// staging completes before the call that started it returns, so the slot is
/// claimed and filled inside one call and nothing ever queues.
///
/// A queue of *slots* rather than a count and a queue of frames, because two
/// voice notes can be staging at once and their uploads finish in whatever
/// order the network settles them. Counting told the second one to finish
/// that it was the head of the queue, so recording two notes and letting the
/// shorter one land first delivered them in the wrong order, the position is
/// taken when the send is made, and what arrives later only fills it.
#[derive(Default)]
struct Outbox {
    /// Everything not yet written, in the order it was asked for.
    slots: std::collections::VecDeque<Slot>,
    /// The next ticket a staged send is given.
    next_ticket: u64,
}

/// One place in the outbox.
enum Slot {
    /// A frame that could not go out because something ahead of it has not,
    /// and the reservation it answers for where it has one.
    ///
    /// The id is carried so a flush can tell a frame still worth writing from
    /// one whose request has already been failed. A frame with no id is
    /// fire-and-forget, nothing is waiting on it, and writing it late costs
    /// nothing.
    Ready(Option<RequestId>, Vec<u8>),
    /// A staged send whose payload is still crossing.
    Awaiting(u64),
}

impl Outbox {
    /// Claim a place for a send whose payload has not been staged yet.
    fn claim(&mut self) -> u64 {
        let ticket = self.next_ticket;
        self.next_ticket = self.next_ticket.wrapping_add(1);
        self.slots.push_back(Slot::Awaiting(ticket));
        ticket
    }

    /// Put a frame in the place this ticket claimed, or give the place up.
    ///
    /// Giving it up rather than filling it is what a staging that failed
    /// does: the frame is never going to exist, and everything behind it is
    /// still waiting.
    fn fill(&mut self, ticket: u64, frame: Option<(RequestId, Vec<u8>)>) {
        let Some(at) = self
            .slots
            .iter()
            .position(|slot| matches!(slot, Slot::Awaiting(held) if *held == ticket))
        else {
            return;
        };
        match frame {
            Some((id, frame)) => self.slots[at] = Slot::Ready(Some(id), frame),
            None => {
                self.slots.remove(at);
            }
        }
    }

    /// Everything at the head that is ready to be written.
    ///
    /// Taken out under the lock and written outside it, because a write can
    /// block and the lock is on the path of every other send.
    fn drain_ready(&mut self) -> Vec<(Option<RequestId>, Vec<u8>)> {
        let mut ready = Vec::new();
        while let Some(Slot::Ready(..)) = self.slots.front() {
            let Some(Slot::Ready(id, frame)) = self.slots.pop_front() else {
                break;
            };
            ready.push((id, frame));
        }
        ready
    }
}

type Sending = Arc<Mutex<Outbox>>;

/// What ends this connection's reader when the connection is dropped.
///
/// The read half is not the writer's to drop: it belongs to whatever the
/// transport parks in, and nothing wakes it while the daemon has nothing to
/// say. So dropping a [`Session`] used to leave the connection open — every
/// reconnect after a network blip left another one behind, and at the
/// daemon's `MAX_CLIENTS` the window never connected again, with the ghosts
/// all still claiming to hold a window.
///
/// What "end it" means is the transport's, so it arrives as a closure from
/// whichever module opened the connection.
pub(super) struct Teardown(Option<Box<dyn FnOnce() + Send>>);

impl Teardown {
    /// Nothing to end. A page's socket goes with the page.
    pub(super) fn none() -> Self {
        Self(None)
    }

    #[cfg(not(target_family = "wasm"))]
    pub(super) fn new(end: impl FnOnce() + Send + 'static) -> Self {
        Self(Some(Box::new(end)))
    }
}

impl Drop for Teardown {
    fn drop(&mut self) {
        if let Some(end) = self.0.take() {
            end();
        }
    }
}

/// Everything a request is made with, and nothing that ends the connection.
///
/// Cheap to clone and shared by every clone: the request table, the outbox,
/// the id counter, the media cache, the events channel and the call frames
/// are one set per connection, whoever holds a handle to them. The window is
/// not one thing that sends — a call bar, a composer, a settings pane and a
/// plugin surface all do — and before this each of them had to borrow the one
/// [`Session`] the root held, which is a good part of why the root held
/// everything.
///
/// What a handle deliberately does *not* carry is the teardown: ending the
/// connection is [`Session`]'s, once, and a part of the window that can send
/// must not be a part of the window that can hang up. Sending through a
/// handle whose session has gone is refused by [`Wire`] rather than being an
/// error the holder has to prevent.
#[derive(Clone)]
pub struct SessionHandle {
    /// Everything a request is written with, and every part of it crosses a
    /// thread.
    conn: Conn,
    /// The newest decoded picture of each direction, written where the frames
    /// are read and taken by the window. See [`FromDaemon::CallFrames`].
    ///
    /// Outside [`Conn`] because a decoded picture is the one thing here that
    /// does not travel: on a page it is a `Rc<RefCell<..>>` of frames the
    /// browser decoded, so a bundle carrying it could not be moved into the
    /// `Send` callback a staged send finishes on. Nothing in that callback
    /// wants a picture anyway — it is writing a frame, and the video belongs
    /// to whoever is drawing.
    frames: crate::video::LatestFrames,
}

/// The request-making half of a connection, as one cloneable set.
///
/// Every send needs all of it and the staged path needs to *own* all of it —
/// a voice note is written from the completion of its own upload, which
/// outlives the call that started it — so it is one value rather than five
/// arguments, which is what this reached eight parameters as.
#[derive(Clone)]
struct Conn {
    /// The write half, whichever transport is under it, behind the slot the
    /// owner empties. The reader holds the other half, and the two are used
    /// at the same time.
    wire: Wire,
    pending: Pending,
    /// Keeps the wire in the order the person acted in. See [`Outbox`].
    outbox: Sending,
    /// Shared, because an id correlates an answer for the whole connection:
    /// two handles taking ids from two counters would be answering each
    /// other's requests.
    next_id: Arc<AtomicU64>,
    /// Where the bytes a frame only names live.
    ///
    /// Held here as well as by the reader because sending has a direction of
    /// its own: a voice note is staged by this side and read by the daemon.
    media: Arc<dyn MediaCache>,
    /// The same channel the reader publishes on.
    ///
    /// A send can fail on *this* side — the socket is gone, or the recording
    /// could not be staged in the media cache — and the front end has already
    /// drawn the message. Without a way to say so from here those failures had
    /// nowhere to go, and the bubble sat pending for good with no retry.
    events: EventSink,
}

/// A connection to `oxidezapd`, and the one value that ends it.
///
/// The requests all live on [`SessionHandle`], which this derefs to, so a
/// caller that only sends can be handed a handle and never learn that the
/// connection is somebody's to end. There is exactly one of these per
/// connection, it lives where the window's own state does, and dropping it is
/// how a reconnect begins.
pub struct Session {
    handle: SessionHandle,
    /// How this connection's reader is ended when this goes.
    ///
    /// Last, and that is load-bearing on a named pipe: cancelling a pipe read
    /// disconnects nothing — the pipe breaks when the last handle to it
    /// closes — so the write half has to be released *before* the hangup
    /// runs. Two things keep that true, and both are needed now that the
    /// write half is reachable from every clone of the handle:
    ///
    /// - this value's own `Drop` empties the shared slot the link sits in
    ///   (see [`Wire`]), which releases the writer however many handles
    ///   outlive the session, and runs before any field is dropped;
    /// - and this field stays declared last, so the local reasoning holds
    ///   too: fields drop in declaration order, and `handle` — the only place
    ///   the link is reachable from — goes before the hangup does.
    ///
    /// Held for its `Drop` and read by nobody, which on a page is the whole
    /// of it: there is no reader thread to end.
    #[cfg_attr(
        target_family = "wasm",
        expect(dead_code, reason = "a page's socket goes with the page")
    )]
    teardown: Teardown,
}

impl std::ops::Deref for Session {
    type Target = SessionHandle;

    /// Every request, read through the value that owns the connection.
    ///
    /// A `Deref` rather than a hundred forwarding methods, and it is honest
    /// about the containment: a session *is* a handle plus the ending, and
    /// the only thing it hides is that the caller could have taken a handle
    /// of its own instead. Which is what most callers should do — see
    /// [`Session::handle`].
    fn deref(&self) -> &Self::Target {
        &self.handle
    }
}

impl Drop for Session {
    /// End the connection, in the one order that ends it everywhere.
    ///
    /// The link goes first and explicitly, because a handle held anywhere
    /// else would otherwise keep the transport's last write handle open, and
    /// the daemon would go on counting a client that has gone — the ghost
    /// connections [`Teardown`] describes, arriving by a different road. The
    /// hangup that follows is the field below, dropped once this body has
    /// run.
    fn drop(&mut self) {
        self.handle.conn.wire.close();
    }
}

impl Session {
    /// Attach to the daemon, however this front end reaches one.
    ///
    /// Returns the events it will publish. The daemon reloads history for a
    /// client that asks for events, so the chats arrive without being asked
    /// for separately.
    ///
    /// The two platforms differ in what "no daemon" means, and only in that:
    /// a process starts one, a page reports that it could not reach one.
    ///
    /// # Errors
    ///
    /// Nothing is listening and nothing could be made to listen.
    pub async fn connect() -> std::io::Result<(Self, Events)> {
        #[cfg(not(target_family = "wasm"))]
        {
            native::connect()
        }
        #[cfg(target_family = "wasm")]
        {
            web::connect().await
        }
    }

    /// [`Self::connect`], on whichever thread can carry it.
    ///
    /// Off the UI thread on a desktop: connecting there can mean starting a
    /// daemon and waiting for it to listen, which is a spinner rather than a
    /// frozen window only if it happens somewhere else.
    ///
    /// On the *window's own* thread in a browser, and that is not a
    /// preference. gpui's background executor is a real worker there, and a
    /// worker has no `window` — so the socket's URL would silently ignore the
    /// page's `?daemon=`, and every media fetch afterwards would fail for
    /// want of somewhere to fetch from. There is nothing to move off the
    /// thread anyway: a page cannot start a daemon, and its socket opens
    /// asynchronously, so this returns immediately.
    ///
    /// Here rather than at the call site, because *where a connection opens*
    /// is the same question as *how* one opens and belongs in the same
    /// module: a second platform decision in the app is a second place every
    /// new target has to be taught about.
    ///
    /// # Errors
    ///
    /// As [`Self::connect`].
    pub async fn attach(cx: &mut gpui::AsyncApp) -> std::io::Result<(Self, Events)> {
        #[cfg(not(target_family = "wasm"))]
        {
            use gpui::AppContext as _;
            cx.background_spawn(Self::connect()).await
        }
        #[cfg(target_family = "wasm")]
        {
            let _ = cx;
            Self::connect().await
        }
    }

    /// Whether asking again could ever produce a different answer.
    ///
    /// Most connection failures are transient — a daemon still starting, a
    /// socket not yet accepted — and the error screen is right to keep
    /// trying. Two are not, and both are refusals rather than accidents:
    /// another tab already holds this account, and a preview that has not
    /// been told it may keep one. Retrying either is worse than not: the
    /// window sits looking like it is starting, the reason never reaches the
    /// person, and in the first case the moment the other tab closes this one
    /// silently takes an account nobody was looking at — the exact behaviour
    /// the browser lock's `ifAvailable` was chosen to prevent.
    ///
    /// Manual retry stays available, because the answer *does* change when
    /// somebody closes the other tab. What stops is guessing on their behalf.
    pub fn is_settled(error: &std::io::Error) -> bool {
        matches!(
            error.kind(),
            std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::PermissionDenied
        )
    }

    /// The parts every transport supplies, assembled.
    fn new(link: Link, events: EventSink, media: Arc<dyn MediaCache>) -> Self {
        Self {
            handle: SessionHandle {
                conn: Conn {
                    wire: Wire::new(link),
                    pending: Pending::default(),
                    outbox: Sending::default(),
                    next_id: Arc::new(AtomicU64::new(1)),
                    media,
                    events,
                },
                frames: crate::video::LatestFrames::default(),
            },
            teardown: Teardown::none(),
        }
    }

    /// Say how this connection's reader is ended, once there is one to end.
    ///
    /// Set after construction because the reader needs what construction
    /// builds — the request table and the frame slots — so there is no reader
    /// to name until the session exists.
    #[cfg(not(target_family = "wasm"))]
    fn ends_with(&mut self, teardown: Teardown) {
        self.teardown = teardown;
    }

    /// A handle of one's own, for a part of the window that only sends.
    ///
    /// The thing to hand an entity: it is a refcount on the same connection,
    /// it carries every request, and it carries no way to end anything. A
    /// handle outliving this session is not a leak and not a bug — its sends
    /// are refused from the moment the session goes, which is the answer a
    /// stale caller wants anyway.
    ///
    /// Called from the tests below the window for now, which is the honest
    /// state of it: the window still reaches its connection through the one
    /// root that owns it, and handing each part of it a handle instead is the
    /// point of the split rather than a thing this change does. Named rather
    /// than left to a crate-wide `allow`, the way `notices` names its own.
    #[allow(dead_code)]
    #[must_use]
    pub fn handle(&self) -> SessionHandle {
        self.handle.clone()
    }
}

impl SessionHandle {
    /// The newest decoded picture of each direction of the live call.
    ///
    /// Taken by the window when [`FromDaemon::CallFrames`] says one is
    /// waiting; the reader has been overwriting the slot in the meantime,
    /// which is exactly what should happen to a picture nobody drew.
    pub fn call_frames(&self) -> &crate::video::LatestFrames {
        &self.frames
    }

    /// Send a request nobody is waiting on an answer for.
    fn send(&self, request: ClientRequest) -> std::io::Result<()> {
        self.send_frame(&Request::bare(request))
    }

    fn send_frame(&self, request: &Request) -> std::io::Result<()> {
        let frame = serde_json::to_vec(request).map_err(std::io::Error::other)?;
        // The unreserved callers want an io error, not the reservation list:
        // nothing on this path is waiting on an id, so the reason is all
        // there is to report.
        write_or_queue(&self.conn, request.id, frame).map_err(|Unwritten { reason, .. }| reason)
    }

    /// Send and log rather than propagate.
    ///
    /// Every caller here is a UI action whose outcome arrives as an event, and
    /// a socket that has broken takes the whole session with it — there is no
    /// per-action recovery for the caller to perform.
    fn tell(&self, request: ClientRequest) {
        if let Err(e) = self.send(request) {
            error!("could not reach the daemon: {e}");
        }
    }

    /// Send a request and remember what its answer means.
    fn ask(&self, request: ClientRequest, waiting: Awaiting) -> RequestId {
        let id = self.reserve(waiting);
        deliver(&self.conn, id, request, Delivery::Ordinary);
        id
    }

    /// Claim an id and record what its answer will mean.
    ///
    /// Split from the send because a staged request reserves now and leaves
    /// later: the payload has to reach the daemon before the frame naming it
    /// does, and the id still has to be taken in the order the person acted
    /// in. See [`Self::send_audio_message`].
    fn reserve(&self, waiting: Awaiting) -> RequestId {
        let id = self.conn.next_id.fetch_add(1, Ordering::Relaxed);
        let mut pending = self.conn.pending.lock().unwrap_or_else(|e| e.into_inner());
        // Whoever gave up is no longer listening, and its answer may never
        // come. Swept here rather than on a timer: this is the only thing
        // that grows the map, so it is the only place that needs to shrink
        // it.
        pending.retain(|_, waiting| !waiting.is_abandoned());
        pending.insert(id, waiting);
        id
    }

    pub fn send_message(
        &self,
        jid: &str,
        text: &str,
        local_id: String,
        quoted: Option<QuotedMessage>,
    ) {
        self.ask(
            ClientRequest::SendText(SendText {
                jid: jid.to_string(),
                text: text.to_string(),
                local_id: Some(local_id.clone()),
                quoted,
            }),
            Awaiting::Send {
                chat_jid: jid.to_string(),
                local_id,
                staged: None,
            },
        );
    }

    pub fn send_audio_message(
        &self,
        jid: &str,
        audio: Vec<u8>,
        duration_secs: u32,
        waveform: Vec<u8>,
        local_id: String,
        quoted: Option<QuotedMessage>,
    ) {
        self.send_staged(jid, local_id, audio, "the recording", |upload, local_id| {
            ClientRequest::SendAudio(SendAudio {
                jid: jid.to_string(),
                upload,
                duration_secs,
                waveform,
                local_id: Some(local_id),
                quoted,
            })
        });
    }

    /// Send a file somebody picked, staged the way a recording is.
    ///
    /// The daemon is the side that works out what the file looks like — its
    /// dimensions, its thumbnail — because it is the side holding the bytes
    /// when the message is built. What travels here is what the *picker*
    /// knew: what the file is called, what it is, and what it is being sent
    /// as.
    pub fn send_media_message(
        &self,
        jid: &str,
        file: Attachment,
        local_id: String,
        quoted: Option<QuotedMessage>,
    ) {
        let Attachment {
            bytes,
            kind,
            mime_type,
            file_name,
            caption,
        } = file;
        self.send_staged(jid, local_id, bytes, "that file", |upload, local_id| {
            ClientRequest::SendMedia(SendMedia {
                jid: jid.to_string(),
                upload,
                kind,
                mime_type,
                file_name,
                caption,
                local_id: Some(local_id),
                quoted,
            })
        });
    }

    /// Send a request whose payload goes through the media cache.
    ///
    /// Both of the things this front end sends that are too big for a frame —
    /// a recording and a picked file — and every ordering question they raise
    /// is the same one, which is why they are one function. `what` names the
    /// payload for the sentence a failed staging produces, because "the
    /// recording could not be staged" is what somebody who pressed the
    /// microphone needs to read and not what somebody who picked a file does.
    fn send_staged(
        &self,
        jid: &str,
        local_id: String,
        payload: Vec<u8>,
        what: &'static str,
        request: impl FnOnce(String, String) -> ClientRequest,
    ) {
        // Through the media cache: these are the things this side sends that
        // do not belong in a frame. The key is the local id, which is already
        // unique per send.
        let upload = oxidezap_ipc::staged_key(&sanitize(&local_id));
        let request = request(upload.clone(), local_id.clone());
        // The ceiling, once, where every staged payload passes rather than in
        // each of the four caches. Only one of those enforced it — the web
        // bridge's `PUT`, which has to, because it reads the body into the
        // process holding the account — so the same recording that a page
        // could not stage went out from a desktop, and the sentence in
        // `MAX_STAGED_BYTES` was true of one transport.
        //
        // A backstop rather than the check somebody sees: a file is refused at
        // the chooser, by name and before it is read, and a voice note reaches
        // this at about a megabyte per ten minutes. What it is here for is the
        // path that grows a payload nobody measured.
        let size = payload.len() as u64;
        if size > oxidezap_ipc::MAX_STAGED_BYTES {
            // Reserved and failed rather than dropped: the caller draws its
            // bubble after this returns, and the failure travels the events
            // channel, so it lands on a message that exists by then. Nothing
            // is staged and no place is claimed in the outbox — there is no
            // frame for anything to queue behind.
            let id = self.reserve(Awaiting::Send {
                chat_jid: jid.to_string(),
                local_id,
                staged: None,
            });
            fail_reserved(
                &self.conn,
                id,
                // The same two figures the file chooser prints, from the same
                // formatter: a person who was told a file fits and then told
                // it does not must not be reading two different numbers.
                format!(
                    "{what} could not be sent: it is {} and the most that can be staged is {}.",
                    crate::utils::format_size(size),
                    crate::utils::format_size(oxidezap_ipc::MAX_STAGED_BYTES)
                ),
            );
            return;
        }
        // Reserved before the payload is staged and sent after it lands. The
        // id is taken in the order the person acted in, which a reservation
        // made later would not be; the *frame* waits, because the daemon
        // opens the payload when it handles the request and a page stages it
        // over HTTP. Where staging is a local write this all still happens
        // before the call returns.
        let id = self.reserve(Awaiting::Send {
            chat_jid: jid.to_string(),
            local_id,
            staged: Some(upload.clone()),
        });
        // Claimed before the upload begins, so anything sent while it runs
        // queues behind it rather than overtaking it on the wire, and so
        // that a second payload claims the place *after* this one whichever
        // upload lands first.
        let ticket = self
            .conn
            .outbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .claim();
        // The callback outlives this borrow, so it takes the connection
        // with it. One clone rather than five, and no picture: see [`Conn`].
        let conn = self.conn.clone();
        self.conn.media.stage_then(
            &upload,
            payload,
            Box::new(move |staged| match staged {
                Ok(()) => deliver(&conn, id, request, Delivery::Staged(ticket)),
                Err(e) => {
                    // The queue behind this send is waiting on a frame that
                    // is never going to be written. Releasing it can fail in
                    // turn, and what that leaves is the same as anywhere
                    // else: reservations nobody will ever answer, with the
                    // views that made them waiting for good. Answered here
                    // rather than discarded, exactly as `deliver` does.
                    if let Err(Unwritten { lost, reason }) = release(&conn, ticket, None) {
                        error!("could not reach the daemon: {reason}");
                        for lost in lost {
                            fail_reserved(
                                &conn,
                                lost,
                                format!("could not reach the daemon: {reason}"),
                            );
                        }
                    }
                    fail_reserved(&conn, id, format!("could not stage {what}: {e}"));
                }
            }),
        );
    }

    pub fn send_composing(&self, jid: &str) {
        self.typing(jid, true);
    }

    pub fn send_paused(&self, jid: &str) {
        self.typing(jid, false);
    }

    fn typing(&self, jid: &str, composing: bool) {
        self.tell(ClientRequest::Typing(Typing {
            jid: jid.to_string(),
            composing,
        }));
    }

    /// Mark a chat read up to the message the UI is looking at.
    ///
    /// One request where the old client took two: the daemon owns the read
    /// boundary and the receipts that go with it, so a front end no longer
    /// computes either. `through_message_id` is the newest message this side
    /// holds, and the daemon refuses anything else — a read is irreversible
    /// and must not reach past what the user has seen.
    pub fn mark_chat_read(&self, jid: &str, through_message_id: Option<String>) {
        self.tell(ClientRequest::MarkRead(MarkRead {
            jid: jid.to_string(),
            through_message_id,
        }));
    }

    /// Ask the daemon to republish the history it holds.
    ///
    /// Only used to settle a disagreement this side cannot settle on its own:
    /// the store is the truth about what was written, and re-reading it beats
    /// guessing at it. Nothing else needs it — a client that attaches is sent
    /// a reload without asking.
    pub fn reload_history(&self) {
        self.tell(ClientRequest::ReloadHistory);
    }

    /// Tell the daemon these status updates have been watched.
    ///
    /// Not `mark_chat_read` on the broadcast: that clears one chat, and the
    /// broadcast holds every contact's updates. Nothing goes to the network —
    /// this is the local half of a status view, and the daemon is where it has
    /// to live to outlast the window.
    ///
    /// Tracked rather than told, unlike the other one-way requests: the
    /// window takes the ring down before the daemon has answered, so a
    /// refusal — a read-only store, a full disk, a socket that is gone — has
    /// to find its way back to the update it was about. Without an id the
    /// daemon's answer is an uncorrelated log line, and the ring stays down
    /// over a view nothing recorded.
    /// Ask for one page of a conversation, older than `before`.
    ///
    /// `None` asks for the newest page, which is what opening a chat wants;
    /// the cursor from the last answer asks for what came before it. The page
    /// arrives as [`FromDaemon::Messages`], and a refusal as
    /// [`FromDaemon::PageLost`] — the view is holding a request open either
    /// way, and only an answer releases it.
    pub fn load_messages(&self, jid: String, before: Option<PageCursor>) {
        self.ask(
            ClientRequest::LoadMessages(LoadMessages {
                jid: jid.clone(),
                before,
                // The daemon's own page size, which is the one number that
                // has to match the store's indexes: a front end with an
                // opinion about it would be guessing.
                limit: None,
            }),
            Awaiting::Page { jid: Some(jid) },
        );
    }

    /// Ask for one page of the chat list, after `after`.
    pub fn load_chats(&self, after: Option<PageCursor>) {
        self.ask(
            ClientRequest::LoadChats(LoadChats { after, limit: None }),
            Awaiting::Page { jid: None },
        );
    }

    pub fn mark_status_watched(&self, message_ids: Vec<String>) {
        if message_ids.is_empty() {
            return;
        }
        self.ask(
            ClientRequest::MarkStatusWatched(MarkStatusWatched {
                message_ids: message_ids.clone(),
            }),
            Awaiting::StatusView { message_ids },
        );
    }

    pub fn start_call(&self, jid: &str, is_video: bool, placeholder_id: String) {
        self.call(CallAction::Start {
            jid: jid.to_string(),
            video: is_video,
            placeholder_id,
        });
    }

    pub fn accept_call(&self, call_id: &str) {
        self.call(CallAction::Accept {
            call_id: call_id.to_string(),
        });
    }

    pub fn decline_call(&self, call_id: &str) {
        self.call(CallAction::Decline {
            call_id: call_id.to_string(),
        });
    }

    pub fn cancel_call(&self, call_id: &str) {
        self.call(CallAction::Cancel {
            call_id: call_id.to_string(),
        });
    }

    pub fn set_call_muted(&self, call_id: &str, muted: bool) {
        self.call(CallAction::SetMuted {
            call_id: call_id.to_string(),
            muted,
        });
    }

    /// Turn this window's camera on or off during a live call.
    ///
    /// Only ever our own direction: the peer's camera is theirs. Turning it
    /// on is also how the peer's request to go to video is answered — an
    /// acceptance *is* a camera coming on — so there is no second request for
    /// that.
    pub fn set_call_video(&self, call_id: &str, enabled: bool) {
        self.call(CallAction::SetVideo {
            call_id: call_id.to_string(),
            enabled,
        });
    }

    fn call(&self, action: CallAction) {
        self.tell(ClientRequest::Call(action));
    }

    /// Allow, or stop allowing, what a plugin asked to be able to do.
    ///
    /// Not a [`plugin_action`](Self::plugin_action) with a reserved id: an
    /// action id comes from the plugin's own tree, so a plugin could publish
    /// a button labelled "OK" carrying that id and be granted by somebody
    /// pressing the wrong thing.
    pub fn plugin_approval(&self, plugin: &str, approved: bool) {
        self.tell(ClientRequest::PluginApproval {
            plugin: plugin.to_string(),
            approved,
        });
    }

    /// Ask the daemon to read its plugin folder again and run what is in it.
    ///
    /// Fire and forget from here, like everything else on this channel: what
    /// came back arrives as a republished set of surfaces, which is how every
    /// *other* window learns of it too — a reload is the daemon's, not this
    /// window's, and one window pressing it is not a fact the others should
    /// have to be told separately.
    pub fn reload_plugins(&self) {
        self.tell(ClientRequest::ReloadPlugins);
    }

    /// Tell a plugin somebody used one of its widgets.
    ///
    /// Fire and forget, like typing is: the daemon hands it to the plugin and
    /// what the plugin makes of it comes back as a new interface, or as
    /// nothing. There is no answer to wait for, because "the plugin took it"
    /// is not a fact this window can do anything with.
    ///
    /// The open chat travels along because the daemon does not know it: a
    /// header button is about the conversation the person pressing it was
    /// looking at, and two windows can have different ones.
    pub fn plugin_action(
        &self,
        plugin: &str,
        action: &str,
        value: Option<String>,
        chat_jid: Option<String>,
        slot: oxidezap_core::PluginSlot,
        widget: oxidezap_core::PluginWidget,
    ) {
        self.tell(ClientRequest::PluginAction {
            action: oxidezap_core::PluginAction {
                plugin: plugin.to_string(),
                action: action.to_string(),
                value,
                chat_jid,
                slot,
                widget,
            },
        });
    }

    /// Say how much the daemon should log, answered when it has been done.
    ///
    /// Answered rather than told, because on a desktop the daemon is also
    /// the process that writes the choice down — it persists before it
    /// acknowledges — so this receiver is how the window learns that
    /// somebody remembered the level rather than merely that somebody was
    /// handed it. A frame can sit in a full outbox, and a window that closed
    /// while it sat there would leave nobody having written anything.
    pub fn set_log_level(&self, level: oxidezap_core::LogLevel) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        self.ask(ClientRequest::SetLogLevel { level }, Awaiting::Acted(tx));
        rx
    }

    /// Wipe the local store and pair again.
    ///
    /// The daemon owns that file and stops itself once it is gone, so this is
    /// the last thing this connection will be told anything on.
    pub fn forget_session(&self) {
        self.tell(ClientRequest::ForgetSession);
    }

    /// Ask what the store and the media cache occupy.
    ///
    /// The daemon measures, because it is the only process that owns either
    /// path. The answer arrives under this request's id.
    pub fn storage_usage(&self) -> oneshot::Receiver<StorageUsage> {
        let (tx, rx) = oneshot::channel();
        self.ask(ClientRequest::StorageUsage, Awaiting::Storage(tx));
        rx
    }

    /// Delete the cached media, keeping the history.
    ///
    /// Answered rather than told, because the daemon wipes before it
    /// acknowledges: measuring the moment the request goes out reads the size
    /// the files still had, which looks exactly like a clear that did not
    /// work.
    pub fn clear_media_cache(&self) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        self.ask(ClientRequest::ClearMediaCache, Awaiting::Acted(tx));
        rx
    }

    /// Fetch media, answered when the bytes are available.
    ///
    /// The same signature the old client had, so the callers that thread this
    /// receiver through a timeout did not change. The bytes come back rather
    /// than a path because that is what every caller wanted anyway — and
    /// because one of the two front ends has no paths.
    pub fn download_downloadable_media(
        &self,
        media: DownloadableMedia,
    ) -> oneshot::Receiver<Result<std::sync::Arc<Vec<u8>>, Failure>> {
        let (tx, rx) = oneshot::channel();
        self.ask(
            ClientRequest::Download(Download {
                media: Box::new(media),
            }),
            Awaiting::Download(tx),
        );
        rx
    }
}

/// Write a frame, or hold it behind a staged send that has not gone yet.
///
/// See [`Outbox`]: the wire's order is the order things happen in, so nothing
/// may overtake a send that is waiting on its payload.
fn write_or_queue(conn: &Conn, id: Option<RequestId>, frame: Vec<u8>) -> Result<(), Unwritten> {
    let ready = {
        let mut outbox = conn.outbox.lock().unwrap_or_else(|e| e.into_inner());
        if outbox.slots.is_empty() {
            // Nothing is holding the wire, so this goes straight out. The
            // lock is released before the write for the reason below.
            None
        } else {
            outbox.slots.push_back(Slot::Ready(id, frame.clone()));
            Some(outbox.drain_ready())
        }
    };
    match ready {
        Some(ready) => write_ready(&conn.wire, ready, &conn.pending),
        None => conn.wire.send_line(&frame).map_err(|e| Unwritten {
            lost: id.into_iter().collect(),
            reason: e,
        }),
    }
}

/// Write frames already taken out of the outbox.
///
/// Taken out under the lock and written here, without it: a write can block
/// and that lock is on the path of every other send, which is the rule
/// [`Outbox::drain_ready`] states and this is the half that keeps it.
///
/// A write that fails takes the rest with it, because they were waiting on a
/// connection that has gone. A frame whose reservation has gone is dropped
/// rather than written, which is that same ending seen from the other side:
/// `Frames::finish` answers every reservation and knows nothing about what is
/// queued here, while the `Link` it holds is a clone that does not
/// necessarily refuse the write. Without that check a line typed behind a
/// voice note could reach the daemon after the window had drawn it as failed.
fn write_ready(
    wire: &Wire,
    ready: Vec<(Option<RequestId>, Vec<u8>)>,
    pending: &Pending,
) -> Result<(), Unwritten> {
    let mut ready = ready.into_iter();
    while let Some((id, frame)) = ready.next() {
        if !worth_writing(id, pending) {
            continue;
        }
        if let Err(e) = wire.send_line(&frame) {
            // Which reservations did not go, rather than which call started
            // the batch. A staged send releases everything queued behind it,
            // so a failure part way through leaves frames *before* the break
            // already written and accepted, and frames after it never sent
            // with their askers still waiting. Failing the caller's own id
            // would report a send that landed and say nothing about the ones
            // that did not.
            let mut lost: Vec<RequestId> = id.into_iter().collect();
            lost.extend(ready.filter_map(|(id, _)| id));
            return Err(Unwritten { lost, reason: e });
        }
    }
    Ok(())
}

/// What a broken write left unsent, and why.
struct Unwritten {
    /// The reservations whose frames never went, oldest first.
    lost: Vec<RequestId>,
    reason: std::io::Error,
}

/// Whether a queued frame is still worth writing.
///
/// A frame with no id is fire-and-forget and nothing is waiting on it. One
/// naming a reservation that has gone is a send the window has already drawn
/// as failed, and writing it would have the daemon act on it anyway.
fn worth_writing(id: Option<RequestId>, pending: &Pending) -> bool {
    id.is_none_or(|id| {
        pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&id)
    })
}

/// Fill a staged send's place in the outbox, and write what that unblocks.
///
/// The frame goes where the send was made, never at the head: another staged
/// send can have been asked for first and still be crossing, and its place is
/// ahead of this one whatever order the two uploads finished in.
fn release(conn: &Conn, ticket: u64, frame: Option<(RequestId, Vec<u8>)>) -> Result<(), Unwritten> {
    let ready = {
        let mut outbox = conn.outbox.lock().unwrap_or_else(|e| e.into_inner());
        outbox.fill(ticket, frame);
        outbox.drain_ready()
    };
    write_ready(&conn.wire, ready, &conn.pending)
}

/// Whether this frame is the one the outbox is holding for.
///
/// A staged send *is* the head of the queue and releases what is behind it; an
/// ordinary one queues like anything else.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Delivery {
    Ordinary,
    /// Filling the place this ticket claimed. See [`Outbox`].
    Staged(u64),
}

/// Write a reserved request's frame, and answer for it if it will not go.
///
/// A free function rather than a method because the staged path sends from a
/// callback that outlives the borrow: everything it needs is cloneable, and
/// the alternative was a second copy of the failure handling below, which is
/// the part that must not drift.
fn deliver(conn: &Conn, id: RequestId, request: ClientRequest, how: Delivery) {
    // Still ours to send. A staged request is delivered from the upload's
    // completion, and the connection can end in between — `Frames::finish`
    // drains every reservation and answers each one. The link it holds is a
    // clone and does not necessarily refuse the write, so without this the
    // daemon could receive and send a voice note the window has already
    // reported as failed.
    if !conn
        .pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&id)
    {
        // Nothing to send, but the queue behind it is still waiting.
        if let Delivery::Staged(ticket) = how {
            let _ = release(conn, ticket, None);
        }
        return;
    }
    let frame = match serde_json::to_vec(&Request {
        id: Some(id),
        request,
    }) {
        Ok(frame) => frame,
        Err(e) => {
            if let Delivery::Staged(ticket) = how {
                let _ = release(conn, ticket, None);
            }
            fail_reserved(conn, id, format!("unserializable: {e}"));
            return;
        }
    };
    let written = match how {
        Delivery::Staged(ticket) => release(conn, ticket, Some((id, frame))),
        Delivery::Ordinary => write_or_queue(conn, Some(id), frame),
    };
    if let Err(Unwritten { lost, reason }) = written {
        error!("could not reach the daemon: {reason}");
        for id in lost {
            fail_reserved(conn, id, format!("could not reach the daemon: {reason}"));
        }
    }
}

/// Answer a reserved request that is never going to run.
///
/// Whoever reserved an id is waiting on it, so a request that never left has
/// to say so in the terms that request was made in.
fn fail_reserved(conn: &Conn, id: RequestId, detail: String) {
    let Some(waiting) = conn
        .pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id)
    else {
        return;
    };
    // This request is not going to run, so whatever it staged is dead:
    // nothing will read those bytes and every retry would stage another copy.
    if let Some(key) = waiting.staged_key() {
        conn.media.discard(key);
    }
    match waiting {
        // `try_send` because this can run on the GPUI executor, and that
        // executor is what drains this queue: `failed` publishes with the
        // waiting variant of the send, so a full queue would park the only
        // thread that could empty it — the window stops rather than saying the
        // message did not go. It is not the only caller any more — a staged
        // send's continuation reaches this from wherever the media cache
        // finished, which on a desktop is the cache's own thread — and the
        // rule is kept for the one that can deadlock rather than split in two
        // by where the failure came from.
        Awaiting::Send {
            chat_jid, local_id, ..
        } => {
            error!("send failed before it left: {detail}");
            conn.events
                .try_send(FromDaemon::Session(Box::new(UiEvent::SendFailed {
                    chat_jid,
                    message_id: local_id,
                    reason: detail,
                })));
        }
        // Same thread, same rule: `failed` would `blocking_send` on the queue
        // this executor drains.
        Awaiting::StatusView { message_ids } => {
            error!("a status view never left this process: {detail}");
            conn.events
                .try_send(FromDaemon::StatusViewLost(message_ids));
        }
        // And the same rule again, for the same reason it is a rule: a view
        // waiting on a page asks for nothing until it hears, so a request
        // that never left has to say so — the reconnect keeps the chats and
        // the paging state, and a list left `Loading` never asks again.
        Awaiting::Page { jid } => {
            error!("a page request never left this process: {detail}");
            conn.events.try_send(FromDaemon::PageLost { jid });
        }
        // Nothing left this process, so nothing about the request itself
        // failed: the connection did, and the app reconnects.
        waiting => waiting.failed(&Failure::worth_retrying(detail), None),
    }
}

/// Keep a key to what `oxidezap_ipc::media_path` accepts.
fn sanitize(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '.'
            }
        })
        .take(120)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidezap_core::MediaContent;

    /// What a flush would write, in order. The reservation ids the slots
    /// carry are how a flush drops what has already been failed; the order is
    /// what these tests are about.
    fn written(outbox: &mut Outbox) -> Vec<Vec<u8>> {
        outbox
            .drain_ready()
            .into_iter()
            .map(|(_, frame)| frame)
            .collect()
    }

    /// A staged send holds the wire until its payload has gone.
    ///
    /// The order frames are written in is the order things happen in, so a
    /// line typed while a voice note is uploading must not reach the
    /// recipient first.
    #[test]
    fn nothing_overtakes_a_send_that_is_still_staging() {
        let mut outbox = Outbox::default();
        let note = outbox.claim();

        // Two ordinary frames while the staging is in flight. Neither may go
        // out, because the note's place is ahead of both.
        outbox
            .slots
            .push_back(Slot::Ready(None, b"text-one".to_vec()));
        outbox
            .slots
            .push_back(Slot::Ready(None, b"text-two".to_vec()));
        assert!(
            written(&mut outbox).is_empty(),
            "the queue is closed while the note is staging"
        );

        // The staged frame goes first, then the two in the order they were
        // asked for.
        outbox.fill(note, Some((1, b"voice-note".to_vec())));
        assert_eq!(
            written(&mut outbox),
            vec![
                b"voice-note".to_vec(),
                b"text-one".to_vec(),
                b"text-two".to_vec()
            ]
        );
    }

    /// A second staged send keeps the queue closed: filling one of two must
    /// not let the rest past the other.
    #[test]
    fn two_staged_sends_both_have_to_land_before_the_queue_drains() {
        let mut outbox = Outbox::default();
        let first = outbox.claim();
        let _second = outbox.claim();
        outbox.slots.push_back(Slot::Ready(None, b"text".to_vec()));

        outbox.fill(first, Some((1, b"note-one".to_vec())));
        assert_eq!(
            written(&mut outbox),
            vec![b"note-one".to_vec()],
            "only what the first note unblocked"
        );
        assert_eq!(outbox.slots.len(), 2, "the second note still closes it");
    }

    /// Two notes recorded in order arrive in that order, whichever upload
    /// finishes first.
    ///
    /// The count-and-prepend version put each completion at the head, so the
    /// shorter note, the one whose upload landed first, was written second
    /// and read first.
    #[test]
    fn staged_sends_keep_the_order_they_were_made_in() {
        let mut outbox = Outbox::default();
        let first = outbox.claim();
        let second = outbox.claim();

        // The second note's payload is the one that crosses first.
        outbox.fill(second, Some((2, b"note-two".to_vec())));
        assert!(
            written(&mut outbox).is_empty(),
            "nothing goes until the note in front of it does"
        );

        outbox.fill(first, Some((1, b"note-one".to_vec())));
        assert_eq!(
            written(&mut outbox),
            vec![b"note-one".to_vec(), b"note-two".to_vec()]
        );
    }

    /// A staging that failed gives its place up rather than holding the queue
    /// shut for ever.
    #[test]
    fn a_staging_that_failed_releases_what_was_behind_it() {
        let mut outbox = Outbox::default();
        let note = outbox.claim();
        outbox.slots.push_back(Slot::Ready(None, b"text".to_vec()));

        outbox.fill(note, None);
        assert_eq!(written(&mut outbox), vec![b"text".to_vec()]);
        assert!(outbox.slots.is_empty());
    }

    /// A frame whose reservation has gone is not written.
    ///
    /// `Frames::finish` fails every reservation when the connection ends, and
    /// it knows nothing about what is queued behind a staging. The `Link` it
    /// holds is a clone that does not necessarily refuse the write, so a line
    /// typed behind a voice note could otherwise reach the daemon after the
    /// window had already drawn it as failed.
    #[test]
    fn a_queued_frame_whose_request_was_failed_is_dropped() {
        let pending: Pending = Pending::default();
        pending.lock().expect("fresh lock").insert(
            7,
            Awaiting::Send {
                chat_jid: "someone@s.whatsapp.net".to_string(),
                local_id: "local-7".to_string(),
                staged: None,
            },
        );

        let mut outbox = Outbox::default();
        outbox
            .slots
            .push_back(Slot::Ready(Some(7), b"kept".to_vec()));
        outbox
            .slots
            .push_back(Slot::Ready(Some(9), b"already failed".to_vec()));
        outbox.slots.push_back(Slot::Ready(None, b"bare".to_vec()));

        let kept: Vec<Vec<u8>> = outbox
            .drain_ready()
            .into_iter()
            .filter(|(id, _)| worth_writing(*id, &pending))
            .map(|(_, frame)| frame)
            .collect();
        assert_eq!(
            kept,
            vec![b"kept".to_vec(), b"bare".to_vec()],
            "the reservation that was failed is the only one dropped"
        );
    }

    /// The recording's key is a local id, which the front end composes; it
    /// still has to be a plain file name.
    #[test]
    fn a_staged_recording_cannot_escape_the_cache() {
        for id in ["../../etc/passwd", "local/1", "local 1"] {
            let key = oxidezap_ipc::staged_key(&sanitize(id));
            assert!(
                oxidezap_ipc::media_path(&key).is_some(),
                "{id} produced {key}"
            );
            assert!(!key.contains('/'), "{key}");
        }
    }

    /// The pending map is swept by whoever adds to it, and a status view has
    /// nobody holding a channel to be closed — the thing waiting on it is a
    /// ring already drawn. Swept as abandoned, its refusal would be dropped
    /// and the ring would stay down over a view nothing recorded.
    #[test]
    fn a_status_view_is_never_swept_before_its_answer() {
        let waiting = Awaiting::StatusView {
            message_ids: vec!["3EB0".into()],
        };
        assert!(!waiting.is_abandoned());
    }

    /// A refusal has to come back as the updates it was about, or the window
    /// has no way to know which ring to put back.
    #[test]
    fn a_refused_status_view_names_the_updates_it_lost() {
        let (tx, mut rx) = sink::channel();
        Awaiting::StatusView {
            message_ids: vec!["A".into(), "B".into()],
        }
        .failed(&Failure::permanent("the store is read-only"), Some(&tx));

        match rx.try_recv() {
            Ok(FromDaemon::StatusViewLost(ids)) => assert_eq!(ids, vec!["A", "B"]),
            _ => panic!("the refusal did not come back as the updates it was about"),
        }
    }

    /// Only a send stages anything, and only a send has anything to drop when
    /// its request turns out not to be going anywhere.
    #[test]
    fn only_a_staged_send_names_something_to_drop() {
        assert_eq!(
            Awaiting::Send {
                chat_jid: "559900000001@s.whatsapp.net".into(),
                local_id: "local-1".into(),
                staged: Some("u-local.1".into()),
            }
            .staged_key(),
            Some("u-local.1")
        );
        assert_eq!(
            Awaiting::StatusView {
                message_ids: vec!["3EB0".into()]
            }
            .staged_key(),
            None
        );
    }

    /// The bytes are the one thing the wire does not carry, so a message
    /// arrives naming its media rather than holding it.
    #[test]
    fn media_bytes_do_not_survive_a_round_trip_through_a_frame() {
        let mut media = MediaContent::image(Arc::new(vec![7; 4096]), "image/jpeg".into(), false);
        // Set here rather than by a constructor: the key is the daemon's to
        // write as the message leaves the process holding the bytes, which is
        // exactly the moment this test is about.
        media.cache_key = Some("m-abc".into());
        let small = serde_json::to_string(&media).unwrap();
        let bigger = serde_json::to_string(&MediaContent {
            data: Arc::new(vec![7; 1024 * 1024]),
            ..media
        })
        .unwrap();
        assert_eq!(
            small.len(),
            bigger.len(),
            "a frame's size must not depend on how big the photo in it is"
        );

        let back: MediaContent = serde_json::from_str(&small).unwrap();
        assert!(back.data.is_empty(), "and the bytes do not come back");
        assert_eq!(back.cache_key.as_deref(), Some("m-abc"));
    }
}
