use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;

use crate::error::AppError;
use crate::models::TagNode;
use crate::tags::RenameError;
use crate::{tags, AppState};

pub async fn tree(State(state): State<AppState>) -> Result<Json<Vec<TagNode>>, AppError> {
    Ok(Json(tags::build_tree(&state.pool).await?))
}

#[derive(Deserialize)]
pub struct RenameTagRequest {
    pub name: String,
}

pub async fn rename(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<RenameTagRequest>,
) -> Result<Json<Vec<TagNode>>, AppError> {
    match tags::rename(&state.pool, id, &body.name).await {
        Ok(()) => Ok(Json(tags::build_tree(&state.pool).await?)),
        Err(RenameError::InvalidName) => {
            Err(AppError::bad_request("tag name must not be empty or contain '/'"))
        }
        Err(RenameError::NotFound) => Err(AppError::not_found("tag not found")),
        Err(RenameError::NameConflict) => {
            Err(AppError::bad_request("a tag with that name already exists under the same parent"))
        }
        Err(RenameError::Other(err)) => Err(err.into()),
    }
}
