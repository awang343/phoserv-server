use std::collections::{BTreeSet, HashMap};
use std::path::{Path as StdPath, PathBuf};

use axum::body::Body;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio_util::io::ReaderStream;
use walkdir::WalkDir;

use crate::error::AppError;
use crate::ingest::{ingest_bytes, status_for, FileResult, ImportSummary};
use crate::models::{Photo, PhotoRow, PHOTO_COLUMNS};
use crate::{media, search, storage, tags, AppState};

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

    let outcome = ingest_bytes(&state, filename, content_type, bytes, &tag_paths).await?;
    Ok((StatusCode::CREATED, Json(outcome.photo)))
}

#[derive(Deserialize)]
pub struct ListQuery {
    /// Boolean search query over tags (AND/OR/NOT, `-tag` shorthand,
    /// parentheses for grouping — see `search::parse`). Missing or empty
    /// matches every photo. Parsed and evaluated entirely server-side.
    q: Option<String>,
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

    // `?trash=true` restricts the view to only trashed photos (tagged with
    // the reserved `trash` tag), ignoring any `?q` filter — used by the
    // dedicated Trash tab for review/permanent-delete. Every other view,
    // including plain `?q` searches, shows trashed photos inline alongside
    // everything else; the frontend marks them with a trash icon.
    let (filter_sql, filter_binds): (String, Vec<i64>) = if q.trash {
        match tags::trash_tag_id(&state.pool).await? {
            Some(id) => (
                "EXISTS (SELECT 1 FROM photo_tags pt_trash WHERE pt_trash.photo_id = p.id AND pt_trash.tag_id = ?)"
                    .to_string(),
                vec![id],
            ),
            None => ("0=1".to_string(), vec![]),
        }
    } else {
        let expr = search::parse(q.q.as_deref().unwrap_or("")).map_err(AppError::bad_request)?;
        let ids_by_term = search::resolve_terms(&state.pool, &expr).await?;
        search::build_sql(&expr, &ids_by_term)
    };

    let base_conditions = vec![filter_sql];
    let base_binds = filter_binds;
    let base_where = format!("WHERE {}", base_conditions.join(" AND "));

    let count_query = format!("SELECT COUNT(*) FROM photos p {base_where}");
    let mut cq = sqlx::query_as::<_, (i64,)>(sqlx::AssertSqlSafe(count_query));
    for id in &base_binds {
        cq = cq.bind(id);
    }
    let (total,) = cq.fetch_one(&state.pool).await?;

    let mut list_conditions = base_conditions.clone();
    if cursor.is_some() {
        list_conditions.push(cursor_clause.to_string());
    }
    let list_where = format!("WHERE {}", list_conditions.join(" AND "));
    let list_query = format!(
        "SELECT p.{} FROM photos p {list_where} ORDER BY p.created_at DESC, p.id DESC LIMIT ?",
        PHOTO_COLUMNS.replace(", ", ", p.")
    );
    let mut lq = sqlx::query_as::<_, PhotoRow>(sqlx::AssertSqlSafe(list_query));
    for id in &base_binds {
        lq = lq.bind(id);
    }
    if let Some((created_at, id)) = &cursor {
        lq = lq.bind(created_at).bind(created_at).bind(id);
    }
    let mut rows = lq.bind(fetch_limit).fetch_all(&state.pool).await?;

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

async fn fetch_photos(state: &AppState, ids: &[String]) -> Result<Vec<Photo>, AppError> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let query = format!("SELECT {PHOTO_COLUMNS} FROM photos WHERE id IN ({placeholders})");
    let mut q = sqlx::query_as::<_, PhotoRow>(sqlx::AssertSqlSafe(query));
    for id in ids {
        q = q.bind(id);
    }
    let rows = q.fetch_all(&state.pool).await?;
    let mut tag_map = tags::tags_for_photos(&state.pool, ids).await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let tag_list = tag_map.remove(&row.id).unwrap_or_default();
            Photo::from_row(row, tag_list)
        })
        .collect())
}

#[derive(Deserialize)]
pub struct BulkTagsBody {
    photo_ids: Vec<String>,
    tags: Vec<String>,
}

pub async fn bulk_add_tags(
    State(state): State<AppState>,
    Json(body): Json<BulkTagsBody>,
) -> Result<Json<Vec<Photo>>, AppError> {
    let mut tag_ids = Vec::with_capacity(body.tags.len());
    for path in &body.tags {
        tag_ids.push(tags::resolve_or_create(&state.pool, path).await?);
    }
    for id in &body.photo_ids {
        for tag_id in &tag_ids {
            sqlx::query("INSERT OR IGNORE INTO photo_tags (photo_id, tag_id) VALUES (?, ?)")
                .bind(id)
                .bind(tag_id)
                .execute(&state.pool)
                .await?;
        }
    }
    Ok(Json(fetch_photos(&state, &body.photo_ids).await?))
}

pub async fn bulk_remove_tags(
    State(state): State<AppState>,
    Json(body): Json<BulkTagsBody>,
) -> Result<Json<Vec<Photo>>, AppError> {
    let mut tag_ids = Vec::with_capacity(body.tags.len());
    for path in &body.tags {
        if let Some(tag_id) = tags::find_by_path(&state.pool, path).await? {
            tag_ids.push(tag_id);
        }
    }
    for id in &body.photo_ids {
        for tag_id in &tag_ids {
            sqlx::query("DELETE FROM photo_tags WHERE photo_id = ? AND tag_id = ?")
                .bind(id)
                .bind(tag_id)
                .execute(&state.pool)
                .await?;
        }
    }
    Ok(Json(fetch_photos(&state, &body.photo_ids).await?))
}

#[derive(Deserialize)]
pub struct BulkDeleteBody {
    photo_ids: Vec<String>,
}

/// Permanently deletes multiple photos in one request. Like the single-photo
/// version, every id must already carry the trash tag; if any doesn't, the
/// whole request is rejected before anything is deleted.
pub async fn bulk_delete_permanently(
    State(state): State<AppState>,
    Json(body): Json<BulkDeleteBody>,
) -> Result<StatusCode, AppError> {
    let mut to_delete: Vec<(String, String)> = Vec::with_capacity(body.photo_ids.len());
    for id in &body.photo_ids {
        let row: (String, String) = sqlx::query_as("SELECT hash, ext FROM photos WHERE id = ?")
            .bind(id)
            .fetch_optional(&state.pool)
            .await?
            .ok_or_else(|| AppError::not_found(format!("photo not found: {id}")))?;
        let tag_list = tags::tags_for_photo(&state.pool, id).await?;
        if !tag_list.iter().any(|t| t == tags::TRASH_TAG) {
            return Err(AppError::bad_request(
                "all photos must be moved to trash before they can be permanently deleted",
            ));
        }
        to_delete.push(row);
    }

    for id in &body.photo_ids {
        sqlx::query("DELETE FROM photos WHERE id = ?")
            .bind(id)
            .execute(&state.pool)
            .await?;
    }

    for (hash, ext) in &to_delete {
        if let Err(err) = storage::delete_original(&state.config.library_path, hash, ext).await {
            tracing::warn!("failed to delete original file for {hash}: {err:#}");
        }
        if let Err(err) = storage::delete_thumbnails(&state.config.library_path, hash).await {
            tracing::warn!("failed to delete thumbnails for {hash}: {err:#}");
        }
    }

    Ok(StatusCode::NO_CONTENT)
}

/// A `(regex, template)` pair for deriving a tag from a file's complete path.
/// `template` is rendered with `{name}` placeholders filled in from the
/// regex's named/numbered capture groups plus the built-in `{filename}`,
/// `{stem}` and `{parent}` fields; a placeholder with no matching field
/// fails the whole rule (for that file) rather than silently dropping it.
struct TagRule {
    regex: Regex,
    template: String,
}

impl TagRule {
    fn new(pattern: &str, template: &str) -> Result<Self, regex::Error> {
        Ok(TagRule { regex: Regex::new(pattern)?, template: template.to_string() })
    }

    fn apply(&self, path: &str, filename: &str, stem: &str, parent: &str) -> Option<String> {
        let caps = self.regex.captures(path)?;

        let mut fields: HashMap<String, String> = HashMap::new();
        fields.insert("filename".to_string(), filename.to_string());
        fields.insert("stem".to_string(), stem.to_string());
        fields.insert("parent".to_string(), parent.to_string());
        for (i, group) in caps.iter().enumerate().skip(1) {
            if let Some(m) = group {
                fields.insert(i.to_string(), m.as_str().to_string());
            }
        }
        for name in self.regex.capture_names().flatten() {
            if let Some(m) = caps.name(name) {
                fields.insert(name.to_string(), m.as_str().to_string());
            }
        }

        match render_template(&self.template, &fields) {
            Ok(tag) => Some(tag),
            Err(missing) => {
                tracing::warn!(
                    "tag-rule template {:?} references missing field {missing:?} for {path}",
                    self.template
                );
                None
            }
        }
    }
}

/// Renders `{field}`-style placeholders in `template` from `fields`. Returns
/// the name of the first placeholder that has no matching field, if any.
fn render_template(template: &str, fields: &HashMap<String, String>) -> Result<String, String> {
    let mut result = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        result.push_str(&rest[..open]);
        let after_open = &rest[open + 1..];
        let close = after_open.find('}').ok_or_else(|| "unclosed '{'".to_string())?;
        let key = &after_open[..close];
        let value = fields.get(key).ok_or_else(|| key.to_string())?;
        result.push_str(value);
        rest = &after_open[close + 1..];
    }
    result.push_str(rest);
    Ok(result)
}

#[derive(Deserialize)]
pub struct TagRuleSpec {
    pattern: String,
    template: String,
}

fn default_true() -> bool {
    true
}

#[derive(Deserialize)]
pub struct ImportPathBody {
    /// Absolute path on the server's filesystem to scan; may be a single
    /// file or a directory.
    path: String,
    #[serde(default = "default_true")]
    recursive: bool,
    /// Fixed tags applied to every imported file.
    #[serde(default)]
    tags: Vec<String>,
    /// Regex/template pairs matched against each file's full path to derive
    /// additional tags — see `TagRule`.
    #[serde(default)]
    tag_rules: Vec<TagRuleSpec>,
    #[serde(default)]
    lowercase_tags: bool,
    /// When true, computes and returns tags/status without touching disk or
    /// the database.
    #[serde(default)]
    dry_run: bool,
}

#[derive(Serialize)]
pub struct ImportPathResponse {
    results: Vec<FileResult>,
    summary: ImportSummary,
}

/// Recursively (unless `recursive: false`) walks `root` and returns every
/// regular file found. Runs on a blocking thread since directory walking is
/// synchronous I/O.
async fn collect_files(root: PathBuf, recursive: bool) -> Result<Vec<PathBuf>, AppError> {
    tokio::task::spawn_blocking(move || {
        let mut walker = WalkDir::new(&root);
        if !recursive {
            walker = walker.max_depth(1);
        }
        walker
            .into_iter()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| entry.into_path())
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, anyhow::anyhow!(e)))
}

/// Imports every photo/video under a path *on the server's own filesystem*
/// (as opposed to `upload`, which receives file bytes from the client). Tags
/// are built from a fixed list plus any number of path-matched `tag_rules`.
/// Files whose content hash already exists on the server aren't re-read into
/// storage; only newly-missing tags are attached to the existing photo.
pub async fn import_path(
    State(state): State<AppState>,
    Json(body): Json<ImportPathBody>,
) -> Result<Json<ImportPathResponse>, AppError> {
    let root = StdPath::new(&body.path);
    if !root.is_absolute() {
        return Err(AppError::bad_request("path must be an absolute filesystem path on the server"));
    }
    let root = tokio::fs::canonicalize(root)
        .await
        .map_err(|e| AppError::bad_request(format!("cannot access path {}: {e}", body.path)))?;

    let rules: Vec<TagRule> = body
        .tag_rules
        .iter()
        .map(|r| TagRule::new(&r.pattern, &r.template))
        .collect::<Result<_, regex::Error>>()
        .map_err(|e| AppError::bad_request(format!("invalid tag-rule regex: {e}")))?;

    let files = collect_files(root, body.recursive).await?;

    let mut results = Vec::new();
    let mut summary = ImportSummary::default();

    for file_path in files {
        let mime_type = mime_guess::from_path(&file_path).first().map(|m| m.to_string());
        if mime_type.as_deref().and_then(media::MediaType::from_mime).is_none() {
            continue;
        }

        let path_str = file_path.to_string_lossy().into_owned();
        let filename = file_path.file_name().and_then(|s| s.to_str()).unwrap_or("upload").to_string();
        let stem = file_path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
        let parent = file_path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        let mut tag_set: BTreeSet<String> = body.tags.iter().map(|t| t.trim().to_string()).collect();
        for rule in &rules {
            if let Some(tag) = rule.apply(&path_str, &filename, &stem, &parent) {
                let tag = tag.trim().trim_matches('/').to_string();
                if !tag.is_empty() {
                    tag_set.insert(tag);
                }
            }
        }
        tag_set.remove("");
        let tags: Vec<String> = if body.lowercase_tags {
            tag_set.into_iter().map(|t| t.to_lowercase()).collect()
        } else {
            tag_set.into_iter().collect()
        };

        if body.dry_run {
            summary.record("dry_run");
            results.push(FileResult { path: path_str, status: "dry_run", tags, photo_id: None, error: None });
            continue;
        }

        let bytes = match tokio::fs::read(&file_path).await {
            Ok(bytes) => bytes,
            Err(e) => {
                summary.record("error");
                results.push(FileResult { path: path_str, status: "error", tags, photo_id: None, error: Some(e.to_string()) });
                continue;
            }
        };

        match ingest_bytes(&state, filename, None, bytes, &tags).await {
            Ok(outcome) => {
                let status = status_for(outcome.created, &outcome.tags_added);
                summary.record(status);
                results.push(FileResult { path: path_str, status, tags, photo_id: Some(outcome.photo.id), error: None });
            }
            Err(e) => {
                summary.record("error");
                results.push(FileResult { path: path_str, status: "error", tags, photo_id: None, error: Some(e.1.to_string()) });
            }
        }
    }

    Ok(Json(ImportPathResponse { results, summary }))
}
