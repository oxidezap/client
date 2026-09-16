//! Lightweight, decoupled wire protocol DTOs for OxideZap IPC and CLI.
//!
//! Deliberately free from heavy dependencies like `wacore`, `whatsapp-rust`,
//! SQLite, GPUI, or media codecs, allowing thin client binaries (`oxidezap-cli`)
//! to stay minimal in size (< 2 MiB) and fast to compile.

pub mod dto;
pub mod envelope;
pub mod error;
pub mod event;
pub mod request;
pub mod response;

pub use dto::*;
pub use envelope::*;
pub use error::ApiError;
pub use event::DaemonEvent;
pub use request::ClientRequest;
pub use response::DaemonResponse;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_serialization_round_trip() {
        let req = ClientRequest::SendText {
            to: "5511999999999@s.whatsapp.net".into(),
            message: "Hello from CLI".into(),
            reply_to: None,
            mentions: vec![],
            enqueue_only: false,
        };
        let envelope = RequestEnvelope {
            id: 42,
            request: req,
        };

        let json = serde_json::to_string(&envelope).expect("serialize request envelope");
        let deserialized: RequestEnvelope =
            serde_json::from_str(&json).expect("deserialize request envelope");

        assert_eq!(envelope, deserialized);
    }

    #[test]
    fn response_ok_and_error_round_trip() {
        let ok_resp = ResponseEnvelope {
            id: Some(1),
            result: ResponseResult::Ok {
                payload: Box::new(DaemonResponse::Ack),
            },
        };
        let json_ok = serde_json::to_string(&ok_resp).expect("serialize ok response");
        let deserialized_ok: ResponseEnvelope =
            serde_json::from_str(&json_ok).expect("deserialize ok response");
        assert_eq!(ok_resp, deserialized_ok);

        let err_resp = ResponseEnvelope {
            id: Some(2),
            result: ResponseResult::Error {
                error: ApiError::read_only("Cannot mutate in read-only mode"),
            },
        };
        let json_err = serde_json::to_string(&err_resp).expect("serialize err response");
        let deserialized_err: ResponseEnvelope =
            serde_json::from_str(&json_err).expect("deserialize err response");
        assert_eq!(err_resp, deserialized_err);
    }
}
