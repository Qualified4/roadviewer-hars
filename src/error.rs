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
        // Python's camera-addition refusals (`register_files`) carry a message on every route.
        let added = match self.1 {
            "storage_unavailable" if self.0 == StatusCode::CONFLICT => {
                Some("저장소 연결 후 다시 시도하세요.")
            }
            "prepared_storage_unavailable" => {
                Some("재생 데이터 NAS에 연결할 수 없습니다. NAS 연결과 저장공간 설정을 확인하세요.")
            }
            "invalid_prepared_path" => Some(
                "NAS 경로에 /share/nas/roadviewer-prepared 형식의 하위 폴더 경로를 입력하세요.",
            ),
            "prepared_migration_running" => Some("재생 데이터를 이동하는 중입니다."),
            "log_unavailable" if self.0 == StatusCode::CONFLICT => Some(
                "원본 로그가 없어 영상을 추가할 수 없습니다. 로그와 영상을 함께 다시 업로드하세요.",
            ),
            _ => None,
        };
        let body = match crate::storage::message(self.1) {
            Some(message) if self.0 == StatusCode::INSUFFICIENT_STORAGE => {
                json!({"error":self.1,"message":message})
            }
            _ => match added {
                Some(message) => {
                    json!({"error":self.1,"message":message})
                }
                _ => json!({"error":self.1}),
            },
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
