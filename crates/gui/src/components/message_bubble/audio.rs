//! Voice message / audio attachment player inside a bubble.

use super::*;

pub(super) fn render_audio_player(
    media_content: oxidezap_core::MediaContent,
    message_id: String,
    is_playing: bool,
    entity: Entity<WhatsAppApp>,
    cx: &App,
) -> impl IntoElement + use<> {
    let has_data = media_content.has_data();
    let can_download = media_content.can_download();
    let can_play = has_data || can_download;
    let downloadable = media_content.downloadable.clone();
    let button_id: SharedString = format!("play-{}", message_id).into();
    let duration_text: SharedString = if let Some(secs) = media_content.duration_secs {
        let mins = secs / 60;
        let secs = secs % 60;
        format!("{:02}:{:02}", mins, secs).into()
    } else {
        "Voice message".into()
    };

    div()
        .w(px(220.))
        .h(px(44.))
        .bg(cx.theme().list_active)
        .rounded(px(layout::RADIUS_LARGE))
        .flex()
        .items_center()
        .px_2()
        .gap_2()
        .child(
            Button::new(button_id)
                .icon(
                    Icon::default()
                        .path(if is_playing {
                            "icons/pause.svg"
                        } else {
                            "icons/play.svg"
                        })
                        .text_color(cx.theme().foreground),
                )
                .ghost()
                .disabled(!can_play)
                .on_click({
                    let data = media_content.data.clone();
                    let downloadable = downloadable.clone();
                    move |_, _window, cx| {
                        let msg_id = message_id.clone();
                        entity.update(cx, |app, cx| {
                            if !data.is_empty() {
                                app.toggle_audio(msg_id, (*data).clone(), cx);
                            } else if let Some(dl) = downloadable.clone() {
                                app.toggle_audio_lazy(msg_id, dl, cx);
                            }
                        });
                    }
                }),
        )
        .child(
            div()
                .flex_1()
                .h(px(24.))
                .rounded(px(4.))
                .bg(if is_playing {
                    cx.theme().primary
                } else {
                    cx.theme().list_hover
                })
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(match (is_playing, !has_data && can_download) {
                            (true, _) => SharedString::from("Playing..."),
                            (_, true) => SharedString::from("Tap to download"),
                            _ => duration_text,
                        }),
                ),
        )
}
