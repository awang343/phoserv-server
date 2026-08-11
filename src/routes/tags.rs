use axum::extract::State;
use axum::Json;

use crate::error::AppError;
use crate::models::TagNode;
use crate::{tags, AppState};

pub async fn tree(State(state): State<AppState>) -> Result<Json<Vec<TagNode>>, AppError> {
    Ok(Json(tags::build_tree(&state.pool).await?))
}
