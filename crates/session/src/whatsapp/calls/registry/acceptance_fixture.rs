//! Real outgoing builders and injected production library events, with no media relay.

use super::*;
use oxidezap_call_fixture::CallFixture;
use whatsapp_rust::wacore_binary::{Node, builder::NodeBuilder};

#[derive(Clone, Copy, Debug)]
pub enum OutgoingAcceptCase {
    Video,
    EarlyVideo,
    Audio,
    InvalidOrientation,
    MissingOrientation,
    ExplicitOff,
    Sibling,
    SiblingOff,
    SiblingOffAfter,
    WinnerOffAfter,
    CameraClosed,
    CameraReplaced,
    Ended,
    PhoneTarget,
    WrongUser,
    Flood,
}

impl OutgoingAcceptCase {
    pub const ALL: [Self; 16] = [
        Self::Video,
        Self::EarlyVideo,
        Self::Audio,
        Self::InvalidOrientation,
        Self::MissingOrientation,
        Self::ExplicitOff,
        Self::Sibling,
        Self::SiblingOff,
        Self::SiblingOffAfter,
        Self::WinnerOffAfter,
        Self::CameraClosed,
        Self::CameraReplaced,
        Self::Ended,
        Self::PhoneTarget,
        Self::WrongUser,
        Self::Flood,
    ];
    pub fn connects(self) -> bool {
        !matches!(self, Self::Ended | Self::WrongUser)
    }
    pub fn remote_expected(self) -> bool {
        matches!(
            self,
            Self::Video
                | Self::EarlyVideo
                | Self::InvalidOrientation
                | Self::MissingOrientation
                | Self::Sibling
                | Self::SiblingOff
                | Self::SiblingOffAfter
                | Self::PhoneTarget
                | Self::Flood
        )
    }
}

fn stanza(
    fixture: &CallFixture,
    id: &str,
    device: u16,
    tag: &'static str,
    video: Option<Node>,
) -> Node {
    NodeBuilder::new("call")
        .attr("from", fixture.peer().clone().with_device(device))
        .attr("id", format!("{tag}-{device}"))
        .attr("t", "1788840000")
        .children([NodeBuilder::new(tag)
            .attr("call-id", id)
            .attr("call-creator", fixture.client().lid().unwrap())
            .children(video)
            .build()])
        .build()
}

fn video_state(fixture: &CallFixture, id: &str, device: u16) -> Node {
    NodeBuilder::new("call")
        .attr("from", fixture.peer().clone().with_device(device))
        .attr("id", format!("stopped-{device}"))
        .attr("t", "1788840000")
        .children([NodeBuilder::new("video")
            .attr("call-id", id)
            .attr("call-creator", fixture.client().lid().unwrap())
            .attr("state", "6")
            .build()])
        .build()
}

async fn dispatch(
    fixture: &CallFixture,
    cursor: &mut usize,
    calls: &CallRegistry,
    ui: &UiEventSender,
) {
    let events = fixture.events().unwrap();
    let relevant: Vec<_> = events[*cursor..]
        .iter()
        .filter(|event| {
            matches!(&***event, Event::IncomingCall(_))
                || matches!(&***event, Event::RawNode(node)
            if node.get().tag == "call" && node.get().get_optional_child("accept").is_some())
        })
        .cloned()
        .collect();
    let (_stop, stopping) = tokio::sync::watch::channel(());
    let (processed, done) = async_channel::bounded(relevant.len().max(1));
    let names = Arc::new(NameBook::new(None));
    let mut lanes = crate::whatsapp::lanes::EventLanes::new(
        {
            let client = fixture.client().clone();
            let calls = calls.clone();
            let ui = ui.clone();
            let names = names.clone();
            move |event| {
                let client = client.clone();
                let calls = calls.clone();
                let ui = ui.clone();
                let names = names.clone();
                let processed = processed.clone();
                async move {
                    WhatsAppClient::handle_event(event, client, ui, calls, names).await;
                    processed.send(()).await.unwrap();
                }
            }
        },
        stopping,
    );
    let count = relevant.len();
    for event in relevant {
        lanes.dispatch(fixture.client(), &names, event).await;
    }
    for _ in 0..count {
        done.recv().await.unwrap();
    }
    *cursor = events.len();
}

pub async fn outgoing_accept_events(case: OutgoingAcceptCase) -> Vec<UiEvent> {
    let fixture = Arc::new(CallFixture::new().await.unwrap());
    let _raw = fixture.client().acquire_raw_node_forwarding();
    let calls = CallRegistry::default();
    let starting_guard = StartGuard {
        stamp: calls.begin_start("placeholder"),
        calls: calls.clone(),
        placeholder: "placeholder".into(),
    };
    let (ui, mut events) = ui_queue::channel(
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(ui_queue::HistoryBudget::new()),
    );
    let (local, endpoints, _capture) = video::camera_fixture("placeholder", Arc::new(|_, _| {}));
    let camera = local.camera_id();
    let (_mic, mic_rx) = async_channel::bounded::<Vec<i16>>(1);
    let (speaker, _speaker_rx) = async_channel::bounded::<Vec<i16>>(1);
    let requested = if matches!(case, OutgoingAcceptCase::PhoneTarget) {
        fixture
            .client()
            .add_lid_pn_mapping(
                fixture.peer().user.as_ref(),
                "15550003333",
                whatsapp_rust::lid_pn_cache::LearningSource::Usync,
            )
            .await
            .unwrap();
        Jid::pn("15550003333")
    } else {
        fixture.peer().clone()
    };
    let starting = tokio::spawn({
        let fixture = fixture.clone();
        let requested = requested.clone();
        async move {
            fixture
                .client()
                .voip()
                .call(&requested)
                .audio(mic_rx, speaker)
                .video(endpoints.source, endpoints.sink)
                .start()
                .await
        }
    });
    let offer = fixture.next_offer().await.unwrap();
    assert_eq!(
        offer.stanza().as_node_ref().attrs().optional_jid("to"),
        Some(fixture.peer().clone())
    );
    let id = offer
        .stanza()
        .as_node_ref()
        .get_optional_child("offer")
        .unwrap()
        .attrs()
        .optional_string("call-id")
        .unwrap()
        .into_owned();
    let advertisement = match case {
        OutgoingAcceptCase::Audio => None,
        OutgoingAcceptCase::MissingOrientation => {
            Some(NodeBuilder::new("video").attr("dec", "H264").build())
        }
        _ => Some(
            NodeBuilder::new("video")
                .attr("dec", "H264")
                .attr(
                    "device_orientation",
                    if matches!(case, OutgoingAcceptCase::InvalidOrientation) {
                        "4"
                    } else {
                        "0"
                    },
                )
                .build(),
        ),
    };
    let accept = if matches!(case, OutgoingAcceptCase::WrongUser) {
        NodeBuilder::new("call")
            .attr("from", Jid::lid("999999999999999").with_device(2))
            .attr("id", "wrong-user-accept")
            .attr("t", "1788840000")
            .children([NodeBuilder::new("accept")
                .attr("call-id", id.clone())
                .attr("call-creator", fixture.client().lid().unwrap())
                .children(advertisement)
                .build()])
            .build()
    } else {
        stanza(&fixture, &id, 2, "accept", advertisement)
    };
    let early = matches!(
        case,
        OutgoingAcceptCase::EarlyVideo | OutgoingAcceptCase::Ended | OutgoingAcceptCase::Flood
    );
    let injection = early.then(|| {
        tokio::spawn({
            let fixture = fixture.clone();
            let accept = accept.clone();
            async move {
                fixture.inject(accept).await.unwrap();
            }
        })
    });
    if early {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while fixture
                .call_snapshot(&id)
                .unwrap()
                .answering_device
                .is_none()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!starting.is_finished());
    }
    offer.complete().unwrap();
    let handle = Arc::new(starting.await.unwrap().unwrap());
    assert_eq!(handle.call_id(), id);
    let target = outgoing_target(fixture.client(), &requested).await.unwrap();
    assert_eq!(target, *fixture.peer());
    if let Some(injection) = injection {
        injection.await.unwrap();
    }
    use whatsapp_rust::futures::{StreamExt, stream::FuturesUnordered};
    let mut flooding = FuturesUnordered::new();
    if matches!(case, OutgoingAcceptCase::Flood) {
        for index in 0..300 {
            fixture
                .inject(stanza(
                    &fixture,
                    &format!("unrelated-{index}"),
                    2,
                    "accept",
                    None,
                ))
                .await
                .unwrap();
        }
        for event in fixture.events().unwrap() {
            if let Event::IncomingCall(call) = &*event
                && call.action.call_id().starts_with("unrelated-")
            {
                let calls = calls.clone();
                let call = call.clone();
                let ui = ui.clone();
                flooding.push(async move { calls.accepted(&call, &ui).await });
            }
        }
        assert_eq!(flooding.len(), 300);
        assert!(whatsapp_rust::futures::poll!(flooding.next()).is_pending());
        assert!(
            calls.calls.lock().unwrap().outgoing.is_empty(),
            "unrelated IDs entered the registry"
        );
    }
    let mut cursor = 0;
    let mut early_dispatch = early.then(|| Box::pin(dispatch(&fixture, &mut cursor, &calls, &ui)));
    if let Some(dispatching) = &mut early_dispatch {
        assert!(
            whatsapp_rust::futures::poll!(dispatching.as_mut()).is_pending(),
            "early event must await registration"
        );
        assert!(events.try_recv().is_err());
    }
    assert!(calls.finish_start("placeholder", &id, &handle));
    local.rename(&id);
    if matches!(case, OutgoingAcceptCase::CameraClosed) {
        local.stop().await;
    } else {
        assert!(calls.hold_camera(&id, local).await == Camera::Held);
    }
    let replacement = if matches!(case, OutgoingAcceptCase::CameraReplaced) {
        calls.take_camera(&id).unwrap().stop().await;
        let (local, endpoints, capture) = video::camera_fixture(&id, Arc::new(|_, _| {}));
        assert!(calls.hold_camera(&id, local).await == Camera::Held);
        Some((endpoints, capture))
    } else {
        None
    };
    let watcher = crate::exec::spawn(WhatsAppClient::run_call_events(
        handle.clone(),
        calls.clone(),
        ui.clone(),
    ));
    if matches!(
        case,
        OutgoingAcceptCase::ExplicitOff | OutgoingAcceptCase::SiblingOff
    ) {
        fixture
            .inject(video_state(
                &fixture,
                &id,
                if matches!(case, OutgoingAcceptCase::SiblingOff) {
                    0
                } else {
                    2
                },
            ))
            .await
            .unwrap();
    }
    if !early {
        fixture.inject(accept.clone()).await.unwrap();
    }
    if matches!(case, OutgoingAcceptCase::Sibling) {
        fixture
            .inject(stanza(
                &fixture,
                &id,
                0,
                "accept",
                Some(NodeBuilder::new("video").attr("dec", "H264").build()),
            ))
            .await
            .unwrap();
    }
    if matches!(case, OutgoingAcceptCase::Ended) {
        calls.ended_remotely(&id);
        calls.announce_ending(&id);
    }
    ui.send(UiEvent::OutgoingCallStarted {
        call_id: id.clone(),
        recipient_jid: requested.to_string(),
        placeholder_id: "placeholder".into(),
        is_video: true,
    })
    .unwrap();
    calls.outgoing_ready(&id, &handle, target, Some(camera));
    drop(starting_guard);
    while flooding.next().await.is_some() {}
    if let Some(dispatching) = early_dispatch.take() {
        dispatching.await;
    }
    drop(early_dispatch);
    dispatch(&fixture, &mut cursor, &calls, &ui).await;
    if matches!(
        case,
        OutgoingAcceptCase::SiblingOffAfter | OutgoingAcceptCase::WinnerOffAfter
    ) {
        fixture
            .inject(video_state(
                &fixture,
                &id,
                if matches!(case, OutgoingAcceptCase::WinnerOffAfter) {
                    2
                } else {
                    0
                },
            ))
            .await
            .unwrap();
        dispatch(&fixture, &mut cursor, &calls, &ui).await;
    }
    fixture.inject(accept).await.unwrap();
    dispatch(&fixture, &mut cursor, &calls, &ui).await;
    handle.events().close();
    tokio::time::timeout(std::time::Duration::from_secs(3), watcher)
        .await
        .unwrap()
        .unwrap();
    if matches!(case, OutgoingAcceptCase::WrongUser) {
        assert_eq!(
            fixture
                .call_snapshot(&id)
                .unwrap()
                .answering_device
                .unwrap()
                .user
                .as_str(),
            "999999999999999"
        );
    } else if !matches!(case, OutgoingAcceptCase::Ended) {
        assert_eq!(handle.peer_jid(), fixture.peer().clone().with_device(2));
    }
    let mut result = Vec::new();
    while let Ok(event) = events.try_recv() {
        result.push(event);
    }
    let announcements = fixture
        .outgoing_stanzas()
        .unwrap()
        .iter()
        .filter(|node| {
            node.as_node_ref()
                .get_optional_child("video")
                .is_some_and(|video| video.attrs().optional_string("state").as_deref() == Some("1"))
        })
        .count();
    assert_eq!(
        announcements,
        usize::from(
            case.connects()
                && !matches!(
                    case,
                    OutgoingAcceptCase::CameraClosed | OutgoingAcceptCase::CameraReplaced
                )
        ),
        "{case:?}: changed standalone Enabled count"
    );
    if let Some(camera) = calls.ended(&id) {
        camera.stop().await;
    }
    drop(replacement);
    fixture.shutdown().await.unwrap();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn delayed_watcher_keeps_upgrade_and_stop_in_source_queue_order() {
        for (accepting_upgrade, pressure, legacy_only, before_activation) in [
            (true, false, false, false),
            (false, false, false, false),
            (true, true, false, false),
            (false, true, false, false),
            (true, false, true, false),
            (false, false, true, false),
            (true, false, false, true),
            (false, false, false, true),
        ] {
            let fixture = Arc::new(CallFixture::new().await.unwrap());
            let calls = CallRegistry::default();
            let guard = StartGuard {
                stamp: calls.begin_start("pending"),
                calls: calls.clone(),
                placeholder: "pending".into(),
            };
            let (ui, mut received) = ui_queue::channel(
                Arc::new(tokio::sync::Notify::new()),
                Arc::new(ui_queue::HistoryBudget::new()),
            );
            let (_mic, mic_rx) = async_channel::bounded::<Vec<i16>>(1);
            let (speaker, _speaker_rx) = async_channel::bounded::<Vec<i16>>(1);
            let starting = tokio::spawn({
                let fixture = fixture.clone();
                async move {
                    fixture
                        .client()
                        .voip()
                        .call(fixture.peer())
                        .audio(mic_rx, speaker)
                        .start()
                        .await
                }
            });
            fixture.next_offer().await.unwrap().complete().unwrap();
            let handle = Arc::new(starting.await.unwrap().unwrap());
            let id = handle.call_id();
            assert!(calls.finish_start("pending", id, &handle));
            calls.outgoing_ready(id, &handle, fixture.peer().clone(), None);
            drop(guard);
            fixture
                .inject(stanza(&fixture, id, 2, "accept", None))
                .await
                .unwrap();
            let mut cursor = 0;
            if !before_activation {
                dispatch(&fixture, &mut cursor, &calls, &ui).await;
            }
            while received.try_recv().is_ok() {}

            let capture = if accepting_upgrade {
                let (local, endpoints, capture) = video::camera_fixture(id, Arc::new(|_, _| {}));
                calls.begin_upgrade(id, local.camera_id());
                handle
                    .start_video(endpoints.source, endpoints.sink)
                    .await
                    .unwrap();
                local.live();
                assert!(calls.hold_camera(id, local).await == Camera::Held);
                Some(capture)
            } else {
                None
            };
            let video = |state: VideoState, sequence: usize| {
                NodeBuilder::new("call")
                    .attr("from", fixture.peer().clone().with_device(2))
                    .attr("id", format!("ordered-{sequence}"))
                    .attr("t", "1788840000")
                    .children([NodeBuilder::new("video")
                        .attr("call-id", id)
                        .attr("call-creator", fixture.client().lid().unwrap())
                        .attr("state", state.code().to_string())
                        .attr("dec", "H264")
                        .build()])
                    .build()
            };
            let queue = handle.events();
            if pressure {
                for index in 0..queue.capacity().unwrap() {
                    fixture
                        .inject(video(VideoState::Paused, index))
                        .await
                        .unwrap();
                }
                assert!(queue.is_full(), "the real handle queue was not pressured");
            }
            fixture
                .inject(video(
                    if accepting_upgrade {
                        VideoState::UpgradeAccept
                    } else {
                        VideoState::UpgradeRequestV2
                    },
                    1000,
                ))
                .await
                .unwrap();
            if legacy_only {
                // Fault injection: retain only the real compatibility companion.
                assert!(matches!(
                    queue.try_recv().unwrap(),
                    CallEvent::PeerVideoStateChanged { .. }
                ));
                assert_eq!(queue.len(), 1);
            } else {
                fixture
                    .inject(video(VideoState::Stopped, 1001))
                    .await
                    .unwrap();
            }
            let mut watching = Box::pin(WhatsAppClient::run_call_events(
                handle.clone(),
                calls.clone(),
                ui.clone(),
            ));
            if before_activation {
                assert!(whatsapp_rust::futures::poll!(watching.as_mut()).is_pending());
                let held = calls.calls.lock().unwrap();
                let pending = held.outgoing[id].peer_video.as_ref().unwrap();
                assert_eq!(pending.source, fixture.peer().clone().with_device(2));
                assert_eq!(pending.upgrade_token.is_some(), !accepting_upgrade);
                assert_eq!(
                    queue.len(),
                    3,
                    "only one complete operation may leave the queue before activation"
                );
            }
            // Normal cases hold the watcher; the last two reverse that scheduling.
            dispatch(&fixture, &mut cursor, &calls, &ui).await;
            let mut global_changes = Vec::new();
            while let Ok(event) = received.try_recv() {
                global_changes.push(event);
            }
            queue.close();
            tokio::time::timeout(std::time::Duration::from_secs(3), watching)
                .await
                .unwrap();
            assert!(queue.is_empty());
            let mut remote = None;
            let mut requested = false;
            while let Ok(event) = received.try_recv() {
                match event {
                    UiEvent::CallVideoChanged {
                        stream: VideoStream::Remote,
                        on,
                        ..
                    } => remote = Some(on),
                    UiEvent::CallVideoRequested { pending, .. } => requested = pending,
                    _ => {}
                }
            }
            assert_eq!(
                remote,
                if legacy_only { None } else { Some(false) },
                "accept={accepting_upgrade}, pressure={pressure}, legacy={legacy_only}"
            );
            assert!(!requested, "a stopped request was resurrected");
            assert_eq!(calls.upgrade_pending(id), accepting_upgrade && legacy_only);
            assert!(!calls.calls.lock().unwrap().upgrades.contains_key(id));
            assert!(
                before_activation || global_changes.is_empty(),
                "global video events still mutate call state"
            );
            if let Some(camera) = calls.ended(id) {
                camera.stop().await;
            }
            drop(capture);
            fixture.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn losing_sibling_state_cannot_override_winning_accept() {
        for case in [
            OutgoingAcceptCase::SiblingOff,
            OutgoingAcceptCase::SiblingOffAfter,
        ] {
            let events = outgoing_accept_events(case).await;
            let remote: Vec<_> = events
                .iter()
                .filter_map(|event| match event {
                    UiEvent::CallVideoChanged {
                        stream: VideoStream::Remote,
                        on,
                        ..
                    } => Some(*on),
                    _ => None,
                })
                .collect();
            assert!(
                !remote.is_empty() && remote.iter().all(|on| *on),
                "{case:?}: {remote:?}"
            );
        }
    }

    #[tokio::test]
    async fn real_call_acceptance_cases() {
        for case in OutgoingAcceptCase::ALL {
            let events = outgoing_accept_events(case).await;
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, UiEvent::CallAccepted(_)))
                    .count(),
                usize::from(case.connects()),
                "{case:?}"
            );
            let remote = events
                .iter()
                .filter_map(|event| match event {
                    UiEvent::CallVideoChanged {
                        stream: VideoStream::Remote,
                        on,
                        ..
                    } => Some(*on),
                    _ => None,
                })
                .next_back()
                .unwrap_or(false);
            assert_eq!(remote, case.remote_expected(), "{case:?}");
        }
    }

    #[tokio::test]
    async fn arbitrary_accept_ids_are_not_admitted_by_an_unrelated_start() {
        let fixture = CallFixture::new().await.unwrap();
        let calls = CallRegistry::default();
        calls.begin_start("placeholder");
        let (ui, _) = ui_queue::channel(
            Arc::new(tokio::sync::Notify::new()),
            Arc::new(ui_queue::HistoryBudget::new()),
        );
        fixture
            .inject(stanza(&fixture, "unrelated", 2, "accept", None))
            .await
            .unwrap();
        let events = fixture.events().unwrap();
        let call = events
            .iter()
            .find_map(|event| match &**event {
                Event::IncomingCall(call) if call.action.call_id() == "unrelated" => {
                    Some(call.clone())
                }
                _ => None,
            })
            .unwrap();
        let mut accepting = std::pin::pin!(calls.accepted(&call, &ui));
        let _ = whatsapp_rust::futures::poll!(accepting.as_mut());
        assert!(calls.calls.lock().unwrap().outgoing.is_empty());
        calls.abandon_start("placeholder");
        tokio::time::timeout(std::time::Duration::from_secs(1), accepting)
            .await
            .unwrap();
        fixture.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn registration_waiters_follow_real_builder_completion_failure_and_cancellation() {
        for ending in ["complete", "fail", "cancel", "drop"] {
            let fixture = Arc::new(CallFixture::new().await.unwrap());
            let calls = CallRegistry::default();
            let guard = StartGuard {
                stamp: calls.begin_start("pending"),
                calls: calls.clone(),
                placeholder: "pending".into(),
            };
            let (_mic, mic_rx) = async_channel::bounded::<Vec<i16>>(1);
            let (speaker, _speaker_rx) = async_channel::bounded::<Vec<i16>>(1);
            let starting = tokio::spawn({
                let fixture = fixture.clone();
                let calls = calls.clone();
                async move {
                    let _guard = guard;
                    let result = fixture
                        .client()
                        .voip()
                        .call(fixture.peer())
                        .audio(mic_rx, speaker)
                        .start()
                        .await;
                    match result {
                        Ok(handle) => {
                            let handle = Arc::new(handle);
                            assert!(calls.finish_start("pending", handle.call_id(), &handle));
                            calls.outgoing_ready(
                                handle.call_id(),
                                &handle,
                                fixture.peer().clone(),
                                None,
                            );
                            Some(handle)
                        }
                        Err(_) => {
                            calls.abandon_start("pending");
                            None
                        }
                    }
                }
            });
            let offer = fixture.next_offer().await.unwrap();
            let id = offer
                .stanza()
                .as_node_ref()
                .get_optional_child("offer")
                .unwrap()
                .attrs()
                .optional_string("call-id")
                .unwrap()
                .into_owned();
            let mut waiter = std::pin::pin!(calls.registered_outgoing(&id));
            assert!(whatsapp_rust::futures::poll!(waiter.as_mut()).is_pending());
            match ending {
                "complete" => {
                    offer.complete().unwrap();
                    let handle = starting.await.unwrap().unwrap();
                    let registered =
                        tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
                            .await
                            .unwrap()
                            .unwrap();
                    assert!(Arc::ptr_eq(&handle, &registered));
                }
                "drop" => {
                    starting.abort();
                    assert!(starting.await.err().unwrap().is_cancelled());
                    let _ = offer.fail();
                    assert!(
                        tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
                            .await
                            .unwrap()
                            .is_none()
                    );
                }
                other => {
                    if other == "cancel" {
                        assert!(matches!(calls.cancel("pending"), Cancelled::Deferred));
                        calls.begin_start("replacement");
                        assert!(
                            tokio::time::timeout(
                                std::time::Duration::from_secs(1),
                                waiter.as_mut()
                            )
                            .await
                            .unwrap()
                            .is_none()
                        );
                        assert!(!starting.is_finished());
                    }
                    offer.fail().unwrap();
                    assert!(starting.await.unwrap().is_none());
                    if other == "fail" {
                        assert!(
                            tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
                                .await
                                .unwrap()
                                .is_none()
                        );
                    }
                }
            }
            fixture.shutdown().await.unwrap();
        }
    }
}
