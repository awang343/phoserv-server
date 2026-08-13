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
--write-metadata as a "<file>.json" sidecar next to each download):

  * source/<category>              e.g. source/twitter, source/danbooru
  * source/<category>/<subcategory> when gallery-dl reports a distinct one
  * artist/<name>                  from the first of artist/author/uploader/user/creator
  * booru/<tag>                    one per entry in a site's native tag list, if any

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

# Metadata keys gallery-dl's various extractors use for the uploader/artist,
# checked in order -- first non-empty match wins.
ARTIST_KEYS = ("artist", "author", "uploader", "user", "creator")

# Metadata keys commonly holding a site's own list of tags.
TAG_LIST_KEYS = ("tags", "tag_string")


def log(msg: str) -> None:
    print(msg, flush=True)


def find_gallery_dl() -> str:
    exe = shutil.which("gallery-dl")
    if not exe:
        log("gallery-dl not found on PATH -- install it with `pip install gallery-dl`")
        sys.exit(1)
    return exe


def build_tags(metadata: dict) -> list[str]:
    tags: list[str] = []

    category = metadata.get("category")
    subcategory = metadata.get("subcategory")
    if category:
        tags.append(f"source/{category}")
        if subcategory and subcategory != category:
            tags.append(f"source/{category}/{subcategory}")

    for key in ARTIST_KEYS:
        value = metadata.get(key)
        if isinstance(value, str) and value.strip():
            tags.append(f"artist/{value.strip()}")
            break

    for key in TAG_LIST_KEYS:
        value = metadata.get(key)
        if isinstance(value, str):
            value = value.split()
        if isinstance(value, list):
            for tag in value:
                tag = str(tag).strip()
                if tag:
                    tags.append(f"booru/{tag}")
            break

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
