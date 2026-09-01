//! Native only, and not for want of trying.
//!
//! These drive [`super::serve_client`] over a `tokio::io::duplex` and take the
//! startup lock, so they need `tokio::spawn` — which wants a `Send` future the
//! wasm bridge's state deliberately is not — and a socket path a page has no
//! filesystem for. The web half of this crate has tests of its own that run in
//! a browser; see `plugins/web/tests.rs`.

use oxidezap_ipc::PROTOCOL_VERSION;
use tokio::io::AsyncBufReadExt as _;

use super::accept::{acquire_startup_lock, prepare_state_dir, reject};
use super::handshake::{Attached, check_hello, handshake, read_frame};
use super::requests::handle_request;
use super::*;
use crate::session_bridge::{CommandOutcome, Outbox, SessionCommand};

/// Every request gets exactly one answer, the ones that fail included.
/// A frame that could not be encoded used to be no frame at all, and the
/// view that asked waited on it forever with nothing logged.
#[test]
fn an_answer_that_cannot_be_encoded_is_still_an_answer() {
    let frame = always(
        Some(oxidezap_ipc::RequestId::from(7u64)),
        Err(anyhow::anyhow!("the encoder gave up")),
    )
    .expect("there is always a frame");
    let parsed: serde_json::Value = serde_json::from_str(&frame).expect("valid json");
    assert_eq!(parsed["type"], "error");
    assert_eq!(parsed["error"]["error"], "malformed");
    assert_eq!(parsed["id"], 7);
}

fn hello(protocol: u32, session_events: bool) -> String {
    serde_json::to_string(&ClientRequest::Hello {
        protocol,
        session_events,
        has_window: true,
    })
    .unwrap()
}

#[test]
fn a_matching_hello_is_accepted() {
    assert_eq!(
        check_hello(&hello(PROTOCOL_VERSION, false)),
        Ok(Attached {
            session_events: false,
            has_window: true
        })
    );
}

/// Whether there is a window to raise is the client's to say, and a
/// client that says nothing is one: every client today is a front end,
/// and a build predating the field is likelier than a headless tool. See
/// [`ClientRequest::Hello`].
#[test]
fn a_client_is_a_window_unless_it_says_otherwise() {
    let silent = format!(r#"{{"request":"hello","protocol":{PROTOCOL_VERSION}}}"#);
    assert_eq!(
        check_hello(&silent),
        Ok(Attached {
            session_events: false,
            has_window: true
        })
    );

    let watcher =
        format!(r#"{{"request":"hello","protocol":{PROTOCOL_VERSION},"has_window":false}}"#);
    assert_eq!(
        check_hello(&watcher),
        Ok(Attached {
            session_events: false,
            has_window: false
        })
    );
}

/// The session stream is opt-in: a tray that never asked must not be sent
/// every message in the account.
#[test]
fn the_session_stream_is_only_served_when_asked_for() {
    assert_eq!(
        check_hello(&hello(PROTOCOL_VERSION, true)),
        Ok(Attached {
            session_events: true,
            has_window: true
        })
    );
    // An older client that does not know the field at all still connects,
    // and gets summaries.
    let line = format!(r#"{{"request":"hello","protocol":{PROTOCOL_VERSION}}}"#);
    assert_eq!(
        check_hello(&line),
        Ok(Attached {
            session_events: false,
            has_window: true
        })
    );
}

/// A client speaking another version must be turned away before it is
/// handed a snapshot it cannot parse, and before the daemon acts on
/// commands it may be misreading.
#[test]
fn a_mismatched_hello_is_rejected_with_both_versions() {
    let rejection =
        check_hello(&hello(PROTOCOL_VERSION + 1, false)).expect_err("a mismatch is turned away");
    let reply: DaemonMessage = serde_json::from_str(&rejection.unwrap()).unwrap();
    match reply {
        DaemonMessage::Error {
            error: ProtocolError::VersionMismatch { client, daemon },
            ..
        } => {
            assert_eq!(client, PROTOCOL_VERSION + 1);
            assert_eq!(daemon, PROTOCOL_VERSION);
        }
        other => panic!("expected a version mismatch, got {other:?}"),
    }
}

#[test]
fn state_is_not_served_before_a_hello() {
    let line = serde_json::to_string(&ClientRequest::Snapshot).unwrap();
    let rejection = check_hello(&line).expect_err("anything else is turned away");
    let reply: DaemonMessage = serde_json::from_str(&rejection.unwrap()).unwrap();
    assert!(matches!(
        reply,
        DaemonMessage::Error {
            error: ProtocolError::Malformed { .. },
            ..
        }
    ));
}

/// A connected session: the state a command is allowed to run against.
fn connected_hub() -> Arc<StateHub> {
    let hub = StateHub::new();
    hub.apply(crate::state::Change::live(
        oxidezap_ipc::DaemonEvent::ConnectionChanged(oxidezap_ipc::ConnectionState::Connected),
    ));
    hub
}

fn bare(request: ClientRequest) -> Request {
    Request::bare(request)
}

/// A connection's own answer channel. Tests that do not read it only
/// need it to exist.
fn outbox() -> Outbox {
    tokio::sync::mpsc::channel(OUTBOX_CAPACITY).0
}

/// A host with nothing loaded, for the requests that are not about
/// plugins — which is every request but one.
fn no_plugins() -> Arc<oxidezap_plugin_host::Plugins> {
    Arc::new(oxidezap_plugin_host::Plugins::nothing_loaded(Arc::new(
        |_| {},
    )))
}

fn parse(frame: Option<String>) -> DaemonMessage {
    serde_json::from_str(&frame.expect("every request gets an answer")).unwrap()
}

/// A stand-in bridge: takes one command and answers it. The join handle
/// yields what it was asked to do, so a test can assert on both halves.
fn bridge(outcome: CommandOutcome) -> (Commands, tokio::task::JoinHandle<Option<Action>>) {
    let (tx, mut rx) = tokio::sync::mpsc::channel(MAX_CLIENTS);
    let task = tokio::spawn(async move {
        let SessionCommand { action, reply } = rx.recv().await?;
        let _ = reply.send(outcome);
        Some(action)
    });
    (tx, task)
}

/// The follow-up this replaced: these parsed and were answered
/// `Unsupported`. They now reach the session.
#[tokio::test]
async fn a_command_reaches_the_session_rather_than_being_refused() {
    let hub = connected_hub();
    let (commands, taken) = bridge(CommandOutcome::Accepted);

    let request = bare(ClientRequest::SendText(oxidezap_ipc::SendText {
        jid: "a@s.whatsapp.net".into(),
        text: "hi".into(),
        local_id: None,
        quoted: None,
    }));
    let answer = handle_request(request, &hub, &no_plugins(), &commands, &outbox()).await;
    assert!(matches!(
        parse(answer.frame),
        DaemonMessage::Accepted { .. }
    ));
    assert!(!answer.shutdown);
    assert!(matches!(
        taken.await.unwrap(),
        Some(Action::SendText(oxidezap_ipc::SendText { jid, text, .. }))
            if jid == "a@s.whatsapp.net" && text == "hi"
    ));
}

/// The payload moves rather than being unpacked and rebuilt, so what the
/// session is handed is the struct the client sent. A field dropped between
/// the two compiles and arrives as a document called "file" with no type on
/// it, which is why this asserts on the fields rather than on the variant.
#[tokio::test]
async fn a_picked_file_reaches_the_session_as_it_was_described() {
    let hub = connected_hub();
    let (commands, taken) = bridge(CommandOutcome::Accepted);

    // Every optional field carries a value, because the ones that default to
    // `None` are exactly the ones a handler can drop without failing anything.
    let sent = oxidezap_ipc::SendMedia {
        jid: "a@s.whatsapp.net".into(),
        upload: "u-local-1".into(),
        kind: oxidezap_core::OutgoingMedia::Image,
        mime_type: "image/jpeg".into(),
        file_name: "praia.jpg".into(),
        caption: Some("olha isso".into()),
        local_id: Some("local-1".into()),
        quoted: Some(oxidezap_core::QuotedMessage {
            message_id: "3EB0A".into(),
            sender: "b@s.whatsapp.net".into(),
            sender_name: "quem quer que seja".into(),
            preview: "a linha citada".into(),
            kind: None,
        }),
    };
    let answer = handle_request(
        bare(ClientRequest::SendMedia(sent.clone())),
        &hub,
        &no_plugins(),
        &commands,
        &outbox(),
    )
    .await;
    assert!(matches!(
        parse(answer.frame),
        DaemonMessage::Accepted { .. }
    ));
    // Compared whole rather than field by field: the payload *moves*, so what
    // this is really asserting is that nothing was rebuilt on the way — and a
    // field added later is covered without anybody remembering to add it here.
    assert_eq!(taken.await.unwrap().map(describe), Some(sent));
}

/// The media send inside an action, or nothing. A helper rather than a
/// `matches!`, so the assertion above can be an equality.
fn describe(action: Action) -> oxidezap_ipc::SendMedia {
    match action {
        Action::SendMedia(request) => request,
        other => panic!("that is not a media send: {other:?}"),
    }
}

/// `Accepted` has to mean the session took it, not that a queue did. The
/// account can drop between the check at the door and the moment the
/// bridge picks the command up, and a client told yes on admission alone
/// would never learn its message went nowhere.
#[tokio::test]
async fn a_refusal_at_execution_time_reaches_the_client() {
    // Connected as far as this connection can see, and refused anyway:
    // exactly the race the answer channel exists for.
    let hub = connected_hub();
    let (commands, _taken) = bridge(CommandOutcome::Refused("has moved on".into()));

    let request = bare(ClientRequest::MarkRead(oxidezap_ipc::MarkRead {
        jid: "a@s.whatsapp.net".into(),
        through_message_id: None,
    }));
    let answer = handle_request(request, &hub, &no_plugins(), &commands, &outbox()).await;
    assert!(matches!(
        parse(answer.frame),
        DaemonMessage::Error {
            error: ProtocolError::Refused { ref detail },
            ..
        } if detail == "has moved on"
    ));
}

/// The other way a command can come back: the account went away while the
/// request was in the bridge's hands. A different answer, because a client
/// can see that state coming and wait it out rather than change anything.
#[tokio::test]
async fn a_session_lost_mid_command_is_reported_as_such() {
    let hub = connected_hub();
    let (commands, _taken) = bridge(CommandOutcome::NoSession("not connected".into()));

    let request = bare(ClientRequest::SendText(oxidezap_ipc::SendText {
        jid: "a@s.whatsapp.net".into(),
        text: "hi".into(),
        local_id: None,
        quoted: None,
    }));
    let answer = handle_request(request, &hub, &no_plugins(), &commands, &outbox()).await;
    assert!(matches!(
        parse(answer.frame),
        DaemonMessage::Error {
            error: ProtocolError::NoSession { .. },
            ..
        }
    ));
}

/// Accepting a send the account cannot carry out would answer `Accepted`
/// and then fail out of sight, where the client can never learn of it.
#[tokio::test]
async fn a_command_is_refused_while_there_is_no_session_to_carry_it() {
    // Fresh hub: `Connecting`, which is what a daemon looks like before it
    // has an account and after it loses one.
    let hub = StateHub::new();
    let (commands, taken) = bridge(CommandOutcome::Accepted);

    let request = bare(ClientRequest::SendText(oxidezap_ipc::SendText {
        jid: "a@s.whatsapp.net".into(),
        text: "hi".into(),
        local_id: None,
        quoted: None,
    }));
    let answer = handle_request(request, &hub, &no_plugins(), &commands, &outbox()).await;
    assert!(matches!(
        parse(answer.frame),
        DaemonMessage::Error {
            error: ProtocolError::NoSession { .. },
            ..
        }
    ));

    drop(commands);
    assert!(
        taken.await.unwrap().is_none(),
        "nothing was queued behind the no"
    );
}

/// The acknowledgement has to be on the wire before the daemon is asked
/// to stop, or the shutdown races the answer and a client that asked
/// politely sees EOF where the protocol promised it a reply. Signalling
/// is therefore the caller's job, after the write.
#[tokio::test]
async fn a_shutdown_is_acknowledged_before_it_is_carried_out() {
    let hub = connected_hub();
    let (commands, _taken) = bridge(CommandOutcome::Accepted);

    let answer = handle_request(
        bare(ClientRequest::Shutdown),
        &hub,
        &no_plugins(),
        &commands,
        &outbox(),
    )
    .await;
    assert!(matches!(
        parse(answer.frame),
        DaemonMessage::Accepted { .. }
    ));
    assert!(answer.shutdown, "and only then is the daemon asked to stop");
}

/// A frame that does not parse is the client's bug, not a reason to drop
/// its connection: it gets told, and its next request still works. Driven
/// through the connection, because parsing is what the connection does —
/// `handle_request` is handed a request that already parsed.
#[tokio::test]
async fn a_malformed_frame_is_answered_and_does_not_end_the_connection() {
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let hub = connected_hub();
    let (commands, _taken) = bridge(CommandOutcome::Accepted);

    let served = tokio::spawn(serve_client(
        server,
        Arc::clone(&hub),
        no_plugins(),
        commands,
    ));
    client
        .write_all(format!("{}\n", hello(PROTOCOL_VERSION, false)).as_bytes())
        .await
        .unwrap();

    let mut reader = BufReader::new(client);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap(); // the snapshot

    reader
        .get_mut()
        .write_all(b"not json at all\n")
        .await
        .unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(
        matches!(
            serde_json::from_str::<DaemonMessage>(&line).unwrap(),
            DaemonMessage::Error {
                error: ProtocolError::Malformed { .. },
                ..
            }
        ),
        "expected a complaint, got {line}"
    );

    // Still usable: the next request is answered rather than the
    // connection being gone.
    let snapshot = serde_json::to_string(&Request::bare(ClientRequest::Snapshot)).unwrap();
    reader
        .get_mut()
        .write_all(format!("{snapshot}\n").as_bytes())
        .await
        .unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(matches!(
        serde_json::from_str::<DaemonMessage>(&line).unwrap(),
        DaemonMessage::Hello { .. }
    ));
    served.abort();
}

/// The daemon owns no window, so this is a relay: whoever has one is the
/// only party that can raise it.
#[tokio::test]
async fn a_window_request_is_published_rather_than_acted_on() {
    let hub = StateHub::new();
    let (commands, taken) = bridge(CommandOutcome::Accepted);
    let mut signals = hub.subscribe_signals();

    let request = bare(ClientRequest::ShowWindow);
    let answer = handle_request(request, &hub, &no_plugins(), &commands, &outbox()).await;
    assert!(matches!(
        parse(answer.frame),
        DaemonMessage::Accepted { .. }
    ));

    let frame: DaemonMessage = serde_json::from_str(&signals.recv().await.unwrap()).unwrap();
    assert_eq!(frame, DaemonMessage::ShowWindow);

    drop(commands);
    assert!(
        taken.await.unwrap().is_none(),
        "the session has no part in a window"
    );
}

/// The cap is per frame, not per connection: a long-lived client sending
/// small valid requests must never hit an artificial EOF because they
/// added up.
#[tokio::test]
async fn the_size_cap_applies_to_each_frame_separately() {
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let mut reader = BufReader::new(server);
    let mut buf = Vec::new();

    // More total bytes than the cap, in frames far below it.
    let frame = "x".repeat(1000);
    let frames = (oxidezap_ipc::MAX_REQUEST_BYTES / 1000) + 10;
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt as _;
        for _ in 0..frames {
            let _ = client.write_all(frame.as_bytes()).await;
            let _ = client.write_all(b"\n").await;
        }
    });

    for i in 0..frames {
        match read_frame(&mut reader, &mut buf).await {
            Ok(Some(oxidezap_ipc::FrameRead::Line(line))) => assert_eq!(line.len(), 1000),
            other => panic!("frame {i} of {frames} was cut short: {other:?}"),
        }
    }
}

/// A frame that is not text is the client's bug, and it can recover from
/// being told. Dropping the connection would take its valid requests with
/// it.
#[tokio::test]
async fn invalid_utf8_is_a_recoverable_frame_not_a_dead_connection() {
    let (mut client, server) = tokio::io::duplex(1024);
    let mut reader = BufReader::new(server);
    let mut buf = Vec::new();

    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt as _;
        let _ = client.write_all(&[0xff, 0xfe, b'\n']).await;
        let _ = client.write_all(b"{\"request\":\"snapshot\"}\n").await;
    });

    assert!(matches!(
        read_frame(&mut reader, &mut buf).await,
        Ok(Some(oxidezap_ipc::FrameRead::NotUtf8))
    ));
    // The stream survives it.
    assert!(matches!(
        read_frame(&mut reader, &mut buf).await,
        Ok(Some(oxidezap_ipc::FrameRead::Line(_)))
    ));
}

/// The reader is a `select!` branch, so it loses races with the update
/// stream mid-frame. What it already consumed has to survive that, or a
/// client's command comes back as a parse error for a frame it sent
/// correctly — and only when the account happened to be busy.
#[tokio::test]
async fn a_frame_interrupted_by_an_update_is_not_lost() {
    let (mut client, server) = tokio::io::duplex(1024);
    let mut reader = BufReader::new(server);
    let mut buf = Vec::new();

    client.write_all(b"{\"request\":\"snap").await.unwrap();

    // The read polls first, consumes what is there, and then parks
    // because the frame is not finished; the ready branch wins.
    tokio::select! {
        biased;
        frame = read_frame(&mut reader, &mut buf) => {
            panic!("an unterminated frame must not complete: {frame:?}");
        }
        () = std::future::ready(()) => {}
    }
    assert!(!buf.is_empty(), "the prefix was consumed and kept");

    client.write_all(b"shot\"}\n").await.unwrap();
    match read_frame(&mut reader, &mut buf).await {
        Ok(Some(oxidezap_ipc::FrameRead::Line(line))) => {
            assert!(
                matches!(
                    serde_json::from_str::<ClientRequest>(&line),
                    Ok(ClientRequest::Snapshot)
                ),
                "the frame reassembled as it was sent: {line}"
            );
        }
        other => panic!("the prefix was dropped: {other:?}"),
    }
}

/// The cap covers a frame, not a read. Letting a carried prefix start the
/// budget over would make the limit a suggestion: a client could send a
/// megabyte at a time forever and never trip it.
#[tokio::test]
async fn a_carried_prefix_still_counts_against_the_cap() {
    let (client, server) = tokio::io::duplex(1024);
    let mut reader = BufReader::new(server);
    // As if a cancelled read had already consumed a full frame's worth.
    let mut buf = vec![b'x'; oxidezap_ipc::MAX_REQUEST_BYTES];

    assert!(matches!(
        read_frame(&mut reader, &mut buf).await,
        Ok(Some(oxidezap_ipc::FrameRead::TooLong))
    ));
    assert!(buf.is_empty(), "a refused frame leaves nothing behind");
    drop(client);
}

/// An encoding bug in the opening frame is as recoverable as one after it.
/// Closing on it silently leaves the client unable to tell a rejected
/// hello from a dead socket.
#[tokio::test]
async fn a_hello_that_is_not_text_is_answered_rather_than_dropped() {
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let (reader, mut writer) = tokio::io::split(server);
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();

    let hello = hello(PROTOCOL_VERSION, false);
    client.write_all(&[0xff, 0xfe, b'\n']).await.unwrap();
    client.write_all(hello.as_bytes()).await.unwrap();
    client.write_all(b"\n").await.unwrap();

    assert!(
        handshake(&mut reader, &mut writer, &mut buf)
            .await
            .unwrap()
            .is_some(),
        "the client recovered and was let in"
    );

    let mut answer = String::new();
    BufReader::new(client).read_line(&mut answer).await.unwrap();
    assert!(matches!(
        serde_json::from_str::<DaemonMessage>(&answer).unwrap(),
        DaemonMessage::Error {
            error: ProtocolError::Malformed { .. },
            ..
        }
    ));
}

/// The regression this replaced: a summary-only client was handed its
/// snapshot and then dropped, because the branch serving session events
/// ended the connection on a closed channel and opting out produced one.
#[tokio::test]
async fn a_client_that_wants_only_summaries_stays_connected() {
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let hub = connected_hub();
    let (commands, _taken) = bridge(CommandOutcome::Accepted);

    let served = tokio::spawn(serve_client(
        server,
        Arc::clone(&hub),
        no_plugins(),
        commands,
    ));
    client
        .write_all(format!("{}\n", hello(PROTOCOL_VERSION, false)).as_bytes())
        .await
        .unwrap();

    let mut reader = BufReader::new(client);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert!(
        matches!(
            serde_json::from_str::<DaemonMessage>(&line).unwrap(),
            DaemonMessage::Hello { .. }
        ),
        "expected a snapshot, got {line}"
    );

    // Still there: a summary reaches it rather than an EOF.
    hub.apply(crate::state::Change::live(
        oxidezap_ipc::DaemonEvent::ChatRemoved {
            jid: "a@s.whatsapp.net".into(),
        },
    ));
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(
        matches!(
            serde_json::from_str::<DaemonMessage>(&line).unwrap(),
            DaemonMessage::Update { .. }
        ),
        "the connection was dropped instead: {line}"
    );
    served.abort();
}

/// The other half: a client that asked for events gets them, and the
/// summary stream keeps working alongside.
#[tokio::test]
async fn a_client_that_asked_for_events_receives_them() {
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let hub = connected_hub();
    let (commands, _taken) = bridge(CommandOutcome::Accepted);

    let served = tokio::spawn(serve_client(
        server,
        Arc::clone(&hub),
        no_plugins(),
        commands,
    ));
    client
        .write_all(format!("{}\n", hello(PROTOCOL_VERSION, true)).as_bytes())
        .await
        .unwrap();

    let mut reader = BufReader::new(client);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap(); // the hello

    hub.publish_session(
        serde_json::to_string(&DaemonMessage::Session {
            event: Box::new(oxidezap_core::UiEvent::Connected),
        })
        .unwrap(),
    );
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(
        matches!(
            serde_json::from_str::<DaemonMessage>(&line).unwrap(),
            DaemonMessage::Session { .. }
        ),
        "expected a session event, got {line}"
    );
    served.abort();
}

/// Forgetting the session is the only way out of dead credentials, and
/// dead credentials are a state the account is unreachable in. Gating it
/// on a connection refuses it exactly when it is wanted.
#[test]
fn the_local_actions_do_not_need_a_connection() {
    assert!(!Action::ForgetSession.needs_network());
    assert!(!Action::ReloadHistory.needs_network());
    // A view is one local row and no stanza, over history a disconnected
    // window can still read — and the ring it watched is already drawn.
    assert!(
        !Action::MarkStatusWatched(oxidezap_ipc::MarkStatusWatched {
            message_ids: vec!["3EB0".into()],
        })
        .needs_network()
    );
    assert!(
        Action::SendText(oxidezap_ipc::SendText {
            jid: "a@s.whatsapp.net".into(),
            text: "hi".into(),
            local_id: None,
            quoted: None,
        })
        .needs_network()
    );
}

/// A refused command names the request it refused. Before ids, the only
/// way to report a refused send was to invent a failure against the
/// message the client happened to have drawn.
#[tokio::test]
async fn a_refusal_names_the_request_it_refused() {
    // Not connected, so the send is refused at the door.
    let hub = StateHub::new();
    let (commands, _taken) = bridge(CommandOutcome::Accepted);

    let request = Request {
        id: Some(42),
        request: ClientRequest::SendText(oxidezap_ipc::SendText {
            jid: "a@s.whatsapp.net".into(),
            text: "hi".into(),
            local_id: Some("local_1".into()),
            quoted: None,
        }),
    };
    let answer = handle_request(request, &hub, &no_plugins(), &commands, &outbox()).await;
    assert!(matches!(
        parse(answer.frame),
        DaemonMessage::Error {
            id: Some(42),
            error: ProtocolError::NoSession { .. },
        }
    ));
}

/// A peer that connects and says nothing costs a task and a descriptor
/// for as long as it likes. A reconnect loop doing it takes the listener
/// down, and the daemon treats a dead listener as fatal.
#[tokio::test(start_paused = true)]
async fn a_client_that_never_speaks_does_not_hold_its_slot_forever() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let hub = StateHub::new();
    let (commands, _taken) = bridge(CommandOutcome::Accepted);

    // Returns rather than parking forever; the paused clock reaches the
    // handshake deadline as soon as nothing else can run.
    serve_client(server, hub, no_plugins(), commands)
        .await
        .unwrap();

    let mut answer = String::new();
    BufReader::new(client).read_line(&mut answer).await.unwrap();
    assert!(
        matches!(
            serde_json::from_str::<DaemonMessage>(&answer).unwrap(),
            DaemonMessage::Error {
                error: ProtocolError::Malformed { .. },
                ..
            }
        ),
        "and it is told why: {answer}"
    );
}

/// A client turned away has to be able to tell "full" from "broken", or
/// it retries against a daemon that will keep refusing it.
#[tokio::test]
async fn a_refused_client_is_told_the_daemon_is_full() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    reject(server).await;

    let mut answer = String::new();
    BufReader::new(client).read_line(&mut answer).await.unwrap();
    assert!(matches!(
        serde_json::from_str::<DaemonMessage>(&answer).unwrap(),
        DaemonMessage::Error {
            error: ProtocolError::TooManyClients { limit },
            ..
        } if limit == MAX_CLIENTS
    ));
}

/// Two daemons starting together can both see a stale socket; the lock is
/// what stops the second from unlinking the first's freshly bound one.
///
/// # Why the release is waited for rather than asserted outright
///
/// `flock` is released when the *last* descriptor on the open file
/// description closes, and a `fork` anywhere in this process duplicates
/// every one of them: between the fork and the exec that clears them,
/// a child holds a copy of this lock and closing ours releases nothing.
/// Measured outside the suite at ~5% of attempts against a single
/// spawning thread, and it is what failed this test on macOS while Linux
/// got away with it.
///
/// [`crate::one_at_a_time`] keeps this away from the tests that spawn,
/// which is worth doing on its own — but it cannot cover a fork this
/// crate does not make, and a test that fails when some library forks
/// beside it is testing the wrong thing. The property is that the lock
/// does not *outlive its holder*: a copy in a child that is microseconds
/// from exec is not the holder, so the wait is what separates the two.
/// A lock genuinely never released still fails, which is the bug this
/// test exists for.
#[test]
fn the_startup_lock_is_exclusive() {
    let _exclusive = crate::one_at_a_time();
    let dir = std::env::temp_dir().join(format!("oxidezap-lock-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("daemon.sock");

    let first = acquire_startup_lock(&socket).expect("first daemon takes the lock");
    assert!(
        acquire_startup_lock(&socket).is_err(),
        "a second daemon must not get in"
    );

    // Released with the handle, so a restart is not blocked by the last
    // run.
    //
    // Retried, and not because the property is doubtful: on an idle
    // machine the first attempt succeeds and this loop never sleeps. It
    // is here because the immediate assertion failed twice on the macOS
    // runner and nowhere else, which a single attempt reports as "the
    // lock outlived its holder" — a claim about this code that the
    // evidence does not support, since re-acquiring works everywhere it
    // can be reproduced.
    //
    // The likeliest mechanism is that a `flock` belongs to the *open file
    // description*, which `fork` duplicates: a child spawned by another
    // test in this binary (`window::tests::launching` starts a shell that
    // sleeps) holds this descriptor from the moment it is forked until it
    // execs, so dropping the handle here releases nothing until it does.
    // That is a hypothesis — it did not reproduce under load here — which
    // is why this waits for the lock rather than asserting anything about
    // why it was briefly unavailable. What it still refuses is a lock
    // that is never released.
    drop(first);
    // The library's clock, which is what this repo uses everywhere: a
    // test that moved time would move this with it.
    let deadline = wacore::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut last = None;
    let regained = loop {
        match acquire_startup_lock(&socket) {
            Ok(lock) => break Some(lock),
            Err(e) if wacore::time::Instant::now() < deadline => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => {
                last = Some(e);
                break None;
            }
        }
    };
    assert!(
        regained.is_some(),
        "lock outlived its holder: {}",
        last.map_or_else(|| "no reason given".to_string(), |e| e.to_string())
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The fallback directory sits at a predictable path in a world-writable
/// place, so a symlink planted there must not be followed.
///
/// Unix only, and not for want of porting: on Windows the state directory
/// is under the user's own profile, so there is no world-writable parent
/// for anyone to plant anything in, and `prepare_state_dir` has nothing
/// to check.
#[cfg(unix)]
#[test]
fn a_symlinked_socket_dir_is_refused() {
    let base = std::env::temp_dir().join(format!("oxidezap-symlink-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();

    let target = base.join("elsewhere");
    std::fs::create_dir_all(&target).unwrap();
    let link = base.join("sockdir");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let err = prepare_state_dir(&link).expect_err("a symlink must be refused");
    assert!(
        err.to_string().contains("not a directory"),
        "unexpected reason: {err}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

/// A directory we already own is reused, and tightened if it is loose.
///
/// Unix only, for the same reason as the symlink check above.
#[cfg(unix)]
#[test]
fn a_loose_but_owned_dir_is_tightened_rather_than_refused() {
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir().join(format!("oxidezap-loose-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();

    prepare_state_dir(&dir).expect("our own directory is usable");

    let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "left readable by other users");

    let _ = std::fs::remove_dir_all(&dir);
}
