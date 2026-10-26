//! The Settings device controls and a preview that never enters call state.
use super::WhatsAppApp;
use gpui::{App, AppContext as _, Context, RenderImage};
use oxidezap_core::{
    CallDeviceInventory, CallDevicePreferences, CameraMode, CameraPreviewId, PreviewAdmission,
};
use oxidezap_ipc::{CallDeviceAction as Action, CallDeviceReply as Reply};
use std::sync::Arc;
use std::time::Duration;

#[cfg(not(target_family = "wasm"))]
#[path = "call_devices/native.rs"]
mod imp;
#[cfg(target_family = "wasm")]
#[path = "call_devices/web.rs"]
mod imp;
use imp::PreviewDecoder;

#[derive(Default)]
pub(super) struct DeviceUi {
    pub inventory: Option<CallDeviceInventory>,
    pub status: String,
    pub busy: bool,
    pub preferences_ready: bool,
    pub pending_call: Option<(String, bool)>,
    pub preview_after_inventory: bool,
    approved_call: Option<(String, bool)>,
    pub preview: Option<CameraPreviewId>,
    stopping: Option<CameraPreviewId>,
    pub effective: Option<CameraMode>,
    pub image: Option<Arc<RenderImage>>,
    admission: PreviewAdmission,
    request: usize,
    refresh_after_request: bool,
}

impl DeviceUi {
    fn may_save(&self) -> bool {
        self.preferences_ready && !self.busy
    }
    fn begin_request(&mut self, refresh: bool) -> Option<usize> {
        if self.busy {
            self.refresh_after_request |= refresh;
            return None;
        }
        self.request = self.request.wrapping_add(1);
        self.busy = true;
        Some(self.request)
    }
    fn finish_request(&mut self, succeeded: bool) -> bool {
        self.busy = false;
        std::mem::take(&mut self.refresh_after_request) && succeeded
    }
    fn finish_preview(&mut self, id: CameraPreviewId, stopped: bool, failure: Option<String>) {
        if !self.admission.accepts(id) {
            return;
        }
        self.admission.cancel(id);
        self.preview = None;
        self.image = None;
        self.effective = None;
        self.stopping = (!stopped).then_some(id);
        self.status = if stopped {
            failure.unwrap_or_default()
        } else {
            format!(
                "{} Camera is still stopping. Press Stop camera preview to retry.",
                failure.unwrap_or_else(|| "Preview ended.".into())
            )
        };
    }
}

impl WhatsAppApp {
    pub fn call_device_inventory<'a>(&self, cx: &'a App) -> Option<&'a CallDeviceInventory> {
        self.settings.read(cx).devices.inventory.as_ref()
    }
    pub fn call_device_preferences_ready(&self, cx: &App) -> bool {
        self.settings.read(cx).devices.preferences_ready
    }
    pub fn call_device_status(&self, cx: &App) -> String {
        self.settings.read(cx).devices.status.clone()
    }
    pub fn call_device_busy(&self, cx: &App) -> bool {
        self.settings.read(cx).devices.busy
    }
    pub fn camera_preview_image(&self, cx: &App) -> Option<Arc<RenderImage>> {
        self.settings.read(cx).devices.image.clone()
    }
    pub fn camera_preview_active(&self, cx: &App) -> bool {
        self.settings.read(cx).devices.preview.is_some()
            || self.settings.read(cx).devices.stopping.is_some()
    }
    pub fn camera_preview_mode(&self, cx: &App) -> Option<CameraMode> {
        self.settings.read(cx).devices.effective
    }

    #[allow(
        dead_code,
        reason = "Consumed by the following call controls PR in this stack"
    )]
    pub fn take_call_setup_approval(
        &mut self,
        jid: &str,
        video: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        self.settings.update(cx, |s, _| {
            s.devices
                .approved_call
                .take()
                .is_some_and(|(approved, is_video)| approved == jid && is_video == video)
        })
    }
    pub fn preparing_call(&self, cx: &App) -> bool {
        self.settings.read(cx).devices.pending_call.is_some()
    }

    #[allow(
        dead_code,
        reason = "Consumed by the following call controls PR in this stack"
    )]
    pub fn prepare_call(&mut self, recipient_jid: String, is_video: bool, cx: &mut Context<Self>) {
        use crate::theme::ActiveProductTheme as _;
        self.close_media_viewer(cx);
        let mut state = super::settings::SettingsState::new(cx.product().settings());
        state.section = super::settings::SettingsSection::AudioVideo;
        self.settings.update(cx, |settings, cx| {
            settings.devices.pending_call = Some((recipient_jid, is_video));
            settings.devices.preview_after_inventory = is_video;
            settings.show(state, cx);
        });
        self.refresh_call_devices(cx);
        cx.notify();
    }

    pub fn confirm_prepared_call(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.as_ref().map(|c| c.handle()) else {
            return;
        };
        let (pending, id, epoch) = self.settings.update(cx, |s, _| {
            (
                s.devices.pending_call.clone(),
                s.devices.preview.or(s.devices.stopping),
                s.epoch(),
            )
        });
        let Some((jid, video)) = pending else {
            return;
        };
        self.cancel_camera_preview(cx);
        let entity = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            if let Some(id) = id {
                let waiting = client.call_devices(Action::CancelPreview { preview_id: id });
                if !matches!(crate::platform::with_timeout(waiting, Duration::from_secs(7)).await, Some(Ok(Ok(Reply::Stopped { .. })))) {
                    let _ = entity.update(cx, |app, cx| {
                        app.settings.update(cx, |s, cx| { s.devices.status = "Camera has not finished stopping. Try again before placing the call.".into(); cx.notify(); });
                    });
                    return;
                }
            }
            let _ = entity.update(cx, |app, cx| {
                if app.settings.read(cx).epoch() != epoch || app.settings.read(cx).devices.pending_call.as_ref() != Some(&(jid.clone(), video)) { return; }
                app.settings.update(cx, |s, cx| { s.devices.pending_call = None; s.devices.approved_call = Some((jid.clone(), video)); s.close(cx); });
                app.start_call(jid, video, cx);
            });
        }).detach();
    }

    pub fn refresh_call_device_preferences(&mut self, cx: &mut Context<Self>) {
        self.settings
            .update(cx, |s, _| s.devices.preferences_ready = false);
        self.device_request(Action::Preferences, cx);
    }
    pub fn refresh_call_devices(&mut self, cx: &mut Context<Self>) {
        if self.camera_preview_active(cx) {
            return;
        }
        self.device_request(Action::Inventory, cx);
    }
    pub fn save_call_devices(
        &mut self,
        preferences: CallDevicePreferences,
        cx: &mut Context<Self>,
    ) {
        if !self.settings.read(cx).devices.may_save() {
            self.settings.update(cx, |s, cx| {
                if !s.devices.preferences_ready {
                    s.devices.status =
                        "Refresh saved device settings before making changes.".into();
                }
                cx.notify();
            });
            return;
        }
        self.cancel_camera_preview(cx);
        self.device_request(Action::Save { preferences }, cx);
    }
    pub fn test_call_microphone(&mut self, cx: &mut Context<Self>) {
        self.device_request(Action::TestMicrophone, cx);
    }
    pub fn test_call_speaker(&mut self, cx: &mut Context<Self>) {
        self.device_request(Action::TestSpeaker, cx);
    }
    pub fn test_call_ringtone(&mut self, cx: &mut Context<Self>) {
        self.device_request(Action::TestRingtone, cx);
    }

    fn device_request(&mut self, action: Action, cx: &mut Context<Self>) {
        let Some(client) = &self.client else {
            self.settings.update(cx, |s, cx| {
                s.devices.status = "Connect to the daemon to configure devices".into();
                cx.notify();
            });
            return;
        };
        let is_save = matches!(&action, Action::Save { .. });
        let is_refresh = matches!(&action, Action::Inventory | Action::Preferences);
        let settings = self.settings.clone();
        let epoch = settings.read(cx).epoch();
        let request = settings.update(cx, |s, cx| {
            let request = s.devices.begin_request(is_refresh)?;
            s.devices.status = "Working…".into();
            cx.notify();
            Some(request)
        });
        let Some(request) = request else {
            return;
        };
        let waiting = client.call_devices(action);
        cx.spawn(async move |entity, cx| {
            let answer = crate::platform::with_timeout(waiting, Duration::from_secs(15)).await;
            let (refresh, preview) = settings.update(cx, |s, cx| {
                if s.epoch() != epoch || s.devices.request != request {
                    return (false, false);
                }
                let succeeded = matches!(
                    &answer,
                    Some(Ok(Ok(Reply::Preferences { .. }
                        | Reply::Settings { .. }
                        | Reply::Tested { .. })))
                );
                if is_save && !succeeded {
                    s.devices.preferences_ready = false;
                }
                let inventory_finished = matches!(&answer, Some(Ok(Ok(Reply::Settings { .. }))));
                match answer {
                    Some(Ok(Ok(Reply::Preferences { preferences }))) => {
                        s.call_devices = preferences;
                        s.devices.preferences_ready = true;
                        s.devices.status.clear();
                    }
                    Some(Ok(Ok(Reply::Settings { settings }))) => {
                        s.call_devices = settings.preferences;
                        s.devices.preferences_ready = true;
                        s.devices.status = settings.inventory.failures.join("\n");
                        s.devices.inventory = Some(settings.inventory);
                    }
                    Some(Ok(Ok(Reply::Tested { detail }))) => s.devices.status = detail,
                    Some(Ok(Err(e))) => s.devices.status = e,
                    _ => {
                        s.devices.status =
                            "Device request did not finish. Refresh devices and try again.".into()
                    }
                }
                let preview = inventory_finished
                    && std::mem::take(&mut s.devices.preview_after_inventory)
                    && s.is_open()
                    && s.devices
                        .pending_call
                        .as_ref()
                        .is_some_and(|(_, video)| *video);
                cx.notify();
                (s.devices.finish_request(succeeded), preview)
            });
            if preview {
                let _ = entity.update(cx, |app, cx| app.start_camera_preview(cx));
            } else if refresh {
                let _ = entity.update(cx, |app, cx| app.refresh_call_devices(cx));
            }
        })
        .detach();
    }

    pub fn cancel_camera_preview(&mut self, cx: &mut Context<Self>) {
        let id = self.settings.update(cx, |s, cx| {
            let id = s.devices.preview.take().or(s.devices.stopping);
            s.devices.stopping = id;
            if let Some(id) = id {
                s.devices.admission.cancel(id);
            }
            s.devices.image = None;
            s.devices.effective = None;
            cx.notify();
            id
        });
        if let (Some(id), Some(client)) = (id, &self.client) {
            let waiting = client.call_devices(Action::CancelPreview { preview_id: id });
            let settings = self.settings.clone();
            let epoch = settings.read(cx).epoch();
            cx.spawn(async move |_, cx| {
                let stopped = matches!(crate::platform::with_timeout(waiting, Duration::from_secs(7)).await, Some(Ok(Ok(Reply::Stopped { .. }))));
                settings.update(cx, |s, cx| {
                    if s.epoch() != epoch || s.devices.stopping != Some(id) { return; }
                    if stopped { s.devices.stopping = None; } else { s.devices.status = "Camera is still stopping. Press Stop camera preview to retry before calling.".into(); }
                    cx.notify();
                });
            }).detach();
        }
    }

    pub fn start_camera_preview(&mut self, cx: &mut Context<Self>) {
        if self.call_state(cx).is_busy() {
            self.settings.update(cx, |s, cx| {
                s.devices.status = "End the current call before opening a camera preview.".into();
                cx.notify();
            });
            return;
        }
        if self.camera_preview_active(cx) {
            return;
        }
        let Some(client) = self.client.as_ref().map(|c| c.handle()) else {
            return;
        };
        let settings = self.settings.clone();
        let epoch = settings.read(cx).epoch();
        let id = settings.update(cx, |s, cx| {
            let id = s.devices.admission.begin()?;
            s.devices.preview = Some(id);
            s.devices.status = "Opening camera preview…".into();
            cx.notify();
            Some(id)
        });
        let Some(id) = id else {
            return;
        };
        cx.spawn(async move |_, cx| {
            let start = client.call_devices(Action::StartPreview { preview_id: id });
            let started = crate::platform::with_timeout(start, Duration::from_secs(10)).await;
            let mut failure = match started {
                Some(Ok(Ok(Reply::Preview { .. }))) => None,
                Some(Ok(Err(e))) => Some(e),
                _ => Some("Camera preview did not start".into()),
            };
            let decoder = Arc::new(std::sync::Mutex::new(PreviewDecoder::default()));
            while failure.is_none() {
                let active = settings.read_with(cx, |s, _| {
                    s.epoch() == epoch && s.devices.admission.accepts(id)
                });
                if !active {
                    break;
                }
                let waiting = client.call_devices(Action::PreviewFrame { preview_id: id });
                match crate::platform::with_timeout(waiting, Duration::from_secs(5)).await {
                    Some(Ok(Ok(Reply::Preview {
                        preview_id,
                        mode,
                        frame,
                    }))) if preview_id == id => {
                        let image = if let Some(frame) = frame {
                            if frame.preview_id != id {
                                failure = Some("Preview returned a different identity".into());
                                continue;
                            }
                            let decoder = decoder.clone();
                            cx.background_spawn(async move {
                                decoder
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .decode(&frame)
                            })
                            .await
                        } else {
                            Ok(None)
                        };
                        match image {
                            Ok(image) => {
                                settings.update(cx, |s, cx| {
                                    if s.epoch() != epoch || !s.devices.admission.accepts(id) {
                                        return;
                                    }
                                    s.devices.effective = mode;
                                    if let Some(image) = image {
                                        s.devices.image = Some(image);
                                    }
                                    s.devices.status =
                                        "Preview only. Your camera is not being sent to anyone."
                                            .into();
                                    cx.notify();
                                });
                            }
                            Err(e) => {
                                failure = Some(e);
                            }
                        }
                    }
                    Some(Ok(Err(e))) => failure = Some(e),
                    _ => failure = Some("Camera preview stopped responding".into()),
                }
                crate::platform::sleep(Duration::from_millis(30)).await;
            }
            let waiting = client.call_devices(Action::CancelPreview { preview_id: id });
            let stopped = matches!(
                crate::platform::with_timeout(waiting, Duration::from_secs(7)).await,
                Some(Ok(Ok(Reply::Stopped { .. })))
            );
            settings.update(cx, |s, cx| {
                if s.epoch() != epoch || !s.devices.admission.accepts(id) {
                    return;
                }
                s.devices.finish_preview(id, stopped, failure);
                cx.notify();
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod preview_tests {
    use super::*;
    #[test]
    fn unloaded_preferences_cannot_be_saved() {
        let mut ui = DeviceUi::default();
        assert!(!ui.may_save());
        ui.preferences_ready = true;
        assert!(ui.may_save());
        ui.busy = true;
        assert!(!ui.may_save());
    }
    #[test]
    fn refresh_waits_for_the_save_reply_and_does_not_hide_failure() {
        let mut ui = DeviceUi::default();
        let save_request = ui.begin_request(false).unwrap();
        assert!(ui.begin_request(true).is_none());
        assert_eq!(ui.request, save_request);
        assert!(ui.busy);
        assert!(
            !ui.finish_request(false),
            "failure must remain visible instead of starting refresh"
        );
        assert!(!ui.busy);
        ui.begin_request(false).unwrap();
        assert!(ui.begin_request(true).is_none());
        assert!(ui.finish_request(true), "a successful save can now refresh");
    }
    #[test]
    fn failed_cleanup_retains_stopping_ownership_and_retry() {
        let mut ui = DeviceUi::default();
        let id = ui.admission.begin().unwrap();
        ui.preview = Some(id);
        ui.finish_preview(id, false, Some("Camera timed out.".into()));
        assert_eq!(ui.stopping, Some(id));
        assert!(ui.preview.is_none());
        assert!(ui.status.contains("still stopping"));
    }
    #[test]
    fn successful_cleanup_releases_ownership() {
        let mut ui = DeviceUi::default();
        let id = ui.admission.begin().unwrap();
        ui.preview = Some(id);
        ui.finish_preview(id, true, None);
        assert!(ui.stopping.is_none());
        assert!(ui.preview.is_none());
    }
}
