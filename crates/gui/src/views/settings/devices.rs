//! Audio and camera selection, tests, and the pre-call preview.
use super::panes::{group, label};
use crate::app::WhatsAppApp;
use crate::theme::Metrics;
use gpui::{App, Entity, IntoElement, ParentElement, Styled, div, img};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _};
use oxidezap_core::{CallDevice, CallDevicePreferences, CameraMode, Ringtone};

#[derive(Clone, Copy)]
enum Kind {
    Microphone,
    Speaker,
    RingtoneOutput,
    Camera,
}
impl Kind {
    fn value(self, p: &CallDevicePreferences) -> &Option<String> {
        match self {
            Self::Microphone => &p.microphone,
            Self::Speaker => &p.speaker,
            Self::RingtoneOutput => &p.ringtone_output,
            Self::Camera => &p.camera,
        }
    }
    fn assign(self, p: &mut CallDevicePreferences, id: Option<String>) {
        match self {
            Self::Microphone => p.microphone = id,
            Self::Speaker => p.speaker = id,
            Self::RingtoneOutput => p.ringtone_output = id,
            Self::Camera => p.camera = id,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn choices(
    title: &'static str,
    kind: Kind,
    devices: &[CallDevice],
    preferences: &CallDevicePreferences,
    entity: Entity<WhatsAppApp>,
    busy: bool,
    m: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let mut options = vec![(None, "System default".to_string())];
    options.extend(devices.iter().map(|d| {
        (
            Some(d.id.clone()),
            if d.is_default {
                format!("{} (system default)", d.label)
            } else {
                d.label.clone()
            },
        )
    }));
    let selected = kind.value(preferences).clone();
    if let Some(id) = &selected
        && !devices.iter().any(|d| &d.id == id)
    {
        options.push((
            Some(id.clone()),
            "Saved device unavailable. Refresh or choose another device.".into(),
        ));
    }
    group(
        label(title, m, cx),
        div()
            .flex()
            .flex_col()
            .gap(m.space_sm())
            .children(options.into_iter().enumerate().map(|(index, (id, text))| {
                let entity = entity.clone();
                let chosen = selected == id;
                let mut preferences = preferences.clone();
                kind.assign(&mut preferences, id);
                Button::new(gpui::SharedString::from(format!("device-{title}-{index}")))
                    .label(text)
                    .outline()
                    .small()
                    .selected(chosen)
                    .disabled(busy)
                    .on_click(move |_, _, cx| {
                        let p = preferences.clone();
                        entity.update(cx, |app, cx| app.save_call_devices(p, cx));
                    })
            })),
        m,
    )
}

pub fn render(
    app: &WhatsAppApp,
    entity: Entity<WhatsAppApp>,
    m: Metrics,
    cx: &App,
) -> gpui::AnyElement {
    let preferences = app.call_device_preferences(cx);
    let inventory = app.call_device_inventory(cx).cloned().unwrap_or_default();
    let busy = app.call_device_busy(cx);
    let ready = app.call_device_preferences_ready(cx);
    let refresh = entity.clone();
    let mic_test = entity.clone();
    let speaker_test = entity.clone();
    let ringtone_test = entity.clone();
    let preview = entity.clone();
    let confirm = entity.clone();
    let cancel = entity.clone();
    let active = app.camera_preview_active(cx);
    let camera = preferences
        .camera
        .as_ref()
        .and_then(|id| inventory.cameras.iter().find(|d| &d.id == id))
        .or_else(|| inventory.cameras.iter().find(|d| d.is_default));
    let mut modes = camera.map(|d| d.modes.clone()).unwrap_or_default();
    modes.retain(|m| {
        m.width > 0
            && m.height > 0
            && m.width <= 1280
            && m.height <= 720
            && m.fps > 0
            && m.fps <= 60
            && 90000u32.is_multiple_of(m.fps)
    });
    modes.sort_by_key(|m| (m.width, m.height, m.fps));
    modes.dedup();
    if modes.is_empty() {
        modes.push(CameraMode::default());
    }
    let mut content = div().flex().flex_col().gap(m.space_xl())
        .child(div().text_size(m.text_small()).text_color(cx.theme().muted_foreground).child("Preferences apply to your next call. Test each device before calling. Device tests access only the device you choose to test."))
        .child(Button::new("refresh-call-devices").label("Refresh devices").outline().disabled(busy).on_click(move |_, _, cx| refresh.update(cx, |app, cx| app.refresh_call_devices(cx))))
        .child(choices("MICROPHONE", Kind::Microphone, &inventory.microphones, &preferences, entity.clone(), busy, m, cx))
        .child(Button::new("test-call-microphone").label("Test microphone").outline().disabled(busy).on_click(move |_, _, cx| mic_test.update(cx, |app, cx| app.test_call_microphone(cx))))
        .child(choices("SPEAKER", Kind::Speaker, &inventory.speakers, &preferences, entity.clone(), busy, m, cx))
        .child(Button::new("test-call-speaker").label("Test speaker").outline().disabled(busy).on_click(move |_, _, cx| speaker_test.update(cx, |app, cx| app.test_call_speaker(cx))))
        .child(choices("RINGTONE OUTPUT", Kind::RingtoneOutput, &inventory.speakers, &preferences, entity.clone(), busy, m, cx))
        .child(group(label("RINGTONE", m, cx), div().flex().gap(m.space_sm()).children([(Ringtone::Silent, "Silent"), (Ringtone::Classic, "Classic"), (Ringtone::Gentle, "Gentle")].into_iter().map(|(tone, label)| {
            let entity = entity.clone(); let mut p = preferences.clone(); p.ringtone = tone;
            Button::new(gpui::SharedString::from(format!("ringtone-{label}"))).label(label).outline().small().selected(preferences.ringtone == tone).disabled(busy).on_click(move |_, _, cx| { let p = p.clone(); entity.update(cx, |app, cx| app.save_call_devices(p, cx)); })
        })), m))
        .child(Button::new("test-call-ringtone").label("Test ringtone").outline().disabled(busy).on_click(move |_, _, cx| ringtone_test.update(cx, |app, cx| app.test_call_ringtone(cx))))
        .child(choices("CAMERA", Kind::Camera, &inventory.cameras, &preferences, entity.clone(), busy, m, cx))
        .child(group(label("RESOLUTION AND FRAME RATE", m, cx), div().flex().flex_wrap().gap(m.space_sm()).children(modes.into_iter().enumerate().map(|(index, mode)| {
            let entity = entity.clone(); let mut p = preferences.clone(); p.camera_mode = mode;
            Button::new(gpui::SharedString::from(format!("camera-mode-{index}"))).label(format!("{} × {} · {} FPS", mode.width, mode.height, mode.fps)).outline().small().selected(preferences.camera_mode == mode).disabled(busy).on_click(move |_, _, cx| { let p = p.clone(); entity.update(cx, |app, cx| app.save_call_devices(p, cx)); })
        })), m))
        .child(div().text_size(m.text_small()).child(format!("Requested camera mode: {} × {} · {} FPS", preferences.camera_mode.width, preferences.camera_mode.height, preferences.camera_mode.fps)))
        .child(Button::new("camera-preview").label(if active { "Stop camera preview" } else { "Preview camera" }).outline().disabled((busy || !ready) && !active).on_click(move |_, _, cx| preview.update(cx, |app, cx| if active { app.cancel_camera_preview(cx); } else { app.start_camera_preview(cx); })));
    if let Some(image) = app.camera_preview_image(cx) {
        content = content.child(
            img(crate::video::video_image(image))
                .w_full()
                .max_w(m.reading_width())
                .rounded(m.radius_md()),
        );
    }
    if let Some(mode) = app.camera_preview_mode(cx) {
        content = content.child(div().text_size(m.text_small()).child(format!(
            "Camera opened at {} × {} · {} FPS",
            mode.width, mode.height, mode.fps
        )));
    }
    content = content.child(
        div()
            .text_size(m.text_small())
            .text_color(cx.theme().muted_foreground)
            .child(app.call_device_status(cx)),
    );
    if app.preparing_call(cx) {
        content = content.child(
            div()
                .flex()
                .gap(m.space_md())
                .child(
                    Button::new("confirm-device-call")
                        .label("Start call")
                        .primary()
                        .disabled(busy || !ready)
                        .on_click(move |_, _, cx| {
                            confirm.update(cx, |app, cx| app.confirm_prepared_call(cx))
                        }),
                )
                .child(
                    Button::new("cancel-device-call")
                        .label("Cancel")
                        .outline()
                        .on_click(move |_, _, cx| {
                            cancel.update(cx, |app, cx| {
                                app.close_settings(cx);
                            });
                        }),
                ),
        );
    }
    content.into_any_element()
}
