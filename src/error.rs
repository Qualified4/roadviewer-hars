use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

#[derive(Debug)]
pub struct ApiError(pub StatusCode, pub &'static str);
pub type Result<T> = std::result::Result<T, ApiError>;
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = match crate::storage::message(self.1) {
            Some(message) if self.0 == StatusCode::INSUFFICIENT_STORAGE => {
                json!({"error":self.1,"message":message})
            }
            _ => json!({"error":self.1}),
        };
        (self.0, Json(body)).into_response()
    }
}
impl From<std::io::Error> for ApiError {
    fn from(e: std::io::Error) -> Self {
        // Never log arbitrary OS messages containing source paths or credentials.
        // Errors surfaced through tokio's file buffer keep only their kind, not the errno.
        if e.raw_os_error() == Some(28) || e.kind() == std::io::ErrorKind::StorageFull {
            return Self(StatusCode::INSUFFICIENT_STORAGE, "insufficient_disk_space");
        }
        Self(StatusCode::INTERNAL_SERVER_ERROR, "storage_error")
    }
}
pub fn bad(code: &'static str) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, code)
}
pub fn conflict(code: &'static str) -> ApiError {
    ApiError(StatusCode::CONFLICT, code)
}
pub fn unauthorized(code: &'static str) -> ApiError {
    ApiError(StatusCode::UNAUTHORIZED, code)
}
pub fn missing(code: &'static str) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, code)
}
