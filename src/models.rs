use serde::Serialize;

pub const PHOTO_COLUMNS: &str = "id, hash, original_filename, mime_type, media_type, file_size, width, height, duration_seconds, taken_at, created_at";

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PhotoRow {
    pub id: String,
    pub hash: String,
    pub original_filename: String,
    pub mime_type: String,
    pub media_type: String,
    pub file_size: i64,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub duration_seconds: Option<f64>,
    pub taken_at: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Serialize)]
pub struct Photo {
    pub id: String,
    pub hash: String,
    pub original_filename: String,
    pub mime_type: String,
    pub media_type: String,
    pub file_size: i64,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub duration_seconds: Option<f64>,
    pub taken_at: Option<String>,
    pub created_at: String,
    pub tags: Vec<String>,
}

impl Photo {
    pub fn from_row(row: PhotoRow, tags: Vec<String>) -> Self {
        Photo {
            id: row.id,
            hash: row.hash,
            original_filename: row.original_filename,
            mime_type: row.mime_type,
            media_type: row.media_type,
            file_size: row.file_size,
            width: row.width,
            height: row.height,
            duration_seconds: row.duration_seconds,
            taken_at: row.taken_at,
            created_at: row.created_at,
            tags,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct TagNode {
    pub id: i64,
    pub name: String,
    pub path: String,
    /// Number of distinct items (photos or galleries, depending on which
    /// tag tree this node came from) tagged with this tag or any of its
    /// descendants.
    pub count: i64,
    pub children: Vec<TagNode>,
}

/// A gallery as shown in list views: metadata plus derived fields (page
/// count, cover) computed from its member photos rather than stored.
#[derive(Debug, Serialize)]
pub struct Gallery {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub cover_photo_id: Option<String>,
    pub photo_count: i64,
    pub tags: Vec<String>,
    pub created_at: String,
}

/// A single gallery with its full, ordered page list — what the gallery
/// viewer/reader fetches.
#[derive(Debug, Serialize)]
pub struct GalleryDetail {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub created_at: String,
    pub photos: Vec<Photo>,
}
