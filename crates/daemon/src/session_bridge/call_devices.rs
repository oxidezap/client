//! Device work is independent of network calls and never blocks the bridge.
use super::Outbox;
use oxidezap_core::{
    CallDeviceInventory, CallDevicePreferences, CallDeviceSettings, CameraMode, CameraPreviewId,
};
use oxidezap_ipc::{
    CallDeviceAction as Action, CallDeviceReply as Reply, CameraPreviewFrame, DaemonMessage,
    RequestId,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

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
    oxidezap_video::VideoQuality {
        width: p.camera_mode.width,
        height: p.camera_mode.height,
        fps: p.camera_mode.fps,
        ..Default::default()
    }
}
fn apply(p: CallDevicePreferences) -> Result<(), String> {
    oxidezap_session::set_camera_preferences(p.camera.clone(), quality(&p))?;
    oxidezap_audio::devices::set_call_preferences(p);
    Ok(())
}

pub(super) async fn load_preferences() {
    match oxidezap_session::unblock(read_preferences).await {
        Ok(Ok(p)) => {
            if let Err(e) = apply(p) {
                log::warn!("call device preferences could not apply: {e}");
            }
        }
        Ok(Err(e)) => log::warn!("call device preferences could not load: {e}"),
        Err(e) => log::warn!("call device preference task failed: {e}"),
    }
}

#[cfg(not(target_family = "wasm"))]
fn read_preferences() -> Result<CallDevicePreferences, String> {
    let path = oxidezap_ipc::state_dir()
        .ok_or("no device preference directory")?
        .join("call-devices.json");
    match std::fs::read(path) {
        Ok(raw) => serde_json::from_slice(&raw).map_err(|e| e.to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
        Err(e) => Err(e.to_string()),
    }
}
#[cfg(target_family = "wasm")]
fn read_preferences() -> Result<CallDevicePreferences, String> {
    Ok(Default::default())
}

#[cfg(not(target_family = "wasm"))]
fn persist(p: &CallDevicePreferences) -> Result<(), String> {
    use std::io::Write as _;
    let directory = oxidezap_ipc::state_dir().ok_or("no device preference directory")?;
    let bytes = serde_json::to_vec(p).map_err(|e| e.to_string())?;
    let path = directory.join("call-devices.json");
    let temporary = directory.join("call-devices.json.new");
    let mut file = std::fs::File::create(&temporary).map_err(|e| e.to_string())?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| e.to_string())?;
    std::fs::rename(&temporary, path).map_err(|e| e.to_string())
}
#[cfg(target_family = "wasm")]
fn persist(_p: &CallDevicePreferences) -> Result<(), String> {
    Err("Device preferences cannot be saved from this browser connection".into())
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
        Action::Preferences => Ok(Reply::Preferences {
            preferences: oxidezap_audio::devices::call_preferences(),
        }),
        Action::Inventory => Ok(Reply::Settings {
            settings: inventory().await,
        }),
        Action::Save { preferences } => {
            quality(&preferences).checked().map_err(|e| e.to_string())?;
            static SAVE: Mutex<()> = Mutex::new(());
            oxidezap_session::unblock(move || {
                let _guard = SAVE.lock().unwrap_or_else(|e| e.into_inner());
                persist(&preferences)?;
                apply(preferences)
            })
            .await
            .map_err(|e| e.to_string())??;
            Ok(Reply::Settings {
                settings: inventory().await,
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
            {
                let state = state.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(p) = state
                    .preview
                    .as_ref()
                    .filter(|p| p.id == preview_id && p.owner.same_channel(owner))
                {
                    p.cancel.store(true, Ordering::Release);
                }
            }
            for _ in 0..50 {
                let finished = {
                    let state = state.lock().unwrap_or_else(|e| e.into_inner());
                    !state.preview.as_ref().is_some_and(|p| {
                        p.id == preview_id && p.owner.same_channel(owner) && !p.finished
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
            let cancel = Arc::new(AtomicBool::new(false));
            {
                let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                if state.preview.as_ref().is_some_and(|p| !p.finished) {
                    return Err("A camera preview is already open. Close it first.".into());
                }
                state.preview = Some(Preview {
                    id: preview_id,
                    owner: owner.clone(),
                    cancel: cancel.clone(),
                    frame: None,
                    mode: None,
                    failure: None,
                    finished: false,
                    waiting_keyframe: true,
                });
            }
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

#[cfg(not(target_family = "wasm"))]
async fn test_microphone() -> Result<Reply, String> {
    let mic = oxidezap_audio::devices::open_test_microphone()
        .await
        .map_err(|e| e.to_string())?;
    let mut peak = 0u16;
    for _ in 0..25 {
        match oxidezap_session::with_timeout(mic.recv(), std::time::Duration::from_millis(200))
            .await
        {
            Some(Ok(frame)) => {
                peak = peak.max(frame.into_iter().map(i16::unsigned_abs).max().unwrap_or(0));
            }
            _ => return Err("Microphone stopped or did not deliver samples".into()),
        }
    }
    Ok(Reply::Tested {
        detail: if peak == 0 {
            "Microphone returned silence. Check microphone privacy and input selection.".into()
        } else {
            format!(
                "Microphone received audio, peak {}%",
                u32::from(peak) * 100 / 32768
            )
        },
    })
}
#[cfg(target_family = "wasm")]
async fn test_microphone() -> Result<Reply, String> {
    Err("Microphone testing is unavailable from this browser connection".into())
}

#[cfg(test)]
mod tests {
    use super::*;
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
                preview_id: CameraPreviewId(8),
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
