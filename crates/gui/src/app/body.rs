//! The screen below the call overlay, cached independently of call pictures.

use super::*;

pub(super) struct Body {
    app: WeakEntity<WhatsAppApp>,
    calls: CallState,
}

impl Body {
    pub(super) fn new(
        state: &WhatsAppApp,
        app: Entity<WhatsAppApp>,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&app, |_, _, cx| cx.notify()).detach();
        // These controllers are data entities, not rendered descendants.
        // GPUI cannot propagate their notifications up the dispatch tree.
        cx.observe(&state.pages, |_, _, cx| cx.notify()).detach();
        cx.observe(&state.viewer, |_, _, cx| cx.notify()).detach();
        cx.observe(&state.search, |_, _, cx| cx.notify()).detach();
        cx.observe(&state.recorder, |_, _, cx| cx.notify()).detach();
        cx.observe(&state.recovery, |_, _, cx| cx.notify()).detach();
        cx.observe(&state.plugins, |_, _, cx| cx.notify()).detach();
        cx.observe(&state.settings, |_, _, cx| cx.notify()).detach();
        // Picture-only updates do not change the body's return-to-call banner.
        // Its elapsed label still follows the call timer's root notification.
        cx.observe(&state.calls, |body, calls, cx| {
            let state = calls.read(cx).state();
            if body.calls != *state {
                body.calls = state.clone();
                cx.notify();
            }
        })
        .detach();
        Self {
            app: app.downgrade(),
            calls: state.calls.read(cx).state().clone(),
        }
    }
}

impl Render for Body {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.app
            .update(cx, |app, cx| {
                #[cfg(test)]
                {
                    app.body_renders += 1;
                }
                // Cache hits keep these reports because they keep the same screen.
                app.visible_chat = None;
                app.keyboard_surfaces = KeyboardSurfaces::default();
                let entity = cx.entity();
                match &app.app_state {
                    AppState::Loading => render_loading_view(cx).into_any_element(),
                    AppState::Connecting => render_connecting_view(cx).into_any_element(),
                    AppState::WaitingForPairing { qr_code, pair_code } => {
                        render_pairing_view(qr_code.as_ref(), pair_code.clone(), cx)
                            .into_any_element()
                    }
                    AppState::Syncing => render_syncing_view(cx).into_any_element(),
                    AppState::Connected | AppState::Offline if app.showing_settings(cx) => {
                        render_settings_view(app, window, cx).into_any_element()
                    }
                    AppState::Connected | AppState::Offline => {
                        render_connected_view(app, window, cx).into_any_element()
                    }
                    AppState::Error(fault) => render_error_view(
                        fault,
                        app.retry_countdown(cx),
                        app.error_detail_open(cx),
                        entity,
                        cx,
                    )
                    .into_any_element(),
                    AppState::Refused { reason } => {
                        render_refused_view(reason, app.error_detail_open(cx), entity, cx)
                            .into_any_element()
                    }
                    AppState::LoggedOut { message } => {
                        render_logged_out_view(message, entity, cx).into_any_element()
                    }
                }
            })
            .unwrap_or_else(|_| div().into_any_element())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (
        gpui::HeadlessAppContext,
        gpui::WindowHandle<gpui_component::Root>,
        Entity<WhatsAppApp>,
    ) {
        let mut cx = gpui::HeadlessAppContext::with_asset_source(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("DejaVu Sans")),
            Arc::new(crate::assets::Assets),
        );
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::init(cx);
        });
        let mut app_entity = None;
        let window = cx
            .open_window(gpui::size(gpui::px(1000.), gpui::px(800.)), |window, cx| {
                let app = cx.new(|cx| {
                    let mut app = WhatsAppApp::new(cx);
                    app.app_state = AppState::Offline;
                    app.destination = Destination::Status;
                    app.chats
                        .push(Arc::new(Chat::new("peer@example.invalid".into())));
                    app
                });
                app_entity = Some(app.clone());
                cx.new(|cx| gpui_component::Root::new(app, window, cx))
            })
            .unwrap();
        let app = app_entity.unwrap();
        cx.run_until_parked();
        (cx, window, app)
    }

    fn assert_calls_cached(cx: &mut gpui::HeadlessAppContext, app: &Entity<WhatsAppApp>) {
        // A changed input extent can request one follow-up frame after layout.
        // Measure steady-state calls after that notification has been consumed.
        app.update(cx, |app, cx| {
            app.calls.update(cx, |_, cx| cx.notify());
        });
        let before = cx.update(|cx| app.read(cx).body_renders);
        assert!(before > 0);
        for _ in 0..60 {
            app.update(cx, |app, cx| {
                app.calls.update(cx, |_, cx| cx.notify());
            });
        }
        let after = cx.update(|cx| app.read(cx).body_renders);
        println!("body builds for 60 call notifications: {}", after - before);
        assert_eq!(after, before);
    }

    #[test]
    fn call_notifications_preserve_status_and_settings_body() {
        let (mut cx, window, app) = setup();
        assert_calls_cached(&mut cx, &app);
        window
            .update(&mut cx, |_, window, cx| {
                app.update(cx, |app, cx| app.open_settings(window, cx));
            })
            .unwrap();
        cx.update(|cx| assert_eq!(app.read(cx).keyboard_owner, Some(KeyboardOwner::Screen)));
        assert_calls_cached(&mut cx, &app);
        let before = cx.update(|cx| app.read(cx).body_renders);
        app.update(&mut cx, |app, cx| {
            app.calls.update(cx, |calls, cx| {
                calls.place_outgoing(
                    OutgoingCall::new(
                        "test-call",
                        "peer@example.invalid".into(),
                        "Test peer".into(),
                        false,
                    ),
                    cx,
                );
            });
        });
        cx.update(|cx| {
            assert!(app.read(cx).body_renders > before);
            assert_eq!(
                app.read(cx).keyboard_owner,
                Some(KeyboardOwner::RingingCall("test-call".into()))
            );
            assert!(app.read(cx).keyboard_surfaces.call_card);
        });
        assert_calls_cached(&mut cx, &app);
        app.update(&mut cx, |app, cx| {
            app.calls.update(cx, |calls, cx| {
                calls.end(cx);
            });
        });
        cx.update(|cx| {
            assert_eq!(app.read(cx).keyboard_owner, Some(KeyboardOwner::Screen));
            assert!(!app.read(cx).keyboard_surfaces.call_card);
        });
        assert_calls_cached(&mut cx, &app);
    }

    #[test]
    fn call_notifications_do_not_rebuild_chat_body() {
        let (mut cx, window, app) = setup();
        app.update(&mut cx, |app, cx| {
            app.destination = Destination::Chats;
            cx.notify();
        });
        assert_calls_cached(&mut cx, &app);

        window
            .update(&mut cx, |_, window, cx| {
                app.update(cx, |app, cx| {
                    let chat = Arc::make_mut(&mut app.chats[0]);
                    for n in 0..100 {
                        chat.messages.push(ChatMessage::new_incoming(
                            format!("message-{n}"),
                            chat.jid.clone(),
                            "A synthetic message".into(),
                        ));
                    }
                    app.app_state = AppState::Connected;
                    app.select_chat(
                        "peer@example.invalid".into(),
                        ChatOpen::ToCompose,
                        window,
                        cx,
                    );
                });
            })
            .unwrap();
        assert_calls_cached(&mut cx, &app);
        cx.update(|cx| {
            let app = app.read(cx);
            assert_eq!(app.visible_chat.as_deref(), Some("peer@example.invalid"));
            assert!(app.keyboard_surfaces.composer);
            assert_eq!(app.keyboard_owner, Some(KeyboardOwner::Composer));
        });

        let before = cx.update(|cx| app.read(cx).body_renders);
        app.update(&mut cx, |app, cx| {
            app.input_area
                .as_ref()
                .unwrap()
                .update(cx, |_, cx| cx.notify());
        });
        assert!(cx.update(|cx| app.read(cx).body_renders) > before);
        assert_calls_cached(&mut cx, &app);
    }

    #[test]
    fn call_focus_delivers_input_blur_and_restoration_without_another_redraw() {
        use gpui_component::WindowExt as _;
        use std::rc::Rc;

        let (mut cx, window, app) = setup();
        window
            .update(&mut cx, |_, window, cx| {
                window.activate_window();
                app.update(cx, |app, cx| {
                    app.app_state = AppState::Connected;
                    app.destination = Destination::Chats;
                    app.select_chat(
                        "peer@example.invalid".into(),
                        ChatOpen::ToCompose,
                        window,
                        cx,
                    );
                });
            })
            .unwrap();
        cx.run_until_parked();

        let events = Rc::new(RefCell::new(Vec::new()));
        let (composer, call) = cx
            .update_window(window.into(), |_, window, cx| {
                app.update(cx, |app, cx| {
                    let composer = app.input_area.as_ref().unwrap().read(cx).focus_handle(cx);
                    let call = app.call_focus.clone();
                    assert!(composer.is_focused(window));
                    assert_eq!(window.focused_input(cx).unwrap().focus_handle(cx), composer,);
                    for (handle, focused, blurred) in [
                        (&composer, "composer focused", "composer blurred"),
                        (&call, "call focused", "call blurred"),
                    ] {
                        let focus_events = events.clone();
                        cx.on_focus(handle, window, move |_, _, _| {
                            focus_events.borrow_mut().push(focused);
                        })
                        .detach();
                        let blur_events = events.clone();
                        cx.on_blur(handle, window, move |_, _, _| {
                            blur_events.borrow_mut().push(blurred);
                        })
                        .detach();
                    }
                    (composer, call)
                })
            })
            .unwrap();

        app.update(&mut cx, |app, cx| {
            app.calls.update(cx, |calls, cx| {
                calls.place_outgoing(
                    OutgoingCall::new(
                        "focus-call",
                        "peer@example.invalid".into(),
                        "Test peer".into(),
                        false,
                    ),
                    cx,
                );
            });
        });
        assert_eq!(*events.borrow(), ["composer blurred", "call focused"]);
        cx.update_window(window.into(), |_, window, cx| {
            assert!(call.is_focused(window));
            assert!(!window.has_focused_input(cx));
        })
        .unwrap();

        app.update(&mut cx, |app, cx| {
            app.calls.update(cx, |calls, cx| {
                calls.end(cx);
            });
        });
        assert_eq!(
            *events.borrow(),
            [
                "composer blurred",
                "call focused",
                "call blurred",
                "composer focused"
            ],
        );
        cx.update_window(window.into(), |_, window, cx| {
            assert!(composer.is_focused(window));
            assert_eq!(window.focused_input(cx).unwrap().focus_handle(cx), composer);
        })
        .unwrap();
        assert_calls_cached(&mut cx, &app);
    }

    #[test]
    fn escape_unfocuses_the_composer() {
        let (mut cx, window, app) = setup();
        window
            .update(&mut cx, |_, window, cx| {
                window.activate_window();
                app.update(cx, |app, cx| {
                    app.app_state = AppState::Connected;
                    app.destination = Destination::Chats;
                    app.select_chat(
                        "peer@example.invalid".into(),
                        ChatOpen::ToCompose,
                        window,
                        cx,
                    );
                    app.close_overlay(window, cx);
                });
            })
            .unwrap();
        cx.update_window(window.into(), |_, window, cx| {
            let app = app.read(cx);
            let composer = app.input_area.as_ref().unwrap().read(cx).focus_handle(cx);
            assert!(!composer.is_focused(window));
        })
        .unwrap();
    }

    fn one_pixel_png() -> Vec<u8> {
        vec![
            137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1,
            8, 6, 0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 156, 99, 248, 207,
            192, 240, 31, 0, 3, 3, 1, 0, 24, 251, 3, 253, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96,
            130,
        ]
    }

    fn connected_app_fixture(
        cx: &mut gpui::TestAppContext,
    ) -> (gpui::VisualTestContext, Entity<WhatsAppApp>) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::init(cx);
            init_app_bindings(cx);
        });
        let mut app_entity = None;
        let window = cx.open_window(gpui::size(gpui::px(1000.), gpui::px(800.)), |window, cx| {
            let app = cx.new(|cx| {
                let mut app = WhatsAppApp::new(cx);
                app.app_state = AppState::Connected;
                app.destination = Destination::Chats;
                app.chats
                    .push(Arc::new(Chat::new("peer@example.invalid".into())));
                app
            });
            app_entity = Some(app.clone());
            gpui_component::Root::new(app, window, cx)
        });
        let app = app_entity.unwrap();
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            app.update(cx, |app, cx| {
                // Beside the app the way production does, so the modal can
                // build its caption field.
                app.set_modal_window(window.window_handle());
                app.select_chat(
                    "peer@example.invalid".into(),
                    ChatOpen::ToCompose,
                    window,
                    cx,
                );
            });
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        (cx, app)
    }

    fn paste_preview_fixture(
        cx: &mut gpui::TestAppContext,
    ) -> (gpui::VisualTestContext, Entity<WhatsAppApp>) {
        use gpui::{ClipboardItem, Image, ImageFormat};

        let (mut cx, app) = connected_app_fixture(cx);
        cx.write_to_clipboard(ClipboardItem::new_image(&Image {
            format: ImageFormat::Png,
            bytes: one_pixel_png(),
            id: 0,
        }));
        cx.simulate_keystrokes(if cfg!(target_os = "macos") {
            "cmd-v"
        } else {
            "ctrl-v"
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        (cx, app)
    }

    #[gpui::test]
    fn pasted_image_waits_in_a_visible_preview(cx: &mut gpui::TestAppContext) {
        let (mut cx, app) = paste_preview_fixture(cx);

        cx.read(|cx| {
            let app = app.read(cx);
            assert!(
                app.attachment_attempts.is_empty(),
                "opening the preview must not call the attachment send boundary"
            );
            assert_eq!(app.keyboard_owner, Some(KeyboardOwner::PastePreview));
            assert!(
                app.paste_preview
                    .as_ref()
                    .is_some_and(|preview| preview.chat_was_visible),
                "the preview must remember that its destination was visible before the modal"
            );
            assert!(
                app.visible_chat.is_none(),
                "a conversation covered by the modal must not count as visible"
            );
        });
        cx.update(|window, cx| {
            assert!(app.read(cx).paste_preview_focus.is_focused(window));
        });
        assert!(
            cx.debug_bounds("paste-preview").is_some(),
            "the pasted image must open a rendered preview"
        );
        assert!(
            cx.debug_bounds("paste-preview-image").is_some(),
            "the pasted image itself must be rendered in the preview"
        );
    }

    #[gpui::test]
    fn send_button_confirms_once_and_closes_preview(cx: &mut gpui::TestAppContext) {
        let (mut cx, app) = paste_preview_fixture(cx);
        let send = cx
            .debug_bounds("paste-preview-send")
            .expect("preview must render a Send control");
        cx.simulate_click(send.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.read(|cx| {
            let app = app.read(cx);
            assert_eq!(app.attachment_attempts.len(), 1);
            assert_eq!(app.attachment_attempts[0].0.file_name, "pasted.png");
            assert_eq!(app.attachment_attempts[0].0.mime_type, "image/png");
            assert_eq!(app.attachment_attempts[0].0.bytes, one_pixel_png());
            assert!(app.paste_preview.is_none());
        });
        cx.simulate_click(send.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        cx.read(|cx| assert_eq!(app.read(cx).attachment_attempts.len(), 1));
    }

    #[gpui::test]
    fn cancel_button_discards_preview_without_sending(cx: &mut gpui::TestAppContext) {
        let (mut cx, app) = paste_preview_fixture(cx);
        let cancel = cx
            .debug_bounds("paste-preview-cancel")
            .expect("preview must render a Cancel control");
        cx.simulate_click(cancel.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.read(|cx| {
            assert!(app.read(cx).attachment_attempts.is_empty());
            assert!(app.read(cx).paste_preview.is_none());
        });
    }

    #[gpui::test]
    fn escape_cancels_preview_without_sending_and_restores_composer(cx: &mut gpui::TestAppContext) {
        let (mut cx, app) = paste_preview_fixture(cx);

        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));

        cx.read(|cx| {
            let app = app.read(cx);
            assert!(app.paste_preview.is_none());
            assert!(app.attachment_attempts.is_empty());
            assert_eq!(app.keyboard_owner, Some(KeyboardOwner::Composer));
        });
        cx.update(|window, cx| {
            let app = app.read(cx);
            let composer = app.input_area.as_ref().unwrap().read(cx).focus_handle(cx);
            assert!(composer.is_focused(window));
        });
    }

    #[gpui::test]
    fn preview_modal_renders_a_caption_box(cx: &mut gpui::TestAppContext) {
        let (mut cx, _app) = paste_preview_fixture(cx);
        assert!(
            cx.debug_bounds("paste-preview-caption").is_some(),
            "the confirmation modal must offer a caption box"
        );
    }

    #[gpui::test]
    fn caption_typed_in_modal_is_sent_with_the_image(cx: &mut gpui::TestAppContext) {
        let (mut cx, app) = paste_preview_fixture(cx);
        cx.update(|window, cx| {
            let caption = app
                .read(cx)
                .paste_preview
                .as_ref()
                .expect("preview open")
                .caption
                .clone()
                .expect("caption box built");
            caption.update(cx, |caption, cx| {
                caption.set_value("olha a foto", window, cx);
            });
        });
        let send = cx
            .debug_bounds("paste-preview-send")
            .expect("preview must render a Send control");
        cx.simulate_click(send.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        cx.read(|cx| {
            let app = app.read(cx);
            assert_eq!(app.attachment_attempts.len(), 1);
            // The caption travels with the send, the way the protocol's own
            // does — the echo bubble it would land in is drawn from the same
            // value (see `send_attachment`).
            assert_eq!(app.attachment_attempts[0].1.as_deref(), Some("olha a foto"));
        });
    }

    #[gpui::test]
    fn dropped_file_waits_in_preview_instead_of_sending(cx: &mut gpui::TestAppContext) {
        let (mut cx, app) = connected_app_fixture(cx);
        let path = std::env::temp_dir().join(format!(
            "oxidezap-modal-drop-{}-foto.png",
            std::process::id()
        ));
        std::fs::write(&path, one_pixel_png()).expect("write test file");
        cx.update(|_window, cx| {
            app.update(cx, |app, cx| app.drop_paths(vec![path.clone()], cx));
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.read(|cx| {
            let app = app.read(cx);
            assert!(
                app.attachment_attempts.is_empty(),
                "a drop must not send before confirmation"
            );
            assert!(
                app.paste_preview.is_some(),
                "a drop must open the confirmation modal"
            );
        });
        assert!(
            cx.debug_bounds("paste-preview").is_some(),
            "the dropped file must open a rendered preview"
        );
        let send = cx
            .debug_bounds("paste-preview-send")
            .expect("preview must render a Send control");
        cx.simulate_click(send.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        cx.read(|cx| {
            assert_eq!(
                app.read(cx).attachment_attempts.len(),
                1,
                "confirming the preview sends the dropped file"
            );
        });
        std::fs::remove_file(path).expect("remove test file");
    }

    #[gpui::test]
    fn dropped_video_confirms_as_a_file_without_an_image_preview(cx: &mut gpui::TestAppContext) {
        let (mut cx, app) = connected_app_fixture(cx);
        let path = std::env::temp_dir().join(format!(
            "oxidezap-modal-drop-{}-clipe.mp4",
            std::process::id()
        ));
        std::fs::write(&path, b"not really a video").expect("write test file");
        cx.update(|_window, cx| {
            app.update(cx, |app, cx| app.drop_paths(vec![path.clone()], cx));
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        std::fs::remove_file(path).expect("remove test file");
        cx.read(|cx| {
            let app = app.read(cx);
            assert!(app.attachment_attempts.is_empty());
            let preview = app.paste_preview.as_ref().expect("preview open");
            assert_eq!(preview.files.len(), 1);
            assert_eq!(preview.files[0].file.mime_type, "video/mp4");
            assert!(
                preview.files[0].image.is_none(),
                "a video has no poster frame this side can draw"
            );
        });
        assert!(cx.debug_bounds("paste-preview").is_some());
        assert!(
            cx.debug_bounds("paste-preview-image").is_none(),
            "a video must not draw the picture preview"
        );
    }

    #[gpui::test]
    fn second_paste_while_preview_open_is_dropped(cx: &mut gpui::TestAppContext) {
        use gpui::{ClipboardItem, Image, ImageFormat};

        let (mut cx, app) = paste_preview_fixture(cx);
        cx.write_to_clipboard(ClipboardItem::new_image(&Image {
            format: ImageFormat::Png,
            bytes: one_pixel_png(),
            id: 0,
        }));
        cx.simulate_keystrokes(if cfg!(target_os = "macos") {
            "cmd-v"
        } else {
            "ctrl-v"
        });
        cx.run_until_parked();
        cx.read(|cx| {
            assert_eq!(
                app.read(cx)
                    .paste_preview
                    .as_ref()
                    .map(|preview| preview.files.len()),
                Some(1),
                "a paste while the modal is open must not queue a second preview"
            );
        });
    }

    #[gpui::test]
    fn hidden_chat_does_not_accept_files(cx: &mut gpui::TestAppContext) {
        let (mut cx, app) = connected_app_fixture(cx);
        cx.update(|_window, cx| {
            app.update(cx, |app, cx| {
                assert!(app.prepare_incoming_files(cx).is_some());
                // A phone's chat list or a fullscreen viewer keeps the
                // selected chat but replaces the composer entirely.
                app.visible_chat = None;
                assert!(app.prepare_incoming_files(cx).is_none());
            });
        });
    }

    #[gpui::test]
    fn files_read_for_an_old_account_cannot_open_a_new_preview(cx: &mut gpui::TestAppContext) {
        let (mut cx, app) = connected_app_fixture(cx);
        cx.update(|_window, cx| {
            app.update(cx, |app, cx| {
                let (_, _, epoch) = app.prepare_incoming_files(cx).expect("chat visible");
                app.incoming_file_epoch = app.incoming_file_epoch.wrapping_add(1);
                assert!(!app.incoming_files_are_current(epoch));
            });
        });
    }

    #[gpui::test]
    fn confirm_while_offline_keeps_the_preview_waiting(cx: &mut gpui::TestAppContext) {
        let (mut cx, app) = paste_preview_fixture(cx);
        cx.update(|_window, cx| {
            app.update(cx, |app, cx| {
                // The connection dropped with the modal open: the rendered
                // Send button is disabled there, and the keyboard path must
                // not bypass it — neither sending through a lingering session
                // nor discarding the files.
                app.app_state = AppState::Offline;
                app.confirm_paste_preview(cx);
            });
        });
        cx.read(|cx| {
            let app = app.read(cx);
            assert!(app.attachment_attempts.is_empty());
            assert!(
                app.paste_preview.is_some(),
                "losing the connection must not consume the preview"
            );
        });
    }

    #[test]
    fn body_invalidates_for_controllers_theme_resize_and_focus() {
        let (mut cx, window, app) = setup();
        let controllers = cx.update(|cx| {
            let state = app.read(cx);
            [
                app.entity_id(),
                state.pages.entity_id(),
                state.viewer.entity_id(),
                state.search.entity_id(),
                state.recorder.entity_id(),
                state.recovery.entity_id(),
                state.plugins.entity_id(),
                state.settings.entity_id(),
            ]
        });
        for controller in controllers {
            let before = cx.update(|cx| app.read(cx).body_renders);
            app.update(&mut cx, |_, cx| gpui::App::notify(cx, controller));
            assert!(cx.update(|cx| app.read(cx).body_renders) > before);
            assert_calls_cached(&mut cx, &app);
        }
        let before = cx.update(|cx| app.read(cx).body_renders);
        window
            .update(&mut cx, |_, window, cx| {
                let mut theme = cx.product().settings();
                theme.font_size += 1.;
                crate::theme::install(theme, cx);
                window.refresh();
            })
            .unwrap();
        assert!(cx.update(|cx| app.read(cx).body_renders) > before);
        assert_calls_cached(&mut cx, &app);

        let before = cx.update(|cx| app.read(cx).body_renders);
        window
            .update(&mut cx, |_, window, cx| {
                window.resize(gpui::size(gpui::px(480.), gpui::px(640.)));
                window.bounds_changed(cx);
            })
            .unwrap();
        assert!(cx.update(|cx| app.read(cx).body_renders) > before);
        assert_calls_cached(&mut cx, &app);

        window
            .update(&mut cx, |_, window, cx| {
                app.update(cx, |app, cx| app.open_settings(window, cx));
            })
            .unwrap();
        window
            .update(&mut cx, |_, window, cx| {
                app.update(cx, |app, cx| app.close_overlay(window, cx));
            })
            .unwrap();
        cx.update(|cx| {
            assert_eq!(app.read(cx).keyboard_owner, Some(KeyboardOwner::Root));
            assert!(!app.read(cx).keyboard_surfaces.composer);
        });
    }
}
