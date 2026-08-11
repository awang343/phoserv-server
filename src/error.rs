use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

pub struct AppError(pub StatusCode, pub anyhow::Error);

impl AppError {
    pub fn not_found(msg: impl Into<String>) -> Self {
        AppError(StatusCode::NOT_FOUND, anyhow::anyhow!(msg.into()))
    }

    pub fn bad_request(msg: impl Into<String>) -> Self {
        AppError(StatusCode::BAD_REQUEST, anyhow::anyhow!(msg.into()))
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        tracing::error!("request error: {:?}", self.1);
        let body = Json(json!({ "error": self.1.to_string() }));
        (self.0, body).into_response()
    }
}

impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        AppError(StatusCode::INTERNAL_SERVER_ERROR, err.into())
    }
}
