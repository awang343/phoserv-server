use std::collections::HashMap;

use sqlx::SqlitePool;

use crate::models::TagNode;

/// Reserved top-level tag used to mark photos as soft-deleted ("trash").
/// Excluded from the tag tree and autocomplete suggestions so it doesn't
/// appear alongside user-created tags.
pub const TRASH_TAG: &str = "trash";

/// Resolves the trash tag's id, if it has ever been created (i.e. at least
/// one photo has been trashed).
pub async fn trash_tag_id(pool: &SqlitePool) -> anyhow::Result<Option<i64>> {
    find_by_path(pool, TRASH_TAG).await
}

/// Splits a tag path like "people/alice" into trimmed, non-empty segments.
pub fn split_path(path: &str) -> Vec<&str> {
    path.split('/').map(str::trim).filter(|s| !s.is_empty()).collect()
}

/// Resolves a tag path to its leaf tag id, creating any missing segments.
pub async fn resolve_or_create(pool: &SqlitePool, path: &str) -> anyhow::Result<i64> {
    let segments = split_path(path);
    if segments.is_empty() {
        anyhow::bail!("tag path must not be empty");
    }

    let mut parent_id: i64 = 0;
    for segment in segments {
        let existing: Option<(i64,)> =
            sqlx::query_as("SELECT id FROM tags WHERE parent_id = ? AND name = ?")
                .bind(parent_id)
                .bind(segment)
                .fetch_optional(pool)
                .await?;

        parent_id = match existing {
            Some((id,)) => id,
            None => {
                let result = sqlx::query("INSERT INTO tags (name, parent_id) VALUES (?, ?)")
                    .bind(segment)
                    .bind(parent_id)
                    .execute(pool)
                    .await?;
                result.last_insert_rowid()
            }
        };
    }
    Ok(parent_id)
}

/// Finds a tag id by its full path, without creating it. Returns None if any
/// segment is missing.
pub async fn find_by_path(pool: &SqlitePool, path: &str) -> anyhow::Result<Option<i64>> {
    let segments = split_path(path);
    if segments.is_empty() {
        return Ok(None);
    }

    let mut parent_id: i64 = 0;
    for segment in segments {
        let existing: Option<(i64,)> =
            sqlx::query_as("SELECT id FROM tags WHERE parent_id = ? AND name = ?")
                .bind(parent_id)
                .bind(segment)
                .fetch_optional(pool)
                .await?;
        match existing {
            Some((id,)) => parent_id = id,
            None => return Ok(None),
        }
    }
    Ok(Some(parent_id))
}

/// Returns the tag id itself plus all of its descendant tag ids.
pub async fn descendant_ids(pool: &SqlitePool, tag_id: i64) -> anyhow::Result<Vec<i64>> {
    let rows: Vec<(i64,)> = sqlx::query_as(
        r#"
        WITH RECURSIVE descendants(id) AS (
            SELECT id FROM tags WHERE id = ?
            UNION ALL
            SELECT t.id FROM tags t JOIN descendants d ON t.parent_id = d.id
        )
        SELECT id FROM descendants
        "#,
    )
    .bind(tag_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

struct TagRow {
    id: i64,
    name: String,
    parent_id: i64,
}

async fn fetch_all_tags(pool: &SqlitePool) -> anyhow::Result<Vec<TagRow>> {
    let rows: Vec<(i64, String, i64)> =
        sqlx::query_as("SELECT id, name, parent_id FROM tags WHERE id != 0")
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .map(|(id, name, parent_id)| TagRow { id, name, parent_id })
        .collect())
}

/// Returns the full "/"-joined paths for a set of tag ids.
pub async fn full_paths(pool: &SqlitePool, tag_ids: &[i64]) -> anyhow::Result<Vec<String>> {
    let all = fetch_all_tags(pool).await?;
    let by_id: HashMap<i64, &TagRow> = all.iter().map(|t| (t.id, t)).collect();

    let mut paths = Vec::with_capacity(tag_ids.len());
    for id in tag_ids {
        if let Some(mut node) = by_id.get(id).copied() {
            let mut segments = vec![node.name.clone()];
            while node.parent_id != 0 {
                if let Some(parent) = by_id.get(&node.parent_id) {
                    segments.push(parent.name.clone());
                    node = parent;
                } else {
                    break;
                }
            }
            segments.reverse();
            paths.push(segments.join("/"));
        }
    }
    Ok(paths)
}

pub async fn tags_for_photo(pool: &SqlitePool, photo_id: &str) -> anyhow::Result<Vec<String>> {
    let rows: Vec<(i64,)> =
        sqlx::query_as("SELECT tag_id FROM photo_tags WHERE photo_id = ?")
            .bind(photo_id)
            .fetch_all(pool)
            .await?;
    let ids: Vec<i64> = rows.into_iter().map(|(id,)| id).collect();
    full_paths(pool, &ids).await
}

/// Batch variant of `tags_for_photo` that fetches the entire tag table once
/// and all relevant photo_tags rows once, to avoid N+1 queries when listing.
pub async fn tags_for_photos(
    pool: &SqlitePool,
    photo_ids: &[String],
) -> anyhow::Result<HashMap<String, Vec<String>>> {
    let all = fetch_all_tags(pool).await?;
    let by_id: HashMap<i64, &TagRow> = all.iter().map(|t| (t.id, t)).collect();

    let mut result: HashMap<String, Vec<String>> =
        photo_ids.iter().map(|id| (id.clone(), Vec::new())).collect();

    if photo_ids.is_empty() {
        return Ok(result);
    }

    let placeholders = photo_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let query = format!(
        "SELECT photo_id, tag_id FROM photo_tags WHERE photo_id IN ({placeholders})"
    );
    let mut q = sqlx::query_as::<_, (String, i64)>(sqlx::AssertSqlSafe(query));
    for id in photo_ids {
        q = q.bind(id);
    }
    let rows = q.fetch_all(pool).await?;

    fn path_for(id: i64, by_id: &HashMap<i64, &TagRow>) -> Option<String> {
        let mut node = *by_id.get(&id)?;
        let mut segments = vec![node.name.clone()];
        while node.parent_id != 0 {
            node = by_id.get(&node.parent_id)?;
            segments.push(node.name.clone());
        }
        segments.reverse();
        Some(segments.join("/"))
    }

    for (photo_id, tag_id) in rows {
        if let Some(path) = path_for(tag_id, &by_id) {
            result.entry(photo_id).or_default().push(path);
        }
    }

    Ok(result)
}

pub enum RenameError {
    InvalidName,
    NotFound,
    NameConflict,
    Other(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for RenameError {
    fn from(err: E) -> Self {
        RenameError::Other(err.into())
    }
}

/// Renames a tag in place, keeping its parent and children unchanged.
pub async fn rename(pool: &SqlitePool, tag_id: i64, new_name: &str) -> Result<(), RenameError> {
    let name = new_name.trim();
    if name.is_empty() || name.contains('/') || tag_id == 0 {
        return Err(RenameError::InvalidName);
    }

    let result = sqlx::query("UPDATE tags SET name = ? WHERE id = ?")
        .bind(name)
        .bind(tag_id)
        .execute(pool)
        .await;

    match result {
        Ok(res) if res.rows_affected() == 0 => Err(RenameError::NotFound),
        Ok(_) => Ok(()),
        Err(sqlx::Error::Database(db_err)) if db_err.is_unique_violation() => {
            Err(RenameError::NameConflict)
        }
        Err(e) => Err(e.into()),
    }
}

pub enum DeleteError {
    NotFound,
    Protected,
    Other(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for DeleteError {
    fn from(err: E) -> Self {
        DeleteError::Other(err.into())
    }
}

/// Deletes a tag everywhere: the tag row, any descendant tags (via
/// `ON DELETE CASCADE` on `tags.parent_id`), and its associations in
/// `photo_tags` (via `ON DELETE CASCADE` on `photo_tags.tag_id`) all go in
/// one statement.
pub async fn delete(pool: &SqlitePool, tag_id: i64) -> Result<(), DeleteError> {
    if tag_id == 0 || Some(tag_id) == trash_tag_id(pool).await? {
        return Err(DeleteError::Protected);
    }
    let result = sqlx::query("DELETE FROM tags WHERE id = ?")
        .bind(tag_id)
        .execute(pool)
        .await?;
    if result.rows_affected() == 0 {
        return Err(DeleteError::NotFound);
    }
    Ok(())
}

pub async fn build_tree(pool: &SqlitePool) -> anyhow::Result<Vec<TagNode>> {
    let all = fetch_all_tags(pool).await?;
    let mut children_of: HashMap<i64, Vec<&TagRow>> = HashMap::new();
    for tag in &all {
        children_of.entry(tag.parent_id).or_default().push(tag);
    }

    fn build(parent_id: i64, parent_path: &str, children_of: &HashMap<i64, Vec<&TagRow>>) -> Vec<TagNode> {
        let mut nodes = Vec::new();
        if let Some(children) = children_of.get(&parent_id) {
            for tag in children {
                if parent_id == 0 && tag.name == TRASH_TAG {
                    continue;
                }
                let path = if parent_path.is_empty() {
                    tag.name.clone()
                } else {
                    format!("{parent_path}/{}", tag.name)
                };
                nodes.push(TagNode {
                    id: tag.id,
                    name: tag.name.clone(),
                    children: build(tag.id, &path, children_of),
                    path,
                });
            }
        }
        nodes.sort_by(|a, b| a.name.cmp(&b.name));
        nodes
    }

    Ok(build(0, "", &children_of))
}
