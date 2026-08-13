use std::collections::HashSet;
use std::path::Path as StdPath;

use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::AppError;
use crate::models::{Photo, PhotoRow, PHOTO_COLUMNS};
use crate::{media, storage, tags, AppState};

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

pub struct IngestOutcome {
    pub photo: Photo,
    pub created: bool,
    pub tags_added: Vec<String>,
}

/// Stores `bytes` as a photo (deduping by content hash, same as everywhere
/// else) and attaches `tag_paths` to it, creating any missing tag segments.
/// Shared by every ingestion path that ends up with raw file bytes: the
/// multipart `upload` handler, `photos::import_path` (reads bytes off disk),
/// and `downloaders::run` (reads bytes a downloader script staged on disk).
pub async fn ingest_bytes(
    state: &AppState,
    filename: String,
    content_type: Option<String>,
    bytes: Vec<u8>,
    tag_paths: &[String],
) -> Result<IngestOutcome, AppError> {
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

    let created = existing.is_none();

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

    let tags_before: HashSet<String> = if created {
        Default::default()
    } else {
        tags::tags_for_photo(&state.pool, &photo_row.id).await?.into_iter().collect()
    };

    for path in tag_paths {
        let tag_id = tags::resolve_or_create(&state.pool, path).await?;
        sqlx::query("INSERT OR IGNORE INTO photo_tags (photo_id, tag_id) VALUES (?, ?)")
            .bind(&photo_row.id)
            .bind(tag_id)
            .execute(&state.pool)
            .await?;
    }

    let tag_list = tags::tags_for_photo(&state.pool, &photo_row.id).await?;
    let tags_added: Vec<String> = tag_paths.iter().filter(|t| !tags_before.contains(t.as_str())).cloned().collect();

    Ok(IngestOutcome { photo: Photo::from_row(photo_row, tag_list), created, tags_added })
}

/// The outcome of ingesting a single file, used by both `photos::import_path`
/// and `downloaders::run` to report per-file progress in the same shape.
#[derive(Serialize, Clone)]
pub struct FileResult {
    pub path: String,
    pub status: &'static str,
    pub tags: Vec<String>,
    pub photo_id: Option<String>,
    pub error: Option<String>,
}

#[derive(Serialize, Clone, Default)]
pub struct ImportSummary {
    pub scanned: usize,
    pub uploaded: usize,
    pub tagged: usize,
    pub skipped: usize,
    pub errors: usize,
}

impl ImportSummary {
    pub fn record(&mut self, status: &str) {
        self.scanned += 1;
        match status {
            "uploaded" => self.uploaded += 1,
            "tagged" => self.tagged += 1,
            "skipped" => self.skipped += 1,
            "error" => self.errors += 1,
            _ => {}
        }
    }
}

/// Classifies an `ingest_bytes` outcome into the "uploaded" / "tagged" /
/// "skipped" status strings shared across the import-path and downloader
/// result views.
pub fn status_for(created: bool, tags_added: &[String]) -> &'static str {
    if created {
        "uploaded"
    } else if !tags_added.is_empty() {
        "tagged"
    } else {
        "skipped"
    }
}
