//! Bridge error type; maps to an HTTP status + a client-format error body.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("unauthorized: {0}")]
    Unauthorized(String),
    #[error("unknown model: {0}")]
    UnknownModel(String),
    #[error("upstream error ({status}): {message}")]
    Upstream { status: u16, message: String },
    #[error("upstream unreachable: {0}")]
    UpstreamUnreachable(String),
    #[error("not implemented: {0}")]
    NotImplemented(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl BridgeError {
    pub fn http_status(&self) -> StatusCode {
        match self {
            BridgeError::BadRequest(_) | BridgeError::UnknownModel(_) => StatusCode::BAD_REQUEST,
            BridgeError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            BridgeError::Upstream { status, .. } => {
                StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY)
            }
            BridgeError::UpstreamUnreachable(_) => StatusCode::BAD_GATEWAY,
            BridgeError::NotImplemented(_) => StatusCode::NOT_IMPLEMENTED,
            BridgeError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for BridgeError {
    fn into_response(self) -> Response {
        let status = self.http_status();
        let body = json!({ "error": { "message": self.to_string(), "type": "bridge_error" } });
        (status, Json(body)).into_response()
    }
}
