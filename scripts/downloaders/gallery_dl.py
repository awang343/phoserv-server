#!/usr/bin/env python3
"""Downloader script that shells out to gallery-dl to fetch a URL (a post,
gallery, user feed, search, etc. -- anything gallery-dl's extractors support)
and imports every downloaded file into phoserv.

Follows the same contract as every script in this directory (see
downloaders_path in config.toml and DownloaderPanel in the web app):

  * Invoked as `<script> <url>`.
  * PHOSERV_STAGING_DIR / cwd is a fresh directory to download into.
  * Emits one JSON line per file for the server to ingest:
        {"file": "<path relative to the staging dir>", "tags": [...]}
  * All other stdout is just relayed to the job log.
  * Exit code mirrors gallery-dl's own; files already emitted before a
    failure are still imported.

Tags are derived from gallery-dl's own per-file metadata (written via
--write-metadata as a "<file>.json" sidecar next to each download). Only
four are emitted, each only when the site's extractor actually reports the
underlying field:

  * source/<category>              extractor name, e.g. source/twitter, source/danbooru
  * source_gallery/<gallery_id>    the gallery/album/set id, for sites with that concept
  * source_id/<unique_id>          the specific post/image's own unique id
  * source_creator/<uploader>      the artist/author/uploader

Requires: pip install gallery-dl
"""
from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

METADATA_SUFFIX = ".json"

# Metadata keys checked in order for each tag -- first non-empty match wins.
# Field names aren't standardized across gallery-dl's extractors, so each
# tuple lists every name commonly seen for that concept.
GALLERY_ID_KEYS = ("gallery_id", "album_id", "set_id")
UNIQUE_ID_KEYS = ("id", "post_id", "illust_id", "tweet_id", "pid")
CREATOR_KEYS = ("artist", "author", "uploader", "user", "creator")


def log(msg: str) -> None:
    print(msg, flush=True)


def find_gallery_dl() -> str:
    exe = shutil.which("gallery-dl")
    if not exe:
        log("gallery-dl not found on PATH -- install it with `pip install gallery-dl`")
        sys.exit(1)
    return exe


def first_value(metadata: dict, keys: tuple[str, ...]) -> str | None:
    """Returns the first non-empty value found under `keys`, as a string.
    Some extractors report these as nested dicts (e.g. a Twitter author is
    `{"name": ..., "nick": ..., "id": ...}`) rather than a plain string."""
    for key in keys:
        value = metadata.get(key)
        if isinstance(value, dict):
            value = value.get("name") or value.get("nick") or value.get("id")
        if value is None:
            continue
        value = str(value).strip()
        if value:
            return value
    return None


def build_tags(metadata: dict) -> list[str]:
    tags: list[str] = []

    category = metadata.get("category")
    if category:
        tags.append(f"source/{category}")

    gallery_id = first_value(metadata, GALLERY_ID_KEYS)
    if gallery_id:
        tags.append(f"source_gallery/{gallery_id}")

    unique_id = first_value(metadata, UNIQUE_ID_KEYS)
    if unique_id:
        tags.append(f"source_id/{unique_id}")

    creator = first_value(metadata, CREATOR_KEYS)
    if creator:
        tags.append(f"source_creator/{creator}")

    return tags


def is_sidecar(path: Path) -> bool:
    """True if `path` is gallery-dl's "<file>.json" metadata sidecar for
    another downloaded file next to it, rather than downloaded media itself."""
    return path.suffix == METADATA_SUFFIX and path.with_suffix("").exists()


def collect_downloads(staging_dir: Path) -> list[Path]:
    return sorted(p for p in staging_dir.rglob("*") if p.is_file() and not is_sidecar(p))


def emit_manifest_line(staging_dir: Path, media_path: Path) -> None:
    sidecar = media_path.with_suffix(media_path.suffix + METADATA_SUFFIX)
    metadata: dict = {}
    if sidecar.exists():
        try:
            metadata = json.loads(sidecar.read_text())
        except (OSError, json.JSONDecodeError) as e:
            log(f"warning: failed to read metadata for {media_path.name}: {e}")

    relpath = media_path.relative_to(staging_dir).as_posix()
    print(json.dumps({"file": relpath, "tags": build_tags(metadata)}))


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: gallery_dl.py <url>", file=sys.stderr)
        return 1

    url = sys.argv[1]
    staging_dir = Path(os.environ.get("PHOSERV_STAGING_DIR", "."))
    gallery_dl = find_gallery_dl()

    log(f"running gallery-dl for {url}")
    result = subprocess.run(
        [gallery_dl, "--dest", str(staging_dir), "--write-metadata", "--no-mtime", url],
        cwd=staging_dir,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    for line in result.stdout.splitlines():
        log(line)

    downloaded = collect_downloads(staging_dir)
    if not downloaded:
        log("gallery-dl produced no files")
    for media_path in downloaded:
        emit_manifest_line(staging_dir, media_path)

    if result.returncode != 0:
        log(f"gallery-dl exited with status {result.returncode}")
    return result.returncode


if __name__ == "__main__":
    sys.exit(main())
