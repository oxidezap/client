use chrono::{Local, NaiveDateTime, TimeZone as _};
use gpui::{AppContext as _, Context, Entity, ParentElement as _, Styled as _, Window, div};
use gpui_component::{
    WindowExt as _,
    input::{Input, InputState},
};

use super::{WhatsAppApp, notices::Tone};
use crate::components::InputAreaView;
use crate::theme::ActiveProductTheme as _;

impl WhatsAppApp {
    pub(super) fn open_schedule_message(
        &mut self,
        text: &str,
        input_area: &Entity<InputAreaView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(jid), Some(session)) = (self.selected_chat.clone(), &self.client) else {
            return;
        };
        let account = session.account();
        let time = cx.new(|cx| InputState::new(window, cx).placeholder("YYYY-MM-DD HH:MM"));
        let draft = text.to_owned();
        let composer = input_area.downgrade();
        let app = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |dialog, _, cx| {
            let metrics = cx.product().metrics;
            let time_input = time.clone();
            let jid = jid.clone();
            let draft = draft.clone();
            let composer = composer.clone();
            let app = app.clone();
            dialog
                .confirm()
                .title(crate::tr!(ScheduleMessage))
                .ok_text(crate::tr!(ScheduleMessage))
                .cancel_text(crate::tr!(Cancel))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(metrics.space_md())
                        .child(crate::tr!(ScheduleMessageTime))
                        .child(Input::new(&time))
                        .child(crate::tr!(ScheduleMessageDelivery)),
                )
                .on_ok(move |_, window, cx| {
                    let raw = time_input.read(cx).value();
                    let at = NaiveDateTime::parse_from_str(raw.trim(), "%Y-%m-%d %H:%M")
                        .ok()
                        .and_then(|local| Local.from_local_datetime(&local).single())
                        .map(|at| at.timestamp_millis())
                        .filter(|at| *at > wacore::time::now_millis());
                    let Some(at) = at else {
                        let _ = app.update(cx, |app, cx| {
                            app.notify_user(
                                crate::tr!(ScheduleMessageInvalidTime),
                                Tone::Problem,
                                cx,
                            );
                        });
                        return false;
                    };
                    let window = window.window_handle();
                    let composer = composer.clone();
                    let jid = jid.clone();
                    let draft = draft.clone();
                    let _ = app.update(cx, |app, cx| {
                        if app.selected_chat.as_deref() != Some(jid.as_str()) {
                            return;
                        }
                        let Some(session) = app
                            .client
                            .as_ref()
                            .filter(|session| session.account() == account)
                        else {
                            return;
                        };
                        let reply = session.schedule_message(jid.clone(), draft.clone(), at);
                        cx.spawn(async move |app, cx| {
                            let result = reply
                                .await
                                .map_err(|error| error.to_string())
                                .and_then(|result| result.map_err(|error| error.to_string()));
                            let _ = app.update(cx, |app, cx| match result {
                                Ok(()) => {
                                    if app
                                        .client
                                        .as_ref()
                                        .is_some_and(|session| session.account() == account)
                                    {
                                        if app.drafts.get(&jid) == Some(&draft) {
                                            app.drafts.remove(&jid);
                                        }
                                        if app.selected_chat.as_deref() == Some(jid.as_str()) {
                                            let _ = window.update(cx, |_, window, cx| {
                                                let _ = composer.update(cx, |view, cx| {
                                                    view.clear_scheduled_draft(&draft, window, cx);
                                                });
                                            });
                                        }
                                    }
                                    app.notify_user(crate::tr!(ScheduledMessage), Tone::Info, cx);
                                }
                                Err(error) => app.notify_user(error, Tone::Problem, cx),
                            });
                        })
                        .detach();
                    });
                    true
                })
        });
    }
}

impl WhatsAppApp {
    pub(super) fn open_scheduled_messages(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(jid), Some(session)) = (self.selected_chat.clone(), &self.client) else {
            return;
        };
        let account = session.account();
        let reply = session.scheduled_messages(jid.clone());
        let window = window.window_handle();
        cx.spawn(async move |app, cx| {
            let result = reply
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result.map_err(|error| error.to_string()));
            let _ = window.update(cx, |_, window, cx| {
                let _ = app.update(cx, |view, cx| {
                    if view.selected_chat.as_deref() != Some(jid.as_str())
                        || !view
                            .client
                            .as_ref()
                            .is_some_and(|session| session.account() == account)
                    {
                        return;
                    }
                    match result {
                        Ok(items) => {
                            let items = cx.new(|_| items);
                            show_scheduled_messages(window, cx, app.clone(), account, jid, items);
                        }
                        Err(error) => view.notify_user(error, Tone::Problem, cx),
                    }
                });
            });
        })
        .detach();
    }
}

fn show_scheduled_messages(
    window: &mut Window,
    cx: &mut gpui::App,
    app: gpui::WeakEntity<WhatsAppApp>,
    account: oxidezap_core::AccountId,
    jid: String,
    items: Entity<Vec<oxidezap_core::ScheduledMessage>>,
) {
    use gpui::{InteractiveElement as _, StatefulInteractiveElement as _};
    use gpui_component::button::{Button, ButtonVariants as _};

    window.open_alert_dialog(cx, move |dialog, _, cx| {
        let metrics = cx.product().metrics;
        let rows = items.read(cx);
        let content = div()
            .id("scheduled-messages-list")
            .flex()
            .flex_col()
            .gap(metrics.space_md())
            .max_h(gpui::rems(20.))
            .overflow_y_scroll()
            .children(rows.iter().map(|message| {
                let id = message.id;
                let app = app.clone();
                let jid = jid.clone();
                let items = items.clone();
                let time = chrono::DateTime::from_timestamp_millis(message.send_at_ms)
                    .map(|time| {
                        time.with_timezone(&Local)
                            .format("%Y-%m-%d %H:%M")
                            .to_string()
                    })
                    .unwrap_or_default();
                div()
                    .flex()
                    .flex_col()
                    .gap(metrics.space_sm())
                    .child(time)
                    .child(message.preview.clone())
                    .child(
                        Button::new(format!("cancel-scheduled-{id}"))
                            .ghost()
                            .label(crate::tr!(CancelScheduledMessage))
                            .on_click(move |_, _, cx| {
                                let jid = jid.clone();
                                let items = items.clone();
                                let _ = app.update(cx, |app, cx| {
                                    let Some(session) = app
                                        .client
                                        .as_ref()
                                        .filter(|session| session.account() == account)
                                    else {
                                        return;
                                    };
                                    let reply = session.cancel_scheduled_message(jid, id);
                                    cx.spawn(async move |app, cx| {
                                        let result = reply
                                            .await
                                            .map_err(|error| error.to_string())
                                            .and_then(|result| {
                                                result.map_err(|error| error.to_string())
                                            });
                                        let _ = app.update(cx, |app, cx| match result {
                                            Ok(()) => items.update(cx, |items, cx| {
                                                items.retain(|item| item.id != id);
                                                cx.notify();
                                            }),
                                            Err(error) => app.notify_user(error, Tone::Problem, cx),
                                        });
                                    })
                                    .detach();
                                });
                            }),
                    )
            }));
        let dialog = dialog
            .title(crate::tr!(ScheduledMessages))
            .ok_text(crate::tr!(Close));
        if rows.is_empty() {
            dialog.child(crate::tr!(NoScheduledMessages))
        } else {
            dialog.child(content)
        }
    });
}
