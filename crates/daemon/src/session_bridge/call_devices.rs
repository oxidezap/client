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
fn apply(p: CallDevicePreferences) -> Result<(), String> {
    oxidezap_session::set_camera_preferences(p.camera.clone(), quality(&p))?;
    oxidezap_audio::devices::set_call_preferences(p);
    Ok(())
}

fn initial_preferences(
    saved: Option<CallDevicePreferences>,
    environment: oxidezap_video::VideoQuality,
) -> (CallDevicePreferences, bool) {
    match saved {
        Some(p) => (p, true),
        None => (
            CallDevicePreferences {
                camera_mode: CameraMode {
                    width: environment.width,
                    height: environment.height,
                    fps: environment.fps,
                },
                ..Default::default()
            },
            false,
        ),
    }
}

pub(super) async fn load_preferences() {
    let outcome = match oxidezap_session::unblock(read_preferences).await {
        Ok(Ok(saved)) => {
            let (preferences, explicit) =
                initial_preferences(saved, oxidezap_video::VideoQuality::from_environment());
            if explicit {
                apply(preferences)
            } else {
                oxidezap_audio::devices::set_call_preferences(preferences);
                Ok(())
            }
        }
        Ok(Err(e)) => Err(e),
        Err(e) => Err(e.to_string()),
    };
    if let Err(error) = &outcome {
        log::warn!("call device preferences could not load: {error}");
    }
    *PREFERENCE_ERROR.lock().unwrap_or_else(|e| e.into_inner()) = outcome.err();
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
            let result = handle(state, action, &owner).await;
            if let Ok(frame) = serde_json::to_string(&DaemonMessage::CallDevices { id, result }) {
                let _ = owner.send(frame).await;
            }
        });
    }
}

async fn handle(state: Arc<Mutex<State>>, action: Action, owner: &Outbox) -> Result<Reply, String> {
    match action {
        Action::Preferences => {
            preferences_readable()?;
            Ok(Reply::Preferences {
                preferences: oxidezap_audio::devices::call_preferences(),
            })
        }
        Action::Inventory => {
            preferences_readable()?;
            Ok(Reply::Settings {
                settings: inventory().await,
            })
        }
        Action::Save { preferences } => {
            quality(&preferences).checked().map_err(|e| e.to_string())?;
            static SAVE: Mutex<()> = Mutex::new(());
            oxidezap_session::unblock(move || {
                let _guard = SAVE.lock().unwrap_or_else(|e| e.into_inner());
                persist(&preferences)?;
                apply(preferences)?;
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
    let p = oxidezap_audio::devices::call_preferences();
    let opened = oxidezap_session::CameraPreview::open(id, quality(&p), p.camera).await;
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
    fn missing_preferences_preserve_environment_mode_and_bitrate() {
        let environment = oxidezap_video::VideoQuality {
            width: 800,
            height: 600,
            fps: 15,
            bitrate_kbps: 987,
        };
        let (p, explicit) = initial_preferences(None, environment);
        assert!(
            !explicit,
            "an absent file must not install a camera override"
        );
        assert_eq!(quality_with_environment(&p, environment), environment);
        let saved = CallDevicePreferences {
            camera_mode: CameraMode {
                width: 640,
                height: 480,
                fps: 30,
            },
            ..Default::default()
        };
        let (p, explicit) = initial_preferences(Some(saved.clone()), environment);
        assert!(explicit);
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
