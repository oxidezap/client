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
