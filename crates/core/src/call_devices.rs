//! Device preferences and preview identity. These values never open hardware.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallDevice {
    pub id: String,
    pub label: String,
    pub is_default: bool,
    #[serde(default)]
    pub modes: Vec<CameraMode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CameraMode {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl Default for CameraMode {
    fn default() -> Self {
        Self {
            width: 640,
            height: 480,
            fps: 30,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ringtone {
    Silent,
    #[default]
    Classic,
    Gentle,
}

/// None selects the system default. Missing saved IDs must remain visible as
/// unavailable, rather than silently replacing the user's saved preference.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CallDevicePreferences {
    pub microphone: Option<String>,
    pub speaker: Option<String>,
    pub ringtone_output: Option<String>,
    pub camera: Option<String>,
    pub camera_mode: CameraMode,
    pub ringtone: Ringtone,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CallDeviceInventory {
    pub revision: u64,
    pub microphones: Vec<CallDevice>,
    pub speakers: Vec<CallDevice>,
    pub cameras: Vec<CallDevice>,
    pub failures: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveCallDevices {
    pub microphone: Option<String>,
    pub speaker: Option<String>,
    pub camera: Option<String>,
    pub camera_mode: Option<CameraMode>,
    pub failures: Vec<String>,
}

/// Opaque identity belonging to one preview, never to a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CameraPreviewId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallDeviceSettings {
    pub inventory: CallDeviceInventory,
    pub preferences: CallDevicePreferences,
}

/// Tracks admission independently from completion. Cancellation invalidates
/// work still opening a device; its eventual stream must be dropped by owner.
#[derive(Debug, Default)]
pub struct PreviewAdmission {
    current: Option<CameraPreviewId>,
    next: u64,
}
impl PreviewAdmission {
    pub fn begin(&mut self) -> Option<CameraPreviewId> {
        self.next = self.next.checked_add(1)?;
        let id = CameraPreviewId(self.next);
        self.current = Some(id);
        Some(id)
    }
    pub fn accepts(&self, id: CameraPreviewId) -> bool {
        self.current == Some(id)
    }
    pub fn cancel(&mut self, id: CameraPreviewId) -> bool {
        if !self.accepts(id) {
            return false;
        }
        self.current = None;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancelled_and_superseded_completions_are_rejected() {
        let mut admission = PreviewAdmission::default();
        let first = admission.begin().unwrap();
        assert!(admission.cancel(first));
        assert!(!admission.accepts(first));
        let second = admission.begin().unwrap();
        assert_ne!(first, second);
        assert!(!admission.cancel(first));
        assert!(admission.accepts(second));
        let third = admission.begin().unwrap();
        assert!(!admission.accepts(second));
        assert!(admission.accepts(third));
    }
    #[test]
    fn old_settings_keep_new_defaults() {
        let prefs: CallDevicePreferences =
            serde_json::from_str(r#"{"speaker":"missing-id"}"#).unwrap();
        assert_eq!(prefs.speaker.as_deref(), Some("missing-id"));
        assert_eq!(prefs.camera_mode, CameraMode::default());
        assert_eq!(prefs.ringtone, Ringtone::Classic);
    }
}
