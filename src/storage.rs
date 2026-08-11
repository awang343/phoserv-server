use std::path::{Path, PathBuf};

/// Shards a hex hash into a two-level directory prefix, e.g.
/// "ab" / "cd" for hash "abcd1234...".
fn shard_dir(root: &Path, hash: &str) -> PathBuf {
    root.join(&hash[0..2]).join(&hash[2..4])
}

pub fn original_path(library_path: &Path, hash: &str, ext: &str) -> PathBuf {
    shard_dir(&library_path.join("originals"), hash).join(format!("{hash}.{ext}"))
}

pub fn thumbnail_path(library_path: &Path, hash: &str, size: &str) -> PathBuf {
    shard_dir(&library_path.join("thumbnails"), hash).join(format!("{hash}_{size}.jpg"))
}

pub async fn store_original(library_path: &Path, hash: &str, ext: &str, bytes: &[u8]) -> anyhow::Result<PathBuf> {
    let path = original_path(library_path, hash, ext);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    if !tokio::fs::try_exists(&path).await? {
        tokio::fs::write(&path, bytes).await?;
    }
    Ok(path)
}

async fn remove_if_exists(path: &Path) -> anyhow::Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub async fn delete_original(library_path: &Path, hash: &str, ext: &str) -> anyhow::Result<()> {
    remove_if_exists(&original_path(library_path, hash, ext)).await
}

pub async fn delete_thumbnails(library_path: &Path, hash: &str) -> anyhow::Result<()> {
    for size in ["sm", "md"] {
        remove_if_exists(&thumbnail_path(library_path, hash, size)).await?;
    }
    Ok(())
}

