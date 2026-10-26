//! Persistent machine settings and a microphone-only test.
use super::{CallDevicePreferences, Reply};
use std::path::{Path, PathBuf};

fn directory_from(
    windows: bool,
    local: Option<std::ffi::OsString>,
    xdg: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    let nonempty = |value: std::ffi::OsString| (!value.is_empty()).then(|| PathBuf::from(value));
    let base = if windows {
        local.and_then(nonempty)
    } else {
        xdg.and_then(nonempty)
            .or_else(|| home.and_then(nonempty).map(|p| p.join(".config")))
    }?;
    Some(base.join("oxidezap"))
}
fn directory() -> Result<PathBuf, String> {
    directory_from(
        cfg!(windows),
        std::env::var_os("LOCALAPPDATA"),
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
    .ok_or_else(|| "No persistent configuration directory for call devices".into())
}

pub(super) fn read_preferences() -> Result<Option<CallDevicePreferences>, String> {
    read_at(&directory()?.join("call-devices.json"))
}
fn read_at(path: &Path) -> Result<Option<CallDevicePreferences>, String> {
    match std::fs::read(path) {
        Ok(raw) => serde_json::from_slice(&raw)
            .map(Some)
            .map_err(|e| e.to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}
pub(super) fn persist(p: &CallDevicePreferences) -> Result<(), String> {
    persist_at(&directory()?, p)
}
fn persist_at(directory: &Path, p: &CallDevicePreferences) -> Result<(), String> {
    use std::io::Write as _;
    std::fs::create_dir_all(directory).map_err(|e| e.to_string())?;
    let bytes = serde_json::to_vec(p).map_err(|e| e.to_string())?;
    let path = directory.join("call-devices.json");
    let temporary = directory.join("call-devices.json.new");
    let mut file = std::fs::File::create(&temporary).map_err(|e| e.to_string())?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| e.to_string())?;
    std::fs::rename(&temporary, path).map_err(|e| e.to_string())
}

pub(super) async fn test_microphone() -> Result<Reply, String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn configuration_paths_survive_runtime_directory_cleanup() {
        assert_eq!(
            directory_from(
                false,
                None,
                Some("/persistent/config".into()),
                Some("/home/test".into())
            ),
            Some(PathBuf::from("/persistent/config/oxidezap"))
        );
        assert_eq!(
            directory_from(false, None, None, Some("/home/test".into())),
            Some(PathBuf::from("/home/test/.config/oxidezap"))
        );
        assert_eq!(
            directory_from(true, Some("C:/Users/test/AppData/Local".into()), None, None),
            Some(PathBuf::from("C:/Users/test/AppData/Local/oxidezap"))
        );
    }
    #[test]
    fn saved_preferences_roundtrip_and_replace_previous_values() {
        static NEXT: super::super::AtomicU64 = super::super::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "oxidezap-call-devices-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        let first = CallDevicePreferences {
            microphone: Some("fixture-input".into()),
            ringtone: oxidezap_core::Ringtone::Silent,
            ..Default::default()
        };
        persist_at(&path, &first).unwrap();
        assert_eq!(
            read_at(&path.join("call-devices.json")).unwrap(),
            Some(first)
        );
        let second = CallDevicePreferences {
            camera: Some("fixture-camera".into()),
            ..Default::default()
        };
        persist_at(&path, &second).unwrap();
        assert_eq!(
            read_at(&path.join("call-devices.json")).unwrap(),
            Some(second)
        );
        std::fs::remove_dir_all(path).unwrap();
    }
}
