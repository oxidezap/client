//! Device work is independent of network calls and never blocks the bridge.
use super::Outbox;
use oxidezap_core::{
    CallDeviceInventory, CallDevicePreferences, CallDeviceSettings, CameraMode, CameraPreviewId,
};
use oxidezap_ipc::{
    CallDeviceAction as Action, CallDeviceReply as Reply, CameraPreviewFrame, DaemonMessage,
    RequestId,
};
use portable_atomic::AtomicU64;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

#[cfg(not(target_family = "wasm"))]
#[path = "call_devices/native.rs"]
mod imp;
#[cfg(target_family = "wasm")]
#[path = "call_devices/web.rs"]
mod imp;
use imp::{persist, read_preferences, test_microphone};

static PREFERENCE_ERROR: Mutex<Option<String>> = Mutex::new(None);
static PREFERENCE_WRITE: Mutex<()> = Mutex::new(());

pub(super) struct Devices {
    state: Arc<Mutex<State>>,
    in_flight: Arc<tokio::sync::Semaphore>,
}
impl Default for Devices {
    fn default() -> Self {
        Self {
            state: Default::default(),
            in_flight: Arc::new(tokio::sync::Semaphore::new(2)),
        }
    }
}
impl Drop for Devices {
    fn drop(&mut self) {
        if let Some(p) = &self.state.lock().unwrap_or_else(|e| e.into_inner()).preview {
            p.cancel.store(true, Ordering::Release);
        }
    }
}
#[derive(Default)]
struct State {
    preview: Option<Preview>,
    fences: Vec<PreviewFence>,
    call_busy: bool,
}
struct PreviewFence {
    owner: Outbox,
    started: u64,
    retired: u64,
}
impl State {
    fn fence(&mut self, owner: &Outbox) -> &mut PreviewFence {
        self.fences.retain(|f| !f.owner.is_closed());
        let index = self
            .fences
            .iter()
            .position(|f| f.owner.same_channel(owner))
            .unwrap_or_else(|| {
                self.fences.push(PreviewFence {
                    owner: owner.clone(),
                    started: 0,
                    retired: 0,
                });
                self.fences.len() - 1
            });
        &mut self.fences[index]
    }
    fn retire(&mut self, owner: &Outbox, id: CameraPreviewId) {
        let fence = self.fence(owner);
        fence.retired = fence.retired.max(id.0);
        if let Some(p) = self
            .preview
            .as_ref()
            .filter(|p| p.owner.same_channel(owner) && p.id.0 <= id.0)
        {
            p.cancel.store(true, Ordering::Release);
        }
    }
    fn reserve_preview(
        &mut self,
        owner: &Outbox,
        id: CameraPreviewId,
    ) -> Result<Arc<AtomicBool>, String> {
        if self.call_busy {
            return Err("End the current call before opening a camera preview.".into());
        }
        if owner.is_closed() {
            return Err("Preview connection is closed".into());
        }
        let fence = self.fence(owner);
        if id.0 <= fence.retired || id.0 <= fence.started {
            return Err("Preview identity was already stopped or used".into());
        }
        if self.preview.as_ref().is_some_and(|p| !p.finished) {
            return Err("A camera preview is already open. Close it first.".into());
        }
        self.fence(owner).started = id.0;
        let cancel = Arc::new(AtomicBool::new(false));
        self.preview = Some(Preview {
            id,
            owner: owner.clone(),
            cancel: cancel.clone(),
            frame: None,
            mode: None,
            failure: None,
            finished: false,
            waiting_keyframe: true,
        });
        Ok(cancel)
    }
}
struct Preview {
    id: CameraPreviewId,
    owner: Outbox,
    cancel: Arc<AtomicBool>,
    frame: Option<CameraPreviewFrame>,
    mode: Option<CameraMode>,
    failure: Option<String>,
    finished: bool,
    waiting_keyframe: bool,
}
impl Drop for Preview {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}

fn quality(p: &CallDevicePreferences) -> oxidezap_video::VideoQuality {
    quality_with_environment(p, oxidezap_video::VideoQuality::from_environment())
}
fn quality_with_environment(
    p: &CallDevicePreferences,
    environment: oxidezap_video::VideoQuality,
) -> oxidezap_video::VideoQuality {
    oxidezap_video::VideoQuality {
        width: p.camera_mode.width,
        height: p.camera_mode.height,
        fps: p.camera_mode.fps,
        ..environment
    }
}
fn camera_device(camera: Option<String>, saved: bool) -> Option<String> {
    camera.or_else(|| saved.then(|| "index:0".to_owned()))
}

fn apply(p: CallDevicePreferences, saved: bool) -> Result<(), String> {
    oxidezap_session::set_camera_preferences(camera_device(p.camera.clone(), saved), quality(&p))?;
    if saved {
        oxidezap_audio::devices::set_call_preferences(p);
    } else {
        oxidezap_audio::devices::set_call_preferences_from_defaults(p);
    }
    Ok(())
}

fn initial_preferences(
    saved: Option<CallDevicePreferences>,
    environment: oxidezap_video::VideoQuality,
) -> CallDevicePreferences {
    saved.unwrap_or_else(|| CallDevicePreferences {
        camera_mode: CameraMode {
            width: environment.width,
            height: environment.height,
            fps: environment.fps,
        },
        ..Default::default()
    })
}

pub(super) async fn load_preferences() {
    let outcome = oxidezap_session::unblock(|| {
        let _write = PREFERENCE_WRITE.lock().unwrap_or_else(|e| e.into_inner());
        let outcome = read_preferences().and_then(|saved| {
            let explicit = saved.is_some();
            apply(
                initial_preferences(saved, oxidezap_video::VideoQuality::from_environment()),
                explicit,
            )
        });
        *PREFERENCE_ERROR.lock().unwrap_or_else(|e| e.into_inner()) =
            outcome.as_ref().err().cloned();
        outcome
    })
    .await;
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(error)) => log::warn!("call device preferences could not load: {error}"),
        Err(error) => {
            log::warn!("call device preferences task failed: {error}");
            *PREFERENCE_ERROR.lock().unwrap_or_else(|e| e.into_inner()) = Some(error.to_string());
        }
    }
}

async fn ensure_preferences_readable() -> Result<(), String> {
    let retry = PREFERENCE_ERROR
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some();
    if retry {
        load_preferences().await;
    }
    preferences_readable()
}

fn preferences_readable() -> Result<(), String> {
    if let Some(error) = &*PREFERENCE_ERROR.lock().unwrap_or_else(|e| e.into_inner()) {
        Err(format!(
            "Could not load saved call devices: {error}. Fix the saved configuration and refresh devices."
        ))
    } else {
        Ok(())
    }
}

async fn inventory() -> CallDeviceSettings {
    static REVISION: AtomicU64 = AtomicU64::new(0);
    let mut inventory = CallDeviceInventory {
        revision: REVISION.fetch_add(1, Ordering::Relaxed) + 1,
        ..Default::default()
    };
    match oxidezap_session::unblock(oxidezap_audio::devices::enumerate_audio_devices).await {
        Ok(Ok((microphones, speakers))) => {
            inventory.microphones = microphones;
            inventory.speakers = speakers;
        }
        Ok(Err(e)) => inventory.failures.push(format!("Audio inventory: {e}")),
        Err(e) => inventory
            .failures
            .push(format!("Audio inventory task: {e}")),
    }
    match oxidezap_video::enumerate_cameras().await {
        Ok(cameras) => {
            inventory.cameras = cameras
                .into_iter()
                .map(|d| oxidezap_core::CallDevice {
                    id: d.id,
                    label: d.label,
                    is_default: d.is_default,
                    modes: d
                        .modes
                        .into_iter()
                        .map(|m| CameraMode {
                            width: m.width,
                            height: m.height,
                            fps: m.fps,
                        })
                        .collect(),
                })
                .collect()
        }
        Err(e) => inventory.failures.push(format!("Camera inventory: {e}")),
    }
    CallDeviceSettings {
        inventory,
        preferences: oxidezap_audio::devices::call_preferences(),
    }
}

impl Devices {
    pub(super) fn set_call_busy(&mut self, busy: bool) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .call_busy = busy;
    }

    /// Fence queued starts, including previews owned by another window.
    /// Refuse until capture acknowledges stop; keep the bridge responsive.
    pub(super) fn release_for_call(&mut self) -> Result<(), String> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.call_busy = true;
        if let Some(preview) = &state.preview {
            preview.cancel.store(true, Ordering::Release);
            if !preview.finished {
                return Err(
                    "Camera is still stopping. Try the call again when preview closes.".into(),
                );
            }
        }
        Ok(())
    }

    pub(super) fn request(&mut self, id: RequestId, action: Action, owner: Outbox) {
        let state = self.state.clone();
        let permit = if matches!(
            action,
            Action::Inventory
                | Action::Save { .. }
                | Action::TestMicrophone
                | Action::TestSpeaker
                | Action::TestRingtone
        ) {
            match self.in_flight.clone().try_acquire_owned() {
                Ok(permit) => Some(permit),
                Err(_) => {
                    let result =
                        Err("Device setup is busy. Wait for the current device operation.".into());
                    if let Ok(frame) =
                        serde_json::to_string(&DaemonMessage::CallDevices { id, result })
                    {
                        oxidezap_session::spawn(async move {
                            let _ = owner.send(frame).await;
                        });
                    }
                    return;
                }
            }
        } else {
            None
        };
        oxidezap_session::spawn(async move {
            let _permit = permit;
            let result = handle(state, action, &owner).await.map(Box::new);
            if let Ok(frame) = serde_json::to_string(&DaemonMessage::CallDevices { id, result }) {
                let _ = owner.send(frame).await;
            }
        });
    }
}

async fn handle(state: Arc<Mutex<State>>, action: Action, owner: &Outbox) -> Result<Reply, String> {
    match action {
        Action::Preferences => {
            ensure_preferences_readable().await?;
            Ok(Reply::Preferences {
                preferences: oxidezap_audio::devices::call_preferences(),
            })
        }
        Action::Inventory => {
            ensure_preferences_readable().await?;
            Ok(Reply::Settings {
                settings: inventory().await,
            })
        }
        Action::Save { preferences } => {
            quality(&preferences).checked().map_err(|e| e.to_string())?;
            oxidezap_session::unblock(move || {
                let _guard = PREFERENCE_WRITE.lock().unwrap_or_else(|e| e.into_inner());
                persist(&preferences)?;
                apply(preferences, true)?;
                *PREFERENCE_ERROR.lock().unwrap_or_else(|e| e.into_inner()) = None;
                Ok::<(), String>(())
            })
            .await
            .map_err(|e| e.to_string())??;
            Ok(Reply::Preferences {
                preferences: oxidezap_audio::devices::call_preferences(),
            })
        }
        Action::TestSpeaker | Action::TestRingtone => {
            let p = oxidezap_audio::devices::call_preferences();
            let (output, tone) = if matches!(action, Action::TestSpeaker) {
                (p.speaker, oxidezap_core::Ringtone::Gentle)
            } else {
                (p.ringtone_output, p.ringtone)
            };
            let playback =
                oxidezap_session::unblock(move || oxidezap_audio::start_ringtone(output, tone))
                    .await
                    .map_err(|e| e.to_string())?
                    .map_err(|e| e.to_string())?;
            oxidezap_session::sleep(std::time::Duration::from_millis(1200)).await;
            drop(playback);
            Ok(Reply::Tested {
                detail: "Output test finished".into(),
            })
        }
        Action::TestMicrophone => test_microphone().await,
        Action::CancelPreview { preview_id } => {
            state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retire(owner, preview_id);
            for _ in 0..50 {
                let finished = {
                    let state = state.lock().unwrap_or_else(|e| e.into_inner());
                    !state.preview.as_ref().is_some_and(|p| {
                        p.id.0 <= preview_id.0 && p.owner.same_channel(owner) && !p.finished
                    })
                };
                if finished {
                    return Ok(Reply::Stopped { preview_id });
                }
                oxidezap_session::sleep(std::time::Duration::from_millis(100)).await;
            }
            Err("Camera is still stopping. Wait before starting another preview or call.".into())
        }
        Action::PreviewFrame { preview_id } => {
            let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
            let p = state
                .preview
                .as_mut()
                .filter(|p| p.id == preview_id && p.owner.same_channel(owner))
                .ok_or("Preview ended")?;
            if let Some(e) = &p.failure {
                return Err(e.clone());
            }
            Ok(Reply::Preview {
                preview_id,
                mode: p.mode,
                frame: p.frame.take(),
            })
        }
        Action::StartPreview { preview_id } => {
            let cancel = state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .reserve_preview(owner, preview_id)?;
            let state_pump = state.clone();
            let owner = owner.clone();
            oxidezap_session::spawn(async move {
                preview(state_pump, preview_id, owner, cancel).await;
            });
            Ok(Reply::Preview {
                preview_id,
                mode: None,
                frame: None,
            })
        }
    }
}

async fn preview(
    state: Arc<Mutex<State>>,
    id: CameraPreviewId,
    owner: Outbox,
    cancel: Arc<AtomicBool>,
) {
    if cancel.load(Ordering::Acquire) || owner.is_closed() {
        let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(p) = state
            .preview
            .as_mut()
            .filter(|p| p.id == id && p.owner.same_channel(&owner))
        {
            p.finished = true;
        }
        return;
    }
    let (p, saved) = oxidezap_audio::devices::call_preferences_with_origin();
    let opened = oxidezap_session::CameraPreview::open(
        id,
        quality(&p),
        camera_device(p.camera.clone(), saved),
    )
    .await;
    match opened {
        Ok(preview) => {
            if cancel.load(Ordering::Acquire) || owner.is_closed() {
                preview.stop().await;
                let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(p) = state
                    .preview
                    .as_mut()
                    .filter(|p| p.id == id && p.owner.same_channel(&owner))
                {
                    p.finished = true;
                }
                return;
            }
            let frames = preview.frames();
            let q = preview.quality();
            {
                let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(p) = state
                    .preview
                    .as_mut()
                    .filter(|p| p.id == id && p.owner.same_channel(&owner))
                {
                    p.mode = Some(CameraMode {
                        width: q.width,
                        height: q.height,
                        fps: q.fps,
                    });
                }
            }
            while !cancel.load(Ordering::Acquire) && !owner.is_closed() {
                let received = oxidezap_session::with_timeout(
                    frames.recv(),
                    std::time::Duration::from_millis(100),
                )
                .await;
                if let Some(Ok(frame)) = received {
                    let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(p) = state
                        .preview
                        .as_mut()
                        .filter(|p| p.id == id && p.owner.same_channel(&owner))
                    {
                        if p.frame.is_some() {
                            p.frame = None;
                            p.waiting_keyframe = true;
                            preview.request_keyframe();
                        }
                        if !p.waiting_keyframe || frame.keyframe {
                            p.waiting_keyframe = false;
                            p.frame = Some(CameraPreviewFrame {
                                preview_id: id,
                                data: frame.data,
                                keyframe: frame.keyframe,
                            });
                        } else {
                            preview.request_keyframe();
                        }
                    }
                } else if frames.is_closed() {
                    break;
                }
            }
            preview.stop().await;
            let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(p) = state
                .preview
                .as_mut()
                .filter(|p| p.id == id && p.owner.same_channel(&owner))
            {
                p.finished = true;
                if !cancel.load(Ordering::Acquire) {
                    p.failure = Some("Camera disconnected or stopped".into());
                }
            }
        }
        Err(e) => {
            let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(p) = state
                .preview
                .as_mut()
                .filter(|p| p.id == id && p.owner.same_channel(&owner))
            {
                p.failure = Some(e);
                p.finished = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn saved_camera_default_bypasses_legacy_configuration() {
        assert_eq!(camera_device(None, true), Some("index:0".into()));
        assert_eq!(camera_device(None, false), None);
        assert_eq!(
            camera_device(Some("fixture-camera".into()), true),
            Some("fixture-camera".into())
        );
    }
    #[test]
    fn missing_preferences_preserve_environment_mode_and_bitrate() {
        let environment = oxidezap_video::VideoQuality {
            width: 800,
            height: 600,
            fps: 15,
            bitrate_kbps: 987,
        };
        let p = initial_preferences(None, environment);
        assert_eq!(quality_with_environment(&p, environment), environment);
        let saved = CallDevicePreferences {
            camera_mode: CameraMode {
                width: 640,
                height: 480,
                fps: 30,
            },
            ..Default::default()
        };
        let p = initial_preferences(Some(saved.clone()), environment);
        assert_eq!(p, saved);
        assert_eq!(quality_with_environment(&p, environment).bitrate_kbps, 987);
    }
    #[tokio::test]
    async fn stopped_acknowledgement_fences_a_start_that_has_not_run_yet() {
        let (owner, _rx) = tokio::sync::mpsc::channel(4);
        let (other, _rx2) = tokio::sync::mpsc::channel(4);
        let state = Arc::new(Mutex::new(State::default()));
        let id = CameraPreviewId(1);
        assert!(matches!(
            handle(
                state.clone(),
                Action::CancelPreview { preview_id: id },
                &owner
            )
            .await
            .unwrap(),
            Reply::Stopped { .. }
        ));
        let mut state = state.lock().unwrap();
        assert!(
            state.reserve_preview(&owner, id).is_err(),
            "late start must be refused before opening hardware"
        );
        assert!(state.preview.is_none());
        assert!(
            state.reserve_preview(&other, id).is_ok(),
            "retirement belongs to its connection"
        );
    }
    #[test]
    fn preview_id_cannot_be_reused_after_completion() {
        let (owner, _rx) = tokio::sync::mpsc::channel(4);
        let mut state = State::default();
        state.reserve_preview(&owner, CameraPreviewId(1)).unwrap();
        state.preview.as_mut().unwrap().finished = true;
        assert!(state.reserve_preview(&owner, CameraPreviewId(1)).is_err());
        assert!(state.reserve_preview(&owner, CameraPreviewId(2)).is_ok());
    }
    fn fixture(owner: Outbox) -> (Arc<Mutex<State>>, Arc<AtomicBool>) {
        let cancel = Arc::new(AtomicBool::new(false));
        let state = State {
            preview: Some(Preview {
                id: CameraPreviewId(7),
                owner,
                cancel: cancel.clone(),
                frame: None,
                mode: None,
                failure: None,
                finished: true,
                waiting_keyframe: true,
            }),
            ..Default::default()
        };
        (Arc::new(Mutex::new(state)), cancel)
    }
    #[test]
    fn call_admission_fences_other_windows_until_capture_has_stopped() {
        let (owner, _rx) = tokio::sync::mpsc::channel(4);
        let (other, _rx2) = tokio::sync::mpsc::channel(4);
        let mut devices = Devices::default();
        let cancel = devices
            .state
            .lock()
            .unwrap()
            .reserve_preview(&owner, CameraPreviewId(1))
            .unwrap();
        assert!(devices.release_for_call().is_err());
        assert!(cancel.load(Ordering::Acquire));
        assert!(
            devices
                .state
                .lock()
                .unwrap()
                .reserve_preview(&other, CameraPreviewId(1))
                .is_err()
        );
        devices
            .state
            .lock()
            .unwrap()
            .preview
            .as_mut()
            .unwrap()
            .finished = true;
        assert!(devices.release_for_call().is_ok());
        // The reservation remains after acknowledgement, before the call is marked busy.
        assert!(
            devices
                .state
                .lock()
                .unwrap()
                .reserve_preview(&other, CameraPreviewId(2))
                .is_err()
        );
        devices.set_call_busy(true);
        assert!(
            devices
                .state
                .lock()
                .unwrap()
                .reserve_preview(&other, CameraPreviewId(3))
                .is_err()
        );
        devices.set_call_busy(false);
        assert!(
            devices
                .state
                .lock()
                .unwrap()
                .reserve_preview(&other, CameraPreviewId(4))
                .is_ok()
        );
    }
    #[tokio::test]
    async fn cancellation_is_scoped_to_owner_and_preview_identity() {
        let (owner, _rx) = tokio::sync::mpsc::channel(4);
        let (stranger, _rx2) = tokio::sync::mpsc::channel(4);
        let (state, cancelled) = fixture(owner.clone());
        handle(
            state.clone(),
            Action::CancelPreview {
                preview_id: CameraPreviewId(7),
            },
            &stranger,
        )
        .await
        .unwrap();
        assert!(!cancelled.load(Ordering::Acquire));
        handle(
            state.clone(),
            Action::CancelPreview {
                preview_id: CameraPreviewId(6),
            },
            &owner,
        )
        .await
        .unwrap();
        assert!(!cancelled.load(Ordering::Acquire));
        handle(
            state,
            Action::CancelPreview {
                preview_id: CameraPreviewId(7),
            },
            &owner,
        )
        .await
        .unwrap();
        assert!(cancelled.load(Ordering::Acquire));
    }
    #[tokio::test]
    async fn another_frontend_cannot_read_preview_frames() {
        let (owner, _rx) = tokio::sync::mpsc::channel(4);
        let (stranger, _rx2) = tokio::sync::mpsc::channel(4);
        let (state, _) = fixture(owner);
        assert!(
            handle(
                state,
                Action::PreviewFrame {
                    preview_id: CameraPreviewId(7)
                },
                &stranger
            )
            .await
            .is_err()
        );
    }
}
