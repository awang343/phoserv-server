use std::collections::HashSet;

use sqlx::SqlitePool;
use uuid::Uuid;

use crate::gallery_tags;
use crate::models::{Gallery, GalleryDetail, Photo, PhotoRow, PHOTO_COLUMNS};
use crate::tags;

pub async fn create(pool: &SqlitePool, title: &str, description: Option<&str>) -> anyhow::Result<String> {
    let id = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO galleries (id, title, description) VALUES (?, ?, ?)")
        .bind(&id)
        .bind(title)
        .bind(description)
        .execute(pool)
        .await?;
    Ok(id)
}

type SummaryRow = (String, String, Option<String>, String, i64, Option<String>);

const SUMMARY_QUERY: &str = r#"
    SELECT g.id, g.title, g.description, g.created_at,
           COUNT(DISTINCT gp.photo_id),
           (SELECT photo_id FROM gallery_photos WHERE gallery_id = g.id ORDER BY position LIMIT 1)
    FROM galleries g
    LEFT JOIN gallery_photos gp ON gp.gallery_id = g.id
"#;

fn summary_from_row(row: SummaryRow, tags: Vec<String>) -> Gallery {
    let (id, title, description, created_at, photo_count, cover_photo_id) = row;
    Gallery { id, title, description, cover_photo_id, photo_count, tags, created_at }
}

pub async fn get_summary(pool: &SqlitePool, id: &str) -> anyhow::Result<Option<Gallery>> {
    let row: Option<SummaryRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{SUMMARY_QUERY} WHERE g.id = ? GROUP BY g.id"
    )))
    .bind(id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let tags = gallery_tags::tags_for_gallery(pool, id).await?;
    Ok(Some(summary_from_row(row, tags)))
}

/// Lists galleries, most recently created first. When `tag_ids` is given
/// (a tag id plus its descendants, mirroring how `photos::list` filters by
/// tag), only galleries carrying one of those gallery tags are returned.
pub async fn list_summaries(pool: &SqlitePool, tag_ids: Option<Vec<i64>>) -> anyhow::Result<Vec<Gallery>> {
    let rows: Vec<SummaryRow> = match &tag_ids {
        Some(ids) if ids.is_empty() => vec![],
        Some(ids) => {
            let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let query = format!(
                "{SUMMARY_QUERY} JOIN gallery_tag_links gt ON gt.gallery_id = g.id \
                 WHERE gt.tag_id IN ({placeholders}) GROUP BY g.id ORDER BY g.created_at DESC"
            );
            let mut q = sqlx::query_as(sqlx::AssertSqlSafe(query));
            for id in ids {
                q = q.bind(id);
            }
            q.fetch_all(pool).await?
        }
        None => {
            let query = format!("{SUMMARY_QUERY} GROUP BY g.id ORDER BY g.created_at DESC");
            sqlx::query_as(sqlx::AssertSqlSafe(query)).fetch_all(pool).await?
        }
    };

    let ids: Vec<String> = rows.iter().map(|r| r.0.clone()).collect();
    let mut tag_map = gallery_tags::tags_for_galleries(pool, &ids).await?;

    Ok(rows
        .into_iter()
        .map(|row| {
            let tags = tag_map.remove(&row.0).unwrap_or_default();
            summary_from_row(row, tags)
        })
        .collect())
}

pub async fn get_detail(pool: &SqlitePool, id: &str) -> anyhow::Result<Option<GalleryDetail>> {
    let row: Option<(String, String, Option<String>, String)> =
        sqlx::query_as("SELECT id, title, description, created_at FROM galleries WHERE id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    let Some((id, title, description, created_at)) = row else { return Ok(None) };

    let photo_rows: Vec<PhotoRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT p.{} FROM photos p JOIN gallery_photos gp ON gp.photo_id = p.id WHERE gp.gallery_id = ? ORDER BY gp.position",
        PHOTO_COLUMNS.replace(", ", ", p.")
    )))
    .bind(&id)
    .fetch_all(pool)
    .await?;

    let photo_ids: Vec<String> = photo_rows.iter().map(|r| r.id.clone()).collect();
    let mut photo_tag_map = tags::tags_for_photos(pool, &photo_ids).await?;
    let photos: Vec<Photo> = photo_rows
        .into_iter()
        .map(|row| {
            let photo_tags = photo_tag_map.remove(&row.id).unwrap_or_default();
            Photo::from_row(row, photo_tags)
        })
        .collect();

    let tags = gallery_tags::tags_for_gallery(pool, &id).await?;

    Ok(Some(GalleryDetail { id, title, description, tags, created_at, photos }))
}

pub async fn update(pool: &SqlitePool, id: &str, title: Option<&str>, description: Option<&str>) -> anyhow::Result<()> {
    if let Some(t) = title {
        sqlx::query("UPDATE galleries SET title = ? WHERE id = ?")
            .bind(t.trim())
            .bind(id)
            .execute(pool)
            .await?;
    }
    if let Some(d) = description {
        sqlx::query("UPDATE galleries SET description = ? WHERE id = ?")
            .bind(d)
            .bind(id)
            .execute(pool)
            .await?;
    }
    Ok(())
}

pub async fn delete(pool: &SqlitePool, id: &str) -> anyhow::Result<bool> {
    let result = sqlx::query("DELETE FROM galleries WHERE id = ?").bind(id).execute(pool).await?;
    Ok(result.rows_affected() > 0)
}

/// Appends photos to the end of a gallery, in the given order. Photos
/// already in the gallery are left at their existing position rather than
/// moved.
pub async fn add_photos(pool: &SqlitePool, gallery_id: &str, photo_ids: &[String]) -> anyhow::Result<()> {
    let (max_position,): (Option<i64>,) =
        sqlx::query_as("SELECT MAX(position) FROM gallery_photos WHERE gallery_id = ?")
            .bind(gallery_id)
            .fetch_one(pool)
            .await?;
    let mut next = max_position.unwrap_or(0) + 1;
    for photo_id in photo_ids {
        let result = sqlx::query(
            "INSERT INTO gallery_photos (gallery_id, photo_id, position) VALUES (?, ?, ?) ON CONFLICT DO NOTHING",
        )
        .bind(gallery_id)
        .bind(photo_id)
        .bind(next)
        .execute(pool)
        .await?;
        if result.rows_affected() > 0 {
            next += 1;
        }
    }
    Ok(())
}

pub async fn remove_photos(pool: &SqlitePool, gallery_id: &str, photo_ids: &[String]) -> anyhow::Result<()> {
    for photo_id in photo_ids {
        sqlx::query("DELETE FROM gallery_photos WHERE gallery_id = ? AND photo_id = ?")
            .bind(gallery_id)
            .bind(photo_id)
            .execute(pool)
            .await?;
    }
    Ok(())
}

pub enum ReorderError {
    Mismatch,
    Other(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for ReorderError {
    fn from(err: E) -> Self {
        ReorderError::Other(err.into())
    }
}

/// Replaces a gallery's page order wholesale: `photo_ids` must contain
/// exactly the photo ids currently in the gallery, in their new order. A
/// mismatched set (missing or unexpected ids) is rejected outright rather
/// than silently reconciled, since silently dropping/appending pages would
/// be surprising for a manual reorder action.
pub async fn reorder(pool: &SqlitePool, gallery_id: &str, photo_ids: &[String]) -> Result<(), ReorderError> {
    let current: Vec<(String,)> = sqlx::query_as("SELECT photo_id FROM gallery_photos WHERE gallery_id = ?")
        .bind(gallery_id)
        .fetch_all(pool)
        .await?;
    let mut remaining: HashSet<&str> = current.iter().map(|(id,)| id.as_str()).collect();
    for id in photo_ids {
        if !remaining.remove(id.as_str()) {
            return Err(ReorderError::Mismatch);
        }
    }
    if !remaining.is_empty() {
        return Err(ReorderError::Mismatch);
    }

    let mut tx = pool.begin().await?;
    // Shift everything into the negative range first so the
    // UNIQUE(gallery_id, position) index never sees a duplicate while the
    // new positions are being written one row at a time.
    sqlx::query("UPDATE gallery_photos SET position = -position WHERE gallery_id = ?")
        .bind(gallery_id)
        .execute(&mut *tx)
        .await?;
    for (i, photo_id) in photo_ids.iter().enumerate() {
        sqlx::query("UPDATE gallery_photos SET position = ? WHERE gallery_id = ? AND photo_id = ?")
            .bind(i as i64 + 1)
            .bind(gallery_id)
            .bind(photo_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}
