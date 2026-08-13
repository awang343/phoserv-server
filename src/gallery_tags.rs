use std::collections::HashMap;

use sqlx::SqlitePool;

use crate::models::TagNode;
use crate::tags::{split_path, DeleteError, RenameError};

/// Resolves a gallery tag path to its leaf tag id, creating any missing
/// segments. Mirrors `tags::resolve_or_create` but against the separate
/// `gallery_tags` tree.
pub async fn resolve_or_create(pool: &SqlitePool, path: &str) -> anyhow::Result<i64> {
    let segments = split_path(path);
    if segments.is_empty() {
        anyhow::bail!("tag path must not be empty");
    }

    let mut parent_id: i64 = 0;
    for segment in segments {
        sqlx::query(
            "INSERT INTO gallery_tags (name, parent_id) VALUES (?, ?) ON CONFLICT (parent_id, name) DO NOTHING",
        )
        .bind(segment)
        .bind(parent_id)
        .execute(pool)
        .await?;

        let (id,): (i64,) = sqlx::query_as("SELECT id FROM gallery_tags WHERE parent_id = ? AND name = ?")
            .bind(parent_id)
            .bind(segment)
            .fetch_one(pool)
            .await?;
        parent_id = id;
    }
    Ok(parent_id)
}

/// Finds a gallery tag id by its full path, without creating it.
pub async fn find_by_path(pool: &SqlitePool, path: &str) -> anyhow::Result<Option<i64>> {
    let segments = split_path(path);
    if segments.is_empty() {
        return Ok(None);
    }

    let mut parent_id: i64 = 0;
    for segment in segments {
        let existing: Option<(i64,)> =
            sqlx::query_as("SELECT id FROM gallery_tags WHERE parent_id = ? AND name = ?")
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
            SELECT id FROM gallery_tags WHERE id = ?
            UNION ALL
            SELECT t.id FROM gallery_tags t JOIN descendants d ON t.parent_id = d.id
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
        sqlx::query_as("SELECT id, name, parent_id FROM gallery_tags WHERE id != 0")
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .map(|(id, name, parent_id)| TagRow { id, name, parent_id })
        .collect())
}

/// Returns the full "/"-joined paths for a set of gallery tag ids.
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

pub async fn tags_for_gallery(pool: &SqlitePool, gallery_id: &str) -> anyhow::Result<Vec<String>> {
    let rows: Vec<(i64,)> = sqlx::query_as("SELECT tag_id FROM gallery_tag_links WHERE gallery_id = ?")
        .bind(gallery_id)
        .fetch_all(pool)
        .await?;
    let ids: Vec<i64> = rows.into_iter().map(|(id,)| id).collect();
    full_paths(pool, &ids).await
}

/// Batch variant of `tags_for_gallery` that fetches the entire gallery tag
/// table once and all relevant link rows once, to avoid N+1 queries when
/// listing galleries.
pub async fn tags_for_galleries(
    pool: &SqlitePool,
    gallery_ids: &[String],
) -> anyhow::Result<HashMap<String, Vec<String>>> {
    let all = fetch_all_tags(pool).await?;
    let by_id: HashMap<i64, &TagRow> = all.iter().map(|t| (t.id, t)).collect();

    let mut result: HashMap<String, Vec<String>> =
        gallery_ids.iter().map(|id| (id.clone(), Vec::new())).collect();

    if gallery_ids.is_empty() {
        return Ok(result);
    }

    let placeholders = gallery_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let query = format!("SELECT gallery_id, tag_id FROM gallery_tag_links WHERE gallery_id IN ({placeholders})");
    let mut q = sqlx::query_as::<_, (String, i64)>(sqlx::AssertSqlSafe(query));
    for id in gallery_ids {
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

    for (gallery_id, tag_id) in rows {
        if let Some(path) = path_for(tag_id, &by_id) {
            result.entry(gallery_id).or_default().push(path);
        }
    }

    Ok(result)
}

/// Renames a gallery tag in place. Reuses `tags::RenameError` since the
/// error cases (empty/slashed name, missing row, name collision) are
/// identical regardless of which tag tree they came from.
pub async fn rename(pool: &SqlitePool, tag_id: i64, new_name: &str) -> Result<(), RenameError> {
    let name = new_name.trim();
    if name.is_empty() || name.contains('/') || tag_id == 0 {
        return Err(RenameError::InvalidName);
    }

    let result = sqlx::query("UPDATE gallery_tags SET name = ? WHERE id = ?")
        .bind(name)
        .bind(tag_id)
        .execute(pool)
        .await;

    match result {
        Ok(res) if res.rows_affected() == 0 => Err(RenameError::NotFound),
        Ok(_) => Ok(()),
        Err(sqlx::Error::Database(db_err)) if db_err.is_unique_violation() => Err(RenameError::NameConflict),
        Err(e) => Err(e.into()),
    }
}

/// Deletes a gallery tag everywhere: the tag row, any descendant tags, and
/// its links in `gallery_tag_links` all go in one statement via cascades.
pub async fn delete(pool: &SqlitePool, tag_id: i64) -> Result<(), DeleteError> {
    if tag_id == 0 {
        return Err(DeleteError::Protected);
    }
    let result = sqlx::query("DELETE FROM gallery_tags WHERE id = ?")
        .bind(tag_id)
        .execute(pool)
        .await?;
    if result.rows_affected() == 0 {
        return Err(DeleteError::NotFound);
    }
    Ok(())
}

/// Computes, for every gallery tag, the number of distinct galleries tagged
/// with that tag or any of its descendants, in a single recursive-CTE query.
/// Mirrors `tags::photo_counts`.
async fn gallery_counts(pool: &SqlitePool) -> anyhow::Result<HashMap<i64, i64>> {
    let rows: Vec<(i64, i64)> = sqlx::query_as(
        r#"
        WITH RECURSIVE ancestors(tag_id, ancestor_id) AS (
            SELECT id, id FROM gallery_tags WHERE id != 0
            UNION ALL
            SELECT a.tag_id, t.parent_id
            FROM ancestors a
            JOIN gallery_tags t ON t.id = a.ancestor_id
            WHERE t.parent_id != 0
        )
        SELECT a.ancestor_id, COUNT(DISTINCT gtl.gallery_id)
        FROM ancestors a
        JOIN gallery_tag_links gtl ON gtl.tag_id = a.tag_id
        GROUP BY a.ancestor_id
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

pub async fn build_tree(pool: &SqlitePool) -> anyhow::Result<Vec<TagNode>> {
    let all = fetch_all_tags(pool).await?;
    let counts = gallery_counts(pool).await?;
    let mut children_of: HashMap<i64, Vec<&TagRow>> = HashMap::new();
    for tag in &all {
        children_of.entry(tag.parent_id).or_default().push(tag);
    }

    fn build(
        parent_id: i64,
        parent_path: &str,
        children_of: &HashMap<i64, Vec<&TagRow>>,
        counts: &HashMap<i64, i64>,
    ) -> Vec<TagNode> {
        let mut nodes = Vec::new();
        if let Some(children) = children_of.get(&parent_id) {
            for tag in children {
                let path = if parent_path.is_empty() {
                    tag.name.clone()
                } else {
                    format!("{parent_path}/{}", tag.name)
                };
                nodes.push(TagNode {
                    id: tag.id,
                    name: tag.name.clone(),
                    children: build(tag.id, &path, children_of, counts),
                    count: counts.get(&tag.id).copied().unwrap_or(0),
                    path,
                });
            }
        }
        nodes.sort_by(|a, b| a.name.cmp(&b.name));
        nodes
    }

    Ok(build(0, "", &children_of, &counts))
}
