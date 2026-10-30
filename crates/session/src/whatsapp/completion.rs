use whatsapp_rust::wacore::types::events::ConnectFailureReason;
use whatsapp_rust::{ConflictKind, ProtocolTerminalReason, RunCompletionReason};

use oxidezap_core::UiEvent;

pub(super) fn completion_event(reason: &RunCompletionReason) -> Option<UiEvent> {
    match reason {
        RunCompletionReason::ShutdownRequested => None,
        RunCompletionReason::AutoReconnectDisabled { protocol_error: Some(protocol), .. } => {
            match protocol {
                ProtocolTerminalReason::ConnectFailure(ConnectFailureReason::LoggedOut)
                | ProtocolTerminalReason::StreamErrorCode(401)
                | ProtocolTerminalReason::Conflict(ConflictKind::DeviceRemoved) => {
                    Some(UiEvent::LoggedOut("This device was unlinked from WhatsApp. Reconnecting cannot restore this link. Your local history is still available.".into()))
                }
                _ => Some(UiEvent::Disconnected(format!("WhatsApp ended the connection: {protocol:?}"))),
            }
        }
        _ => Some(UiEvent::Disconnected(format!("The WhatsApp session ended: {reason:?}"))),
    }
}

/// Record the upstream shutdown observations before releasing local owners.
/// These observations do not claim detached workers have released the store.
pub(super) fn report_shutdown(report: &whatsapp_rust::client::ShutdownReport) {
    use whatsapp_rust::client::DrainOutcome;
    if !matches!(report.inbound, DrainOutcome::Completed)
        || !matches!(report.outbound, DrainOutcome::Completed)
        || !matches!(report.signal_settle, DrainOutcome::Completed)
        || report.device.is_err()
        || report.message_secrets.failed_batches != 0
    {
        log::warn!("WhatsApp shutdown has incomplete drains or persistence failures: {report:?}");
    } else {
        log::debug!("WhatsApp shutdown observations: {report:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_logged_out_connect_failure_is_terminal_without_a_connection_error() {
        let reason = RunCompletionReason::AutoReconnectDisabled {
            connection: None,
            connect_error: None,
            protocol_error: Some(ProtocolTerminalReason::ConnectFailure(
                ConnectFailureReason::LoggedOut,
            )),
        };
        assert!(matches!(
            completion_event(&reason),
            Some(UiEvent::LoggedOut(_))
        ));
        assert!(completion_event(&RunCompletionReason::ShutdownRequested).is_none());
        assert!(matches!(
            completion_event(&RunCompletionReason::Stopped),
            Some(UiEvent::Disconnected(_))
        ));
    }
}
