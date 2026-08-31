//! The client end of the daemon connection, when the daemon is another tab.
//!
//! The fourth transport, and the one with no socket in it at all. A tab that
//! did not win the account's lock is a front end with no session — exactly
//! what a desktop window is — and what it reaches its daemon over is a
//! `BroadcastChannel` named after the connection rather than after the
//! origin. See [`crate::tabs`] for why a channel and not a `MessagePort`.
//!
//! Everything above this is unchanged. The frames are the same frames, the
//! [`Link`] is the same `Link` the WebSocket hands out — a whole frame per
//! message, no terminator, because a channel already frames — and
//! `session/frames.rs` on the front end's side never learns which of the four
//! carried it.
//!
//! # Media does not travel as a frame here either
//!
//! On a socket the front end fetches a payload over HTTP; in the tab that
//! holds the session it reads the daemon's own map. A follower can do
//! neither, so the sideband is three more messages on the same channel —
//! read, stage, discard — and the bytes cross as a `Uint8Array`, which is one
//! structured clone rather than a base64 round trip through JSON.
//!
//! # What closes it
//!
//! A `BroadcastChannel` has no close event, and a tab that is killed says no
//! goodbye. So a connection's liveness is a lock: this end holds
//! [`crate::tabs::liveness_lock_for`] for as long as the connection is worth
//! serving, and the leader waits on it. Nothing polls, and a tab that
//! vanishes is noticed at the moment it vanishes.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{Receiver, Sender, UnboundedSender, channel, unbounded_channel};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::prelude::Closure;
use web_sys::{BroadcastChannel, MessageEvent};

use crate::Link;
use crate::tabs::fields::{
    bytes as bytes_field, number as number_field, set, string as string_field,
};
use crate::tabs::{self, Rendezvous};

/// How many frames off the connection may wait to be applied.
///
/// The same bound and the same reasoning as the WebSocket's: applying a frame
/// means gathering the media it names first, while a daemon with a large
/// history writes its hydration frames as fast as the channel takes them.
/// The one difference is where the pressure lands — both ends are in one
/// browser, so a full queue is another tab's memory rather than a network
/// buffer, which is a reason for the bound rather than against it.
const MAX_QUEUED_FRAMES: usize = 256;

/// What arrives from the tab holding the session.
#[derive(Debug)]
pub enum FromTab {
    /// One frame.
    Line(String),
    /// The connection is over, and why — for the person, not for a log.
    Closed(String),
}

/// A connection to the tab that holds the account.
pub struct Connection {
    /// Where requests go.
    pub link: Link,
    /// Where frames arrive.
    pub incoming: Incoming,
    /// The media sideband.
    pub media: Media,
    /// How this side ends it.
    pub hangup: Hangup,
}

/// The frames from the other tab, and whether it is still there.
pub struct Incoming {
    queued: Receiver<FromTab>,
    ended: Rc<std::cell::Cell<bool>>,
    gone: Rc<std::cell::Cell<bool>>,
}

impl Incoming {
    /// Whether the connection has already gone, ahead of the queue saying so.
    ///
    /// The twin of the WebSocket's `connection_ended`, and it answers the same
    /// question for the same reason: the frames still queued are worth
    /// applying, and the *media* they name is not worth waiting for once the
    /// tab that would serve it has gone. Here that matters more rather than
    /// less — posting to a `BroadcastChannel` nobody is listening on succeeds,
    /// so an unasked question is not refused, it is simply never answered, and
    /// a frame naming a hundred keys would otherwise spend a hundred deadlines
    /// in a row learning that.
    #[must_use]
    pub fn connection_ended(&self) -> bool {
        self.ended.get()
    }

    /// Whether the tab on the other end is the reason it ended.
    ///
    /// Two questions rather than one, and the socket path's answer is why:
    /// there a closed connection says *nothing* about the media sideband,
    /// because the sideband is a different endpoint — so a `Downloaded`
    /// frame, which is somebody's answer rather than a frame's decoration,
    /// is still fetched after a close this page itself caused.
    ///
    /// Here the two are one channel to one tab. When that tab is the reason
    /// the connection ended, it cannot answer for the media either, and
    /// asking anyway spends the whole download allowance discovering it —
    /// with the takeover queued behind. So the exception survives only for
    /// the ending this side chose, where the other tab is still perfectly
    /// well.
    #[must_use]
    pub fn peer_is_gone(&self) -> bool {
        self.gone.get()
    }

    /// The next thing that happened, oldest first.
    pub async fn recv(&mut self) -> Option<FromTab> {
        self.queued.recv().await
    }
}

/// Ending the connection from this side.
///
/// There is one reason to, and it is the good one: this tab has just been
/// handed the account. The front end then reconnects, finds the session in
/// its own address space, and what was a connection to another tab becomes a
/// connection to `daemon::embedded`.
#[derive(Clone)]
pub struct Hangup {
    inbound: Sender<FromTab>,
    ended: Rc<std::cell::Cell<bool>>,
    gone: Rc<std::cell::Cell<bool>>,
}

impl Hangup {
    /// End the connection, saying why.
    ///
    /// The flag is raised before the frame is queued, and that ordering is the
    /// whole value: what is behind this call is a tab that has gone, so the
    /// backlog still in the queue has to drain *without* asking it for
    /// anything. Announcing the ending behind a hundred frames that each spend
    /// a media deadline first is a takeover measured in hours.
    pub fn close(&self, reason: String) {
        self.ended.set(true);
        // Every way this is reached is one where the other tab will not be
        // answering anything more: it took the account, it could not be
        // written to, or this front end has let go of the connection.
        self.gone.set(true);
        deliver(&self.inbound, FromTab::Closed(reason));
    }
}

/// Put one event on the queue, waiting for room only where waiting is right.
///
/// Lifted from the WebSocket transport, which learned it the same way: a
/// `Closed` is one event and says what happened to the connection, so losing
/// it to a full queue leaves the reader waiting on a connection that has
/// already gone. A `Line` does not come through here — a task per overflowing
/// frame is the unbounded queue wearing the scheduler's clothes — except the
/// single frame that *triggers* the overflow, which is spent knowing that
/// nothing is queued after it.
fn deliver(inbound: &Sender<FromTab>, event: FromTab) {
    match inbound.try_send(event) {
        Ok(()) | Err(TrySendError::Closed(_)) => {}
        Err(TrySendError::Full(event)) => {
            let inbound = inbound.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let _ = inbound.send(event).await;
            });
        }
    }
}

/// How long staging a payload may take before it is given up on.
///
/// Both ends are in one browser, so this is not a network allowance — it is
/// the bound on the one failure that has no other end: the tab holding the
/// account can go away between a request and its answer, and nothing about a
/// `BroadcastChannel` says so. Without it the send waiting on that answer
/// waits for the life of the page.
///
/// The lock in `daemon/claim` is what notices the tab leaving, and it is
/// quicker than this — this is the floor under a leader that is present but
/// not answering, which is a bug rather than a state.
///
/// Reads carry their own, because theirs differ: see [`Ask::within_ms`].
const STAGE_MS: i32 = 15_000;

/// The media sideband, as the asking side sees it.
///
/// `Send + Sync` because a front end's media cache is, and safely so: what it
/// holds is a channel into the one task that touches the JS object. The same
/// arrangement, and the same reason, as [`Link`].
#[derive(Clone)]
pub struct Media {
    asks: UnboundedSender<Outgoing>,
}

/// What one media read is allowed, and for how long.
///
/// Three terms rather than three arguments, because they are one decision:
/// a frame's own media is rationed and short, and the answer to a download
/// somebody asked for is neither. Splitting them into positional flags is how
/// the second kind quietly inherited the first kind's deadline.
#[derive(Clone, Copy)]
pub struct Ask {
    /// Release the other tab's claim on these bytes, exactly as
    /// `MediaCache::read_once` does in the tab that holds the cache: the
    /// sideband is a different shape here, not a different contract.
    pub once: bool,
    /// The largest payload worth having, enforced by the tab that *has* the
    /// bytes rather than by the tab that asked. That is the only place it can
    /// be: a ceiling applied on arrival is applied after the copy it exists to
    /// prevent — the sending tab has already built a `Uint8Array` and the
    /// browser has already cloned it into this heap.
    pub most: u64,
    /// How long to wait for it. The bound here is the tab on the other end
    /// going away, which nothing about a `BroadcastChannel` announces —
    /// posting to a channel with no listener succeeds.
    pub within_ms: i32,
}

impl Media {
    /// The bytes under `key`, from the tab that has them.
    ///
    /// # Errors
    ///
    /// The connection has gone, the other tab does not have the bytes, or they
    /// are larger than the ceiling this ask carried.
    pub async fn read(&self, key: &str, ask: Ask) -> Result<Vec<u8>, String> {
        let (tell, told) = futures_channel::oneshot::channel();
        self.asks
            .send(Outgoing::Read {
                key: key.to_string(),
                once: ask.once,
                most: ask.most,
                answer: tell,
            })
            .map_err(|_| "the tab holding this account has gone".to_string())?;
        match deadline(told, ask.within_ms).await {
            Some(Ok(answer)) => answer,
            _ => Err("the tab holding this account did not answer".to_string()),
        }
    }

    /// Put a payload where the other tab's daemon will look for it.
    ///
    /// # Errors
    ///
    /// The connection has gone, or the other tab refused the write — its
    /// cache is the one with the budget.
    pub async fn stage(&self, key: &str, bytes: Vec<u8>) -> Result<(), String> {
        let (tell, told) = futures_channel::oneshot::channel();
        self.asks
            .send(Outgoing::Stage {
                key: key.to_string(),
                bytes,
                answer: tell,
            })
            .map_err(|_| "the tab holding this account has gone".to_string())?;
        match deadline(told, STAGE_MS).await {
            Some(Ok(answer)) => answer,
            _ => Err("the tab holding this account did not answer".to_string()),
        }
    }

    /// Drop a staged payload whose request is never going to run.
    ///
    /// Best effort and unanswered, like every other discard: what is behind
    /// it is a send that has already failed.
    pub fn discard(&self, key: &str) {
        let _ = self.asks.send(Outgoing::Discard {
            key: key.to_string(),
        });
    }
}

/// What the connection's own task is asked to send.
enum Outgoing {
    Line(String),
    Read {
        key: String,
        once: bool,
        most: u64,
        answer: futures_channel::oneshot::Sender<Result<Vec<u8>, String>>,
    },
    Stage {
        key: String,
        bytes: Vec<u8>,
        answer: futures_channel::oneshot::Sender<Result<(), String>>,
    },
    Discard {
        key: String,
    },
}

/// Where an answer to one sideband request lands.
enum Answer {
    Read(futures_channel::oneshot::Sender<Result<Vec<u8>, String>>),
    Stage(futures_channel::oneshot::Sender<Result<(), String>>),
}

/// Find the tab holding the account and connect to it.
///
/// Returns once that tab has answered. A tab that does not answer within
/// [`tabs::ANSWER_TIMEOUT_MS`] is one that is not there — the leader closed,
/// and what this page does next is try to take the account itself.
///
/// # Errors
///
/// Nobody answered, or the browser has no `BroadcastChannel` to ask on.
pub async fn connect() -> Result<Connection, String> {
    let ask = nonce();

    // The frames channel is opened *before* the ask goes out, and its name is
    // derived from the ask so that it can be: the leader may start writing
    // the moment it answers, and a channel opened after that answer would
    // miss whatever it wrote first. Deriving the name is what removes the
    // race rather than narrowing it.
    let channel_name = tabs::channel_for(&ask);
    let frames = BroadcastChannel::new(&channel_name)
        .map_err(|e| format!("this browser would not open a channel between tabs: {e:?}"))?;

    // Held before the ask, for the same reason and against the opposite
    // failure: the leader waits on this lock to learn that this tab has gone,
    // and a lock taken after the answer is one the leader could find free in
    // between — closing a connection it had only just opened.
    let live = crate::web_locks::hold(&tabs::liveness_lock_for(&ask)).await?;

    let (incoming, from_leader) = channel(MAX_QUEUED_FRAMES);
    let ended = Rc::new(std::cell::Cell::new(false));
    let gone = Rc::new(std::cell::Cell::new(false));
    let answers: Rc<RefCell<HashMap<u64, Answer>>> = Rc::new(RefCell::new(HashMap::new()));
    let on_frame = frame_handler(&incoming, &answers, &ended, &gone);
    frames.set_onmessage(Some(on_frame.as_ref().unchecked_ref()));

    let hangup = Hangup {
        inbound: incoming,
        ended: Rc::clone(&ended),
        gone: Rc::clone(&gone),
    };

    // One listener on the rendezvous, opened before the ask and kept for the
    // life of the connection. Two would have been the obvious shape — one to
    // wait for the answer, one to watch for a new leader — and it leaves a
    // gap between them: a leader that dies in that moment is replaced by a
    // tab whose `Leading` this one is not listening for, and a broadcast is
    // never replayed. What is missed there is missed permanently, which is
    // the failure this watch exists to prevent, so the watch may not have a
    // beginning that comes after the ask.
    let mut meeting = Meeting::open(&ask, hangup.clone())?;
    if let Err(e) = meeting.ask_and_wait().await {
        // Closed by hand rather than left to the collector: a channel with a
        // handler on it is reachable from the browser, and this ask is one of
        // several a tab makes while the leader is still starting up.
        frames.set_onmessage(None);
        frames.close();
        drop(on_frame);
        drop(live);
        return Err(e);
    }

    let (asks, mut to_send) = unbounded_channel::<Outgoing>();
    let (lines, mut written) = unbounded_channel::<String>();
    {
        // Lines and sideband requests are one queue on the way out, so the
        // task below waits on one thing. A front end writes lines from
        // wherever a click lands and `Link` takes a string channel, so the
        // joining happens here rather than in every caller.
        let asks = asks.clone();
        let released = hangup.clone();
        wasm_bindgen_futures::spawn_local(async move {
            while let Some(line) = written.recv().await {
                if asks.send(Outgoing::Line(line)).is_err() {
                    break;
                }
            }
            // The `Link` going is the front end letting go of this
            // connection, and it is the *only* signal of that: a Reconnect
            // drops the session while the channel is perfectly quiet, and
            // nothing else here would ever notice. Without this the reader
            // task waits on frames that will not come while holding the
            // media handles, those hold the outgoing task, and that holds the
            // liveness lock — so the leader goes on serving a connection
            // nobody is reading, one more per reconnection, until it is
            // serving `MAX_CLIENTS` ghosts.
            //
            // Saying it here unwinds the whole chain in order: the reader
            // stops on the `Closed`, drops the media handles, and the
            // outgoing task ends with the lock.
            released.close("this window let go of the connection".to_string());
        });
    }

    let failed = hangup.clone();
    wasm_bindgen_futures::spawn_local(async move {
        // Everything the browser must not collect while this connection is
        // open lives in this task and nowhere a caller could drop it: the
        // channel, the handler behind it, and the lock the leader is
        // watching. The task ends when every sender is gone, which is when
        // the front end has let go of the connection.
        let _live = live;
        let _on_frame = on_frame;
        let _meeting = meeting;
        let mut next: u64 = 0;
        while let Some(outgoing) = to_send.recv().await {
            let posted = match outgoing {
                Outgoing::Line(line) => post_line(&frames, &line),
                Outgoing::Read {
                    key,
                    once,
                    most,
                    answer,
                } => {
                    let id = next;
                    next += 1;
                    answers.borrow_mut().insert(id, Answer::Read(answer));
                    post_read(&frames, id, &key, once, most)
                }
                Outgoing::Stage { key, bytes, answer } => {
                    let id = next;
                    next += 1;
                    answers.borrow_mut().insert(id, Answer::Stage(answer));
                    post_stage(&frames, id, &key, &bytes)
                }
                Outgoing::Discard { key } => post_discard(&frames, &key),
            };
            if let Err(e) = posted {
                // Said to the front end, not only to the console. This
                // direction failing is the connection ending — there is no
                // way to ask the other tab anything after it — and a reader
                // that is never told goes on waiting for frames that cannot
                // come, with every request it has outstanding unanswered. It
                // is the same rule the two receiving paths already follow,
                // arrived at from the side that writes.
                log::error!("this tab could not reach the one holding the account: {e:?}");
                failed.close(
                    "this tab lost its connection to the one holding the account".to_string(),
                );
                break;
            }
        }
        frames.close();
    });

    Ok(Connection {
        link: Link::over_socket(lines),
        incoming: Incoming {
            queued: from_leader,
            ended,
            gone,
        },
        media: Media { asks },
        hangup,
    })
}

/// This tab's place on the rendezvous, for as long as it has a connection.
///
/// It answers two questions with one listener, because they are two halves of
/// one fact — where is the tab holding the account — and because splitting
/// them leaves a moment when neither is being asked. Before the connection
/// exists it is waiting for a `Serve` naming this tab's ask; afterwards it is
/// waiting for a `Leading` from a tab that has taken the account, which means
/// the connection it is holding is to a tab that has gone.
struct Meeting {
    channel: BroadcastChannel,
    ask: String,
    /// Where the answer to this tab's ask lands, until it is waited on.
    answered: Option<futures_channel::oneshot::Receiver<()>>,
    /// Held so the browser does not collect it, and taken off the channel
    /// before it is dropped: a `Closure` freed while the browser still holds a
    /// reference is a panic rather than a missed call.
    _heard: Closure<dyn FnMut(MessageEvent)>,
}

impl Drop for Meeting {
    /// The handler comes off before it is dropped: a browser holding a
    /// reference to a freed callback is a crash rather than a missed event.
    fn drop(&mut self) {
        self.channel.set_onmessage(None);
        self.channel.close();
    }
}

impl Meeting {
    /// Start listening. Nothing is said yet.
    fn open(ask: &str, hangup: Hangup) -> Result<Self, String> {
        let channel = BroadcastChannel::new(tabs::RENDEZVOUS)
            .map_err(|e| format!("this browser would not open a channel between tabs: {e:?}"))?;
        let (answered, was_answered) = futures_channel::oneshot::channel::<()>();
        let answered = RefCell::new(Some(answered));

        let want = ask.to_string();
        let asking = channel.clone();
        let heard = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
            let Some(line) = event.data().as_string() else {
                return;
            };
            match Rendezvous::decode(&line) {
                Some(Rendezvous::Serve { ask, .. }) if ask == want => {
                    if let Some(tell) = answered.borrow_mut().take() {
                        let _ = tell.send(());
                    }
                }
                Some(Rendezvous::Leading { .. }) => {
                    // Before this tab is connected, a new leader is one that
                    // was not there to hear the ask a moment ago: asked again
                    // rather than waited out, since the alternative is sitting
                    // through the whole timeout and then trying for a lock the
                    // new leader holds.
                    //
                    // After it is connected, the same message means the
                    // opposite and is the more important of the two: some
                    // *other* tab has taken the account, so the tab this
                    // connection is to has gone. A leader announces once, on
                    // the way up, and a `BroadcastChannel` does not deliver to
                    // the object that posted — so this is never our own
                    // leader, and never a repeat.
                    if answered.borrow().is_some() {
                        log::info!("a tab has taken the account; asking it again");
                        let _ = post_ask(&asking, &want);
                    } else {
                        log::info!("another tab has taken the account; reconnecting to it");
                        hangup.close("another tab has taken the account".to_string());
                    }
                }
                _ => {}
            }
        });
        channel.set_onmessage(Some(heard.as_ref().unchecked_ref()));

        Ok(Self {
            channel,
            ask: ask.to_string(),
            answered: Some(was_answered),
            _heard: heard,
        })
    }

    /// Say what this tab is looking for, and wait for the tab that has it.
    ///
    /// Waited on once, which is what taking the receiver says. The listener
    /// stays behind either way — after this returns it is watching for the
    /// *next* leader, which is the whole reason it was opened before the ask
    /// rather than after the answer.
    async fn ask_and_wait(&mut self) -> Result<(), String> {
        let Some(answered) = self.answered.take() else {
            return Err("this tab has already asked for the account".to_string());
        };
        // The nonce is logged on both sides, which is what makes two consoles
        // readable as one conversation: it is a connection's name and not a
        // secret — see [`nonce`].
        log::info!("asking for the account as {}", self.ask);
        post_ask(&self.channel, &self.ask)
            .map_err(|e| format!("this tab could not ask for the account: {e:?}"))?;
        match deadline(answered, tabs::ANSWER_TIMEOUT_MS).await {
            Some(Ok(())) => {
                log::info!("a tab answered for the account");
                Ok(())
            }
            // Not an error worth a screen. The tab that would have answered
            // has gone, and the caller's next move is to take the account
            // itself.
            _ => Err("no other tab is holding this account".to_string()),
        }
    }
}

/// Say what this tab is looking for.
///
/// Sent more than once on purpose — see the `Leading` arm of the handler
/// above — which is why the leader answers a repeat with the connection it
/// already opened rather than a second one.
fn post_ask(channel: &BroadcastChannel, ask: &str) -> Result<(), wasm_bindgen::JsValue> {
    let Some(line) = (Rendezvous::Ask {
        v: tabs::VERSION,
        ask: ask.to_string(),
    })
    .encode() else {
        return Err(wasm_bindgen::JsValue::from_str(
            "this ask could not be written",
        ));
    };
    channel.post_message(&wasm_bindgen::JsValue::from_str(&line))
}

/// The handler that turns a message from the other tab into a frame or an
/// answer.
fn frame_handler(
    incoming: &Sender<FromTab>,
    answers: &Rc<RefCell<HashMap<u64, Answer>>>,
    ended: &Rc<std::cell::Cell<bool>>,
    gone: &Rc<std::cell::Cell<bool>>,
) -> Closure<dyn FnMut(MessageEvent)> {
    let incoming = incoming.clone();
    let answers = Rc::clone(answers);
    let ended = Rc::clone(ended);
    let gone = Rc::clone(gone);
    Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
        let data = event.data();
        let Some(kind) = string_field(&data, "k") else {
            return;
        };
        match kind.as_str() {
            "line" => {
                let Some(line) = string_field(&data, "s") else {
                    return;
                };
                // Refused rather than queued past the bound, and the
                // connection ends with it: the front end tracks requests
                // against these frames, and failing them all at once lets the
                // views that asked ask again.
                //
                // The ending cannot be queued *here*, though, and that was the
                // bug: a `try_send` that failed for want of room proves there
                // is no room for the next one either, so the frame and the
                // close were both dropped on the floor, the handler went on
                // accepting, and the front end sat on a connection it had no
                // reason to think was broken. Both go out from one task that
                // may wait — one task rather than two, because two race and
                // the ending winning that race is the retained frame lost
                // after all — with the flag raised first so the backlog drains
                // without paying for its media on the way.
                if let Err(TrySendError::Full(FromTab::Line(line))) =
                    incoming.try_send(FromTab::Line(line))
                {
                    if ended.replace(true) {
                        // Already ending: the frame is past the bound and the
                        // close is already on its way.
                        return;
                    }
                    log::error!(
                        "the tab holding this account is sending frames faster than this one \
                         can apply them; ending the connection"
                    );
                    let incoming = incoming.clone();
                    wasm_bindgen_futures::spawn_local(async move {
                        if incoming.send(FromTab::Line(line)).await.is_ok() {
                            let _ = incoming
                                .send(FromTab::Closed(
                                    "this tab fell too far behind the one holding the account"
                                        .to_string(),
                                ))
                                .await;
                        }
                    });
                }
            }
            "media" | "staged" => {
                let Some(id) = number_field(&data, "id") else {
                    return;
                };
                let Some(answer) = answers.borrow_mut().remove(&id) else {
                    return;
                };
                let failed = string_field(&data, "e");
                match answer {
                    Answer::Read(tell) => {
                        let _ = tell.send(match failed {
                            Some(e) => Err(e),
                            None => bytes_field(&data, "b")
                                .ok_or_else(|| "those bytes did not arrive".to_string()),
                        });
                    }
                    Answer::Stage(tell) => {
                        let _ = tell.send(failed.map_or(Ok(()), Err));
                    }
                }
            }
            "bye" => {
                // The flags before the frame, and `deliver` rather than a bare
                // `try_send`, for the two reasons the overflow path has them:
                // a `Closed` lost to a full queue leaves the front end waiting
                // on a connection that has already gone, and a backlog that
                // does not know the connection ended goes on asking a
                // departed tab for media, one per-frame deadline at a time.
                //
                // And this ending is the *other tab's*, which is the half that
                // also gives up on a requested download rather than spending
                // its whole allowance asking a tab that has said goodbye.
                ended.set(true);
                gone.set(true);
                deliver(
                    &incoming,
                    FromTab::Closed(string_field(&data, "e").unwrap_or_else(|| {
                        "the tab holding this account closed the connection".to_string()
                    })),
                );
            }
            _ => {}
        }
    })
}

/// One request, on its way to the tab holding the account.
///
/// Without a terminator: a channel frames its own messages, where the pipe on
/// the other end of the connection needs the newline this does not carry. The
/// serving side adds it back.
fn post_line(frames: &BroadcastChannel, line: &str) -> Result<(), wasm_bindgen::JsValue> {
    let message = js_sys::Object::new();
    set(&message, "k", &"line".into())?;
    set(&message, "s", &line.into())?;
    frames.post_message(&message)
}

/// Ask for a payload, on the terms this frame allows.
fn post_read(
    frames: &BroadcastChannel,
    id: u64,
    key: &str,
    once: bool,
    most: u64,
) -> Result<(), wasm_bindgen::JsValue> {
    let message = js_sys::Object::new();
    set(&message, "k", &"read".into())?;
    set(&message, "id", &(id as f64).into())?;
    set(&message, "key", &key.into())?;
    set(&message, "once", &once.into())?;
    // A `f64` carries every byte count a browser can hold exactly — the
    // integers are exact to 2^53, and a payload is bounded by a linear memory
    // a thousand times smaller than that.
    set(&message, "most", &(most.min(1 << 53) as f64).into())?;
    frames.post_message(&message)
}

/// Hand a payload to the tab that will send it.
///
/// The one direction bytes travel *out* of a follower: a voice note is
/// recorded here and sent by the tab holding the account.
fn post_stage(
    frames: &BroadcastChannel,
    id: u64,
    key: &str,
    bytes: &[u8],
) -> Result<(), wasm_bindgen::JsValue> {
    let message = js_sys::Object::new();
    set(&message, "k", &"stage".into())?;
    set(&message, "id", &(id as f64).into())?;
    set(&message, "key", &key.into())?;
    // Copied into a JS array once here, and structured-cloned once by the
    // browser. A voice note is the one payload this side allocates whole, and
    // this is the same copy the HTTP path makes into a request body.
    set(&message, "b", &js_sys::Uint8Array::from(bytes).into())?;
    frames.post_message(&message)
}

/// Drop a staged payload whose request is never going to run.
///
/// Unanswered, and safe to be: a channel delivers in order, so a discard
/// posted after a stage is handled after it. The HTTP path needs a record of
/// what is in flight because a `DELETE` and a `PUT` are two requests that can
/// land the wrong way round.
fn post_discard(frames: &BroadcastChannel, key: &str) -> Result<(), wasm_bindgen::JsValue> {
    let message = js_sys::Object::new();
    set(&message, "k", &"discard".into())?;
    set(&message, "key", &key.into())?;
    frames.post_message(&message)
}

/// A `setTimeout`, cleared whenever this is dropped.
///
/// A drop guard rather than a cleanup at the end of the race, and the
/// difference is the whole reason the type exists: the futures here *are*
/// dropped mid-await — a media read sits inside `gather`, which sits inside a
/// budget for the whole frame — and a timer still armed when its Rust
/// `Closure` has been freed fires into a dropped closure. The same shape, and
/// the same reason, as the WebSocket's `FetchDeadline`.
struct Timer {
    /// Whatever `setTimeout` answered, kept as it came: a window hands back a
    /// number and some workers hand back an object, and `clearTimeout` wants
    /// the one it gave out. Nothing here looks inside it.
    handle: Option<wasm_bindgen::JsValue>,
    /// Held so the browser does not collect it before it fires, and dropped
    /// with this — after the handle above has been cleared, never before.
    _fire: Closure<dyn FnMut()>,
}

impl Drop for Timer {
    fn drop(&mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        let global = js_sys::global();
        if let Ok(clear) =
            js_sys::Reflect::get(&global, &wasm_bindgen::JsValue::from_str("clearTimeout"))
            && let Ok(clear) = clear.dyn_into::<js_sys::Function>()
        {
            let _ = clear.call1(&global, &handle);
        }
    }
}

/// Race a future against a timer, and say which won.
///
/// `tokio::time` is the row in docs/web.md that says compiling is not the
/// question: its clock is `Instant::now()`, which traps on this target. The
/// browser's own timer is the clock a page has, and it is armed through the
/// global rather than through `window`, so that this works unchanged in the
/// worker the session is one day moving into.
async fn deadline<T>(
    waiting: futures_channel::oneshot::Receiver<T>,
    millis: i32,
) -> Option<Result<T, futures_channel::oneshot::Canceled>> {
    let (fired, was_fired) = futures_channel::oneshot::channel::<()>();
    let fired = RefCell::new(Some(fired));
    let fire = Closure::<dyn FnMut()>::new(move || {
        if let Some(tell) = fired.borrow_mut().take() {
            let _ = tell.send(());
        }
    });
    let global = js_sys::global();
    let handle = js_sys::Reflect::get(&global, &wasm_bindgen::JsValue::from_str("setTimeout"))
        .ok()
        .and_then(|f| f.dyn_into::<js_sys::Function>().ok())
        .and_then(|set_timeout| {
            set_timeout
                .call2(&global, fire.as_ref().unchecked_ref(), &millis.into())
                .ok()
        });

    // Armed and owned before the await, so that a caller dropping this future
    // takes the timer with it.
    let _timer = Timer {
        handle,
        _fire: fire,
    };

    futures_lite::future::or(async { Some(waiting.await) }, async {
        let _ = was_fired.await;
        None
    })
    .await
}

/// A name for one connection.
///
/// Not a secret and not trying to be: a `BroadcastChannel` is same-origin, so
/// everything that could open this name is already the account's own code.
/// What it has to be is *distinct*, so that two tabs asking at the same
/// moment do not land on one channel — hence three draws rather than one,
/// which is more entropy than the number of tabs a browser will ever have.
fn nonce() -> String {
    let mut name = String::with_capacity(24);
    for _ in 0..3 {
        let draw = (js_sys::Math::random() * f64::from(u32::MAX)) as u32;
        name.push_str(&format!("{draw:08x}"));
    }
    name
}
