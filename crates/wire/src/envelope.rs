//! Protocol framing and request/response envelope definitions.

use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::request::ClientRequest;
use crate::response::DaemonResponse;

/// Current wire protocol version.
pub const CURRENT_PROTOCOL_VERSION: u32 = 1;

/// Request correlation identifier.
pub type RequestId = u64;

/// Protocol version number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProtocolVersion(pub u32);

impl Default for ProtocolVersion {
    fn default() -> Self {
        Self(CURRENT_PROTOCOL_VERSION)
    }
}

/// Opaque pagination token.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PageCursor(pub String);

impl PageCursor {
    pub fn new(cursor: impl Into<String>) -> Self {
        Self(cursor.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One outgoing client request wrapped with its tracking ID.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestEnvelope {
    pub id: RequestId,
    #[serde(flatten)]
    pub request: ClientRequest,
}

/// One daemon response correlating to a request ID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseEnvelope {
    pub id: Option<RequestId>,
    #[serde(flatten)]
    pub result: ResponseResult,
}

impl ResponseEnvelope {
    pub fn success(id: RequestId, response: DaemonResponse) -> Self {
        Self {
            id: Some(id),
            result: ResponseResult::Ok {
                payload: Box::new(response),
            },
        }
    }

    pub fn error(id: Option<RequestId>, error: ApiError) -> Self {
        Self {
            id,
            result: ResponseResult::Error { error },
        }
    }
}

/// Successful payload or structured failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ResponseResult {
    Ok {
        #[serde(flatten)]
        payload: Box<DaemonResponse>,
    },
    Error {
        #[serde(flatten)]
        error: ApiError,
    },
}
