use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

#[derive(Debug, Clone)]
pub struct AppError {
    pub status: u16,
    pub kind: String,
    pub message: String,
}

impl AppError {
    pub fn new(status: u16, message: impl Into<String>, kind: impl Into<String>) -> Self {
        AppError {
            status,
            kind: kind.into(),
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(400, message, "invalid_request_error")
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(401, message, "authentication_error")
    }

    pub fn config(message: impl Into<String>) -> Self {
        Self::new(500, message, "configuration_error")
    }

    pub fn upstream(status: u16, message: impl Into<String>) -> Self {
        Self::new(status, message, "upstream_error")
    }

    fn status_code(&self) -> StatusCode {
        StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
    }

    /// Maps the error onto an Anthropic Messages API error type so clients like
    /// Claude Code can apply the right handling (retry on overloaded, stop on
    /// authentication, etc.). Falls back to the internal kind when it is already
    /// a valid Anthropic type.
    pub fn anthropic_error_type(&self) -> &str {
        match self.status {
            400 => "invalid_request_error",
            401 => "authentication_error",
            403 => "permission_error",
            404 => "not_found_error",
            413 => "request_too_large",
            429 => "rate_limit_error",
            503 | 529 => "overloaded_error",
            s if s >= 500 => "api_error",
            _ => &self.kind,
        }
    }

    /// OpenAI-style error body (used by the Codex/Responses passthrough path).
    pub fn into_openai_response(self) -> Response {
        let body = json!({
            "error": {
                "message": self.message,
                "type": self.kind,
            }
        });
        (self.status_code(), Json(body)).into_response()
    }

    /// Anthropic-style error body (used by the /v1/messages path).
    pub fn into_anthropic_response(self) -> Response {
        let body = json!({
            "type": "error",
            "error": {
                "type": self.anthropic_error_type(),
                "message": self.message,
            }
        });
        (self.status_code(), Json(body)).into_response()
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}): {}", self.status, self.kind, self.message)
    }
}

impl std::error::Error for AppError {}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        self.into_openai_response()
    }
}

impl From<reqwest::Error> for AppError {
    fn from(e: reqwest::Error) -> Self {
        if e.is_timeout() {
            return AppError::new(504, "Upstream request timed out", "upstream_timeout");
        }
        if e.is_connect() {
            return AppError::new(
                502,
                format!("Unable to reach upstream: {e}"),
                "upstream_connection_error",
            );
        }
        AppError::new(
            502,
            format!("Upstream request failed: {e}"),
            "upstream_error",
        )
    }
}
