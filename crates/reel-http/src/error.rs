use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use reel_core::EngineError;
use serde_json::json;

/// Every failure the HTTP layer can produce, rendered as a small JSON body.
#[derive(Debug)]
pub struct HttpError {
    status: StatusCode,
    message: String,
}

impl HttpError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}

impl From<EngineError> for HttpError {
    fn from(err: EngineError) -> Self {
        let status = StatusCode::from_u16(err.status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        Self::new(status, err.to_string())
    }
}

impl From<anyhow::Error> for HttpError {
    fn from(err: anyhow::Error) -> Self {
        Self::internal(format!("{err:#}"))
    }
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        let body = Json(json!({
            "error": {
                "status": self.status.as_u16(),
                "message": self.message,
            }
        }));
        (self.status, body).into_response()
    }
}
