//! Browser connection capabilities for machine device settings.
use super::{CallDevicePreferences, Reply};
pub(super) fn read_preferences() -> Result<Option<CallDevicePreferences>, String> {
    Ok(None)
}
pub(super) fn persist(_p: &CallDevicePreferences) -> Result<(), String> {
    Err("Device preferences cannot be saved from this browser connection".into())
}
pub(super) async fn test_microphone() -> Result<Reply, String> {
    Err("Microphone testing is unavailable from this browser connection".into())
}
