use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;

use crate::error::AppError;
use crate::gallery::{self, ReorderError};
use crate::gallery_tags;
use crate::models::{Gallery, GalleryDetail, TagNode};
use crate::tags::{DeleteError, RenameError};
use crate::AppState;

#[derive(Deserialize)]
pub struct CreateGalleryBody {
    title: String,
    description: Option<String>,
}

pub async fn create(
    State(state): State<AppState>,
    Json(body): Json<CreateGalleryBody>,
) -> Result<(StatusCode, Json<Gallery>), AppError> {
    let title = body.title.trim();
    if title.is_empty() {
        return Err(AppError::bad_request("title must not be empty"));
    }
    let id = gallery::create(&state.pool, title, body.description.as_deref()).await?;
    let g = gallery::get_summary(&state.pool, &id)
        .await?
        .ok_or_else(|| AppError::not_found("gallery not found"))?;
    Ok((StatusCode::CREATED, Json(g)))
}

#[derive(Deserialize)]
pub struct ListQuery {
    tag: Option<String>,
}

pub async fn list(State(state): State<AppState>, Query(q): Query<ListQuery>) -> Result<Json<Vec<Gallery>>, AppError> {
    let tag_ids: Option<Vec<i64>> = match &q.tag {
        Some(path) => match gallery_tags::find_by_path(&state.pool, path).await? {
            Some(id) => Some(gallery_tags::descendant_ids(&state.pool, id).await?),
            None => Some(vec![]),
        },
        None => None,
    };
    Ok(Json(gallery::list_summaries(&state.pool, tag_ids).await?))
}

pub async fn get_one(State(state): State<AppState>, Path(id): Path<String>) -> Result<Json<GalleryDetail>, AppError> {
    let detail = gallery::get_detail(&state.pool, &id)
        .await?
        .ok_or_else(|| AppError::not_found("gallery not found"))?;
    Ok(Json(detail))
}

#[derive(Deserialize)]
pub struct UpdateGalleryBody {
    title: Option<String>,
    description: Option<String>,
}

pub async fn update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateGalleryBody>,
) -> Result<Json<Gallery>, AppError> {
    if let Some(t) = &body.title {
        if t.trim().is_empty() {
            return Err(AppError::bad_request("title must not be empty"));
        }
    }
    gallery::update(&state.pool, &id, body.title.as_deref(), body.description.as_deref()).await?;
    let g = gallery::get_summary(&state.pool, &id)
        .await?
        .ok_or_else(|| AppError::not_found("gallery not found"))?;
    Ok(Json(g))
}

pub async fn delete(State(state): State<AppState>, Path(id): Path<String>) -> Result<StatusCode, AppError> {
    if !gallery::delete(&state.pool, &id).await? {
        return Err(AppError::not_found("gallery not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct PhotoIdsBody {
    photo_ids: Vec<String>,
}

pub async fn add_photos(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PhotoIdsBody>,
) -> Result<Json<GalleryDetail>, AppError> {
    gallery::add_photos(&state.pool, &id, &body.photo_ids).await?;
    let detail = gallery::get_detail(&state.pool, &id)
        .await?
        .ok_or_else(|| AppError::not_found("gallery not found"))?;
    Ok(Json(detail))
}

pub async fn remove_photos(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PhotoIdsBody>,
) -> Result<Json<GalleryDetail>, AppError> {
    gallery::remove_photos(&state.pool, &id, &body.photo_ids).await?;
    let detail = gallery::get_detail(&state.pool, &id)
        .await?
        .ok_or_else(|| AppError::not_found("gallery not found"))?;
    Ok(Json(detail))
}

#[derive(Deserialize)]
pub struct ReorderBody {
    photo_ids: Vec<String>,
}

pub async fn reorder(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ReorderBody>,
) -> Result<Json<GalleryDetail>, AppError> {
    match gallery::reorder(&state.pool, &id, &body.photo_ids).await {
        Ok(()) => {}
        Err(ReorderError::Mismatch) => {
            return Err(AppError::bad_request("photo_ids must exactly match the gallery's current photos"));
        }
        Err(ReorderError::Other(err)) => return Err(err.into()),
    }
    let detail = gallery::get_detail(&state.pool, &id)
        .await?
        .ok_or_else(|| AppError::not_found("gallery not found"))?;
    Ok(Json(detail))
}

#[derive(Deserialize)]
pub struct GalleryTagsBody {
    tags: Vec<String>,
}

pub async fn add_tags(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<GalleryTagsBody>,
) -> Result<Json<Gallery>, AppError> {
    for path in &body.tags {
        let tag_id = gallery_tags::resolve_or_create(&state.pool, path).await?;
        sqlx::query("INSERT OR IGNORE INTO gallery_tag_links (gallery_id, tag_id) VALUES (?, ?)")
            .bind(&id)
            .bind(tag_id)
            .execute(&state.pool)
            .await?;
    }
    let g = gallery::get_summary(&state.pool, &id)
        .await?
        .ok_or_else(|| AppError::not_found("gallery not found"))?;
    Ok(Json(g))
}

pub async fn remove_tags(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<GalleryTagsBody>,
) -> Result<Json<Gallery>, AppError> {
    for path in &body.tags {
        if let Some(tag_id) = gallery_tags::find_by_path(&state.pool, path).await? {
            sqlx::query("DELETE FROM gallery_tag_links WHERE gallery_id = ? AND tag_id = ?")
                .bind(&id)
                .bind(tag_id)
                .execute(&state.pool)
                .await?;
        }
    }
    let g = gallery::get_summary(&state.pool, &id)
        .await?
        .ok_or_else(|| AppError::not_found("gallery not found"))?;
    Ok(Json(g))
}

pub async fn tag_tree(State(state): State<AppState>) -> Result<Json<Vec<TagNode>>, AppError> {
    Ok(Json(gallery_tags::build_tree(&state.pool).await?))
}

#[derive(Deserialize)]
pub struct RenameGalleryTagBody {
    name: String,
}

pub async fn rename_tag(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<RenameGalleryTagBody>,
) -> Result<Json<Vec<TagNode>>, AppError> {
    match gallery_tags::rename(&state.pool, id, &body.name).await {
        Ok(()) => Ok(Json(gallery_tags::build_tree(&state.pool).await?)),
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

pub async fn delete_tag(State(state): State<AppState>, Path(id): Path<i64>) -> Result<Json<Vec<TagNode>>, AppError> {
    match gallery_tags::delete(&state.pool, id).await {
        Ok(()) => Ok(Json(gallery_tags::build_tree(&state.pool).await?)),
        Err(DeleteError::NotFound) => Err(AppError::not_found("tag not found")),
        Err(DeleteError::Protected) => Err(AppError::bad_request("this tag cannot be deleted")),
        Err(DeleteError::Other(err)) => Err(err.into()),
    }
}
