use std::path::Path as StdPath;

use axum::body::Body;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use crate::error::AppError;
use crate::models::{Photo, PhotoRow};
use crate::{media, storage, tags, AppState};

const PHOTO_COLUMNS: &str = "id, hash, original_filename, mime_type, media_type, file_size, width, height, duration_seconds, taken_at, created_at";

fn extension_for(filename: &str, mime_type: &str) -> String {
    if let Some(ext) = StdPath::new(filename).extension().and_then(|e| e.to_str()) {
        if !ext.is_empty() {
            return ext.to_lowercase();
        }
    }
    mime_guess::get_mime_extensions_str(mime_type)
        .and_then(|exts| exts.first())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "bin".to_string())
}

pub async fn upload(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<Photo>), AppError> {
    let mut file_bytes: Option<Vec<u8>> = None;
    let mut filename: Option<String> = None;
    let mut content_type: Option<String> = None;
    let mut tag_paths: Vec<String> = Vec::new();

    while let Some(field) = multipart.next_field().await.map_err(|e| AppError::bad_request(e.to_string()))? {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "file" => {
                filename = field.file_name().map(|s| s.to_string());
                content_type = field.content_type().map(|s| s.to_string());
                file_bytes = Some(field.bytes().await.map_err(|e| AppError::bad_request(e.to_string()))?.to_vec());
            }
            "tags" => {
                let value = field.text().await.map_err(|e| AppError::bad_request(e.to_string()))?;
                if !value.trim().is_empty() {
                    tag_paths.push(value);
                }
            }
            _ => {}
        }
    }

    let bytes = file_bytes.ok_or_else(|| AppError::bad_request("missing file field"))?;
    let filename = filename.unwrap_or_else(|| "upload".to_string());

    let mime_type = content_type
        .filter(|c| !c.is_empty() && c != "application/octet-stream")
        .or_else(|| mime_guess::from_path(&filename).first().map(|m| m.to_string()))
        .ok_or_else(|| AppError::bad_request("could not determine file type"))?;

    let media_type = media::MediaType::from_mime(&mime_type)
        .ok_or_else(|| AppError::bad_request(format!("unsupported media type: {mime_type}")))?;

    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let hash = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();

    let existing: Option<PhotoRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {PHOTO_COLUMNS} FROM photos WHERE hash = ?"
    )))
    .bind(&hash)
    .fetch_optional(&state.pool)
    .await?;

    let photo_row = match existing {
        Some(row) => row,
        None => {
            let ext = extension_for(&filename, &mime_type);
            let stored_path =
                storage::store_original(&state.config.library_path, &hash, &ext, &bytes).await?;

            let mut probe = media::probe(&stored_path).await.unwrap_or_default();
            if media_type == media::MediaType::Image {
                // ffprobe can report a spurious single-frame duration for JPEGs
                probe.duration_seconds = None;
            }

            let sm_path = storage::thumbnail_path(&state.config.library_path, &hash, "sm");
            let md_path = storage::thumbnail_path(&state.config.library_path, &hash, "md");
            // Thumbnailing is best-effort: some sources (e.g. codecs the local
            // ffmpeg build can't decode) will fail here, but that shouldn't
            // block storing the upload itself. Photos without a thumbnail on
            // disk just 404 on `/thumbnail` (handled gracefully by the client).
            if let Err(err) = media::generate_thumbnail(&stored_path, &sm_path, media_type, 320).await {
                tracing::warn!("thumbnail generation failed for {hash} (sm): {err:#}");
            }
            if let Err(err) = media::generate_thumbnail(&stored_path, &md_path, media_type, 1280).await {
                tracing::warn!("thumbnail generation failed for {hash} (md): {err:#}");
            }

            let id = Uuid::new_v4().to_string();
            let file_size = bytes.len() as i64;

            sqlx::query(
                "INSERT INTO photos (id, hash, original_filename, mime_type, media_type, ext, file_size, width, height, duration_seconds, taken_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&id)
            .bind(&hash)
            .bind(&filename)
            .bind(&mime_type)
            .bind(media_type.as_str())
            .bind(&ext)
            .bind(file_size)
            .bind(probe.width)
            .bind(probe.height)
            .bind(probe.duration_seconds)
            .bind(&probe.taken_at)
            .execute(&state.pool)
            .await?;

            sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {PHOTO_COLUMNS} FROM photos WHERE id = ?")))
                .bind(&id)
                .fetch_one(&state.pool)
                .await?
        }
    };

    for path in &tag_paths {
        let tag_id = tags::resolve_or_create(&state.pool, path).await?;
        sqlx::query("INSERT OR IGNORE INTO photo_tags (photo_id, tag_id) VALUES (?, ?)")
            .bind(&photo_row.id)
            .bind(tag_id)
            .execute(&state.pool)
            .await?;
    }

    let tag_list = tags::tags_for_photo(&state.pool, &photo_row.id).await?;
    Ok((StatusCode::CREATED, Json(Photo::from_row(photo_row, tag_list))))
}

#[derive(Deserialize)]
pub struct ListQuery {
    tag: Option<String>,
    limit: Option<i64>,
    cursor: Option<String>,
    #[serde(default)]
    trash: bool,
}

#[derive(Serialize)]
pub struct ListResponse {
    photos: Vec<Photo>,
    total: i64,
    limit: i64,
    next_cursor: Option<String>,
}

/// Cursors are opaque to clients: a `(created_at, id)` pair identifying the
/// last row of the previous page. Ordering by this pair (instead of a numeric
/// OFFSET) keeps pagination stable even as new photos are inserted concurrently.
fn encode_cursor(created_at: &str, id: &str) -> String {
    format!("{created_at}|{id}")
}

fn decode_cursor(cursor: &str) -> Result<(String, String), AppError> {
    let (created_at, id) = cursor.split_once('|').ok_or_else(|| AppError::bad_request("invalid cursor"))?;
    Ok((created_at.to_string(), id.to_string()))
}

pub async fn list(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<ListResponse>, AppError> {
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let fetch_limit = limit + 1;
    let cursor = q.cursor.as_deref().map(decode_cursor).transpose()?;
    let cursor_clause = "created_at < ? OR (created_at = ? AND id < ?)";

    // Trashed photos (tagged with the reserved `trash` tag) are excluded from
    // every normal view; `?trash=true` flips that to show only trash
    // contents, ignoring any `?tag` filter.
    let trash_tag_id = tags::trash_tag_id(&state.pool).await?;
    let exclude_tag_id: Option<i64> = if q.trash { None } else { trash_tag_id };

    let tag_ids: Option<Vec<i64>> = if q.trash {
        Some(trash_tag_id.into_iter().collect())
    } else {
        match &q.tag {
            Some(path) => match tags::find_by_path(&state.pool, path).await? {
                Some(id) => Some(tags::descendant_ids(&state.pool, id).await?),
                None => Some(vec![]),
            },
            None => None,
        }
    };

    let (mut rows, total): (Vec<PhotoRow>, i64) = match &tag_ids {
        Some(ids) if ids.is_empty() => (vec![], 0),
        Some(ids) => {
            let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let exclude_sql = if exclude_tag_id.is_some() {
                "AND NOT EXISTS (SELECT 1 FROM photo_tags ptx WHERE ptx.photo_id = p.id AND ptx.tag_id = ?)"
            } else {
                ""
            };

            let count_query = format!(
                "SELECT COUNT(DISTINCT p.id) FROM photos p JOIN photo_tags pt ON pt.photo_id = p.id WHERE pt.tag_id IN ({placeholders}) {exclude_sql}"
            );
            let mut cq = sqlx::query_as::<_, (i64,)>(sqlx::AssertSqlSafe(count_query));
            for id in ids {
                cq = cq.bind(id);
            }
            if let Some(ex) = exclude_tag_id {
                cq = cq.bind(ex);
            }
            let (total,) = cq.fetch_one(&state.pool).await?;

            let cursor_sql = if cursor.is_some() { format!("AND ({cursor_clause})") } else { String::new() };
            let list_query = format!(
                "SELECT DISTINCT p.{} FROM photos p JOIN photo_tags pt ON pt.photo_id = p.id WHERE pt.tag_id IN ({placeholders}) {exclude_sql} {cursor_sql} ORDER BY p.created_at DESC, p.id DESC LIMIT ?",
                PHOTO_COLUMNS.replace(", ", ", p.")
            );
            let mut lq = sqlx::query_as::<_, PhotoRow>(sqlx::AssertSqlSafe(list_query));
            for id in ids {
                lq = lq.bind(id);
            }
            if let Some(ex) = exclude_tag_id {
                lq = lq.bind(ex);
            }
            if let Some((created_at, id)) = &cursor {
                lq = lq.bind(created_at).bind(created_at).bind(id);
            }
            let rows = lq.bind(fetch_limit).fetch_all(&state.pool).await?;
            (rows, total)
        }
        None => {
            let exclude_sql = if exclude_tag_id.is_some() {
                "NOT EXISTS (SELECT 1 FROM photo_tags ptx WHERE ptx.photo_id = photos.id AND ptx.tag_id = ?)"
            } else {
                ""
            };
            let mut conditions: Vec<&str> = Vec::new();
            if exclude_tag_id.is_some() {
                conditions.push(exclude_sql);
            }
            if cursor.is_some() {
                conditions.push(cursor_clause);
            }
            let where_sql = if conditions.is_empty() {
                String::new()
            } else {
                format!("WHERE {}", conditions.join(" AND "))
            };

            let count_query = format!("SELECT COUNT(*) FROM photos {}", if exclude_tag_id.is_some() {
                format!("WHERE {exclude_sql}")
            } else {
                String::new()
            });
            let mut cq = sqlx::query_as::<_, (i64,)>(sqlx::AssertSqlSafe(count_query));
            if let Some(ex) = exclude_tag_id {
                cq = cq.bind(ex);
            }
            let (total,) = cq.fetch_one(&state.pool).await?;

            let mut lq = sqlx::query_as::<_, PhotoRow>(sqlx::AssertSqlSafe(format!(
                "SELECT {PHOTO_COLUMNS} FROM photos {where_sql} ORDER BY created_at DESC, id DESC LIMIT ?"
            )));
            if let Some(ex) = exclude_tag_id {
                lq = lq.bind(ex);
            }
            if let Some((created_at, id)) = &cursor {
                lq = lq.bind(created_at).bind(created_at).bind(id);
            }
            let rows = lq.bind(fetch_limit).fetch_all(&state.pool).await?;
            (rows, total)
        }
    };

    let has_more = rows.len() as i64 > limit;
    rows.truncate(limit as usize);
    let next_cursor = has_more.then(|| rows.last().map(|r| encode_cursor(&r.created_at, &r.id))).flatten();

    let ids: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();
    let mut tag_map = tags::tags_for_photos(&state.pool, &ids).await?;

    let photos = rows
        .into_iter()
        .map(|row| {
            let tags = tag_map.remove(&row.id).unwrap_or_default();
            Photo::from_row(row, tags)
        })
        .collect();

    Ok(Json(ListResponse { photos, total, limit, next_cursor }))
}

/// Looks up a photo by its content hash (SHA-256 of the original file
/// bytes), the same hash `upload` computes for dedup. Lets clients check
/// whether a file already exists before spending bandwidth uploading it.
pub async fn get_by_hash(State(state): State<AppState>, Path(hash): Path<String>) -> Result<Json<Photo>, AppError> {
    let row: PhotoRow = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {PHOTO_COLUMNS} FROM photos WHERE hash = ?"
    )))
    .bind(&hash)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| AppError::not_found("photo not found"))?;
    let tag_list = tags::tags_for_photo(&state.pool, &row.id).await?;
    Ok(Json(Photo::from_row(row, tag_list)))
}

async fn fetch_photo_row(state: &AppState, id: &str) -> Result<PhotoRow, AppError> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {PHOTO_COLUMNS} FROM photos WHERE id = ?")))
        .bind(id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| AppError::not_found("photo not found"))
}

pub async fn get_one(State(state): State<AppState>, Path(id): Path<String>) -> Result<Json<Photo>, AppError> {
    let row = fetch_photo_row(&state, &id).await?;
    let tag_list = tags::tags_for_photo(&state.pool, &id).await?;
    Ok(Json(Photo::from_row(row, tag_list)))
}

/// Permanently deletes a photo: removes its DB row (cascading photo_tags)
/// and best-effort removes the original + thumbnails from disk. Only allowed
/// once a photo has already been moved to trash, so this can't be triggered
/// as a direct, un-recoverable action from the main library view.
pub async fn delete_permanently(State(state): State<AppState>, Path(id): Path<String>) -> Result<StatusCode, AppError> {
    let row: (String, String) = sqlx::query_as("SELECT hash, ext FROM photos WHERE id = ?")
        .bind(&id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| AppError::not_found("photo not found"))?;
    let (hash, ext) = row;

    let tag_list = tags::tags_for_photo(&state.pool, &id).await?;
    if !tag_list.iter().any(|t| t == tags::TRASH_TAG) {
        return Err(AppError::bad_request("photo must be moved to trash before it can be permanently deleted"));
    }

    sqlx::query("DELETE FROM photos WHERE id = ?")
        .bind(&id)
        .execute(&state.pool)
        .await?;

    if let Err(err) = storage::delete_original(&state.config.library_path, &hash, &ext).await {
        tracing::warn!("failed to delete original file for {hash}: {err:#}");
    }
    if let Err(err) = storage::delete_thumbnails(&state.config.library_path, &hash).await {
        tracing::warn!("failed to delete thumbnails for {hash}: {err:#}");
    }

    Ok(StatusCode::NO_CONTENT)
}

async fn serve_file(path: &StdPath, mime: &str) -> Result<Response, AppError> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|_| AppError::not_found("file not found on disk"))?;
    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);
    Ok((
        [
            (header::CONTENT_TYPE, mime.to_string()),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable".to_string()),
        ],
        body,
    )
        .into_response())
}

pub async fn get_file(State(state): State<AppState>, Path(id): Path<String>) -> Result<Response, AppError> {
    let row: (String, String, String) = sqlx::query_as("SELECT hash, ext, mime_type FROM photos WHERE id = ?")
        .bind(&id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| AppError::not_found("photo not found"))?;
    let (hash, ext, mime_type) = row;
    let path = storage::original_path(&state.config.library_path, &hash, &ext);
    serve_file(&path, &mime_type).await
}

#[derive(Deserialize)]
pub struct ThumbQuery {
    size: Option<String>,
}

pub async fn get_thumbnail(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<ThumbQuery>,
) -> Result<Response, AppError> {
    let size = match q.size.as_deref() {
        Some("md") => "md",
        _ => "sm",
    };
    let row: (String,) = sqlx::query_as("SELECT hash FROM photos WHERE id = ?")
        .bind(&id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| AppError::not_found("photo not found"))?;
    let path = storage::thumbnail_path(&state.config.library_path, &row.0, size);
    serve_file(&path, "image/jpeg").await
}

/// Re-runs thumbnail generation for a photo against its stored original.
/// Unlike upload's best-effort thumbnailing, failures here are surfaced to
/// the caller since this is an explicit, user-initiated retry.
pub async fn regenerate_thumbnail(State(state): State<AppState>, Path(id): Path<String>) -> Result<Json<Photo>, AppError> {
    let row: (String, String, String) = sqlx::query_as("SELECT hash, ext, media_type FROM photos WHERE id = ?")
        .bind(&id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| AppError::not_found("photo not found"))?;
    let (hash, ext, media_type_str) = row;
    let media_type = if media_type_str == "video" {
        media::MediaType::Video
    } else {
        media::MediaType::Image
    };

    let stored_path = storage::original_path(&state.config.library_path, &hash, &ext);
    let sm_path = storage::thumbnail_path(&state.config.library_path, &hash, "sm");
    let md_path = storage::thumbnail_path(&state.config.library_path, &hash, "md");

    media::generate_thumbnail(&stored_path, &sm_path, media_type, 320).await?;
    media::generate_thumbnail(&stored_path, &md_path, media_type, 1280).await?;

    let photo_row = fetch_photo_row(&state, &id).await?;
    let tag_list = tags::tags_for_photo(&state.pool, &id).await?;
    Ok(Json(Photo::from_row(photo_row, tag_list)))
}

#[derive(Deserialize)]
pub struct TagsBody {
    tags: Vec<String>,
}

pub async fn add_tags(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<TagsBody>,
) -> Result<Json<Photo>, AppError> {
    let row = fetch_photo_row(&state, &id).await?;
    for path in &body.tags {
        let tag_id = tags::resolve_or_create(&state.pool, path).await?;
        sqlx::query("INSERT OR IGNORE INTO photo_tags (photo_id, tag_id) VALUES (?, ?)")
            .bind(&id)
            .bind(tag_id)
            .execute(&state.pool)
            .await?;
    }
    let tag_list = tags::tags_for_photo(&state.pool, &id).await?;
    Ok(Json(Photo::from_row(row, tag_list)))
}

pub async fn remove_tags(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<TagsBody>,
) -> Result<Json<Photo>, AppError> {
    let row = fetch_photo_row(&state, &id).await?;
    for path in &body.tags {
        if let Some(tag_id) = tags::find_by_path(&state.pool, path).await? {
            sqlx::query("DELETE FROM photo_tags WHERE photo_id = ? AND tag_id = ?")
                .bind(&id)
                .bind(tag_id)
                .execute(&state.pool)
                .await?;
        }
    }
    let tag_list = tags::tags_for_photo(&state.pool, &id).await?;
    Ok(Json(Photo::from_row(row, tag_list)))
}
