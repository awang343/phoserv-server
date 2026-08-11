use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

use crate::AppState;

pub async fn require_token(
    State(state): State<AppState>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let header = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    let provided = header.and_then(|h| h.strip_prefix("Bearer "));

    match provided {
        Some(token) if token == state.config.api_token => Ok(next.run(req).await),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}
