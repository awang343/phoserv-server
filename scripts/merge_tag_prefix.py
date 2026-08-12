#!/usr/bin/env python3
"""Bulk-merge one tag namespace into another on phoserv, e.g. move every
photo tagged under "series/..." to the equivalent "fandom/..." tag,
preserving whatever sub-hierarchy exists below the prefix
(series/anime/naruto -> fandom/anime/naruto).

For each photo currently tagged under --from-prefix, this adds the
corresponding --to-prefix tag (via POST /api/photos/{id}/tags) and removes
the old --from-prefix tag (via DELETE /api/photos/{id}/tags). If a photo
already has both (e.g. it was independently tagged "fandom/foo" and
"series/foo"), the add is a no-op and only the removal happens, so the two
namespaces cleanly merge instead of leaving duplicates.

Requires: pip install requests

Usage:
  python3 merge_tag_prefix.py --server-url http://localhost:4173 \\
      --server-token <bearer token> --from-prefix series --to-prefix fandom

Run with --dry-run first to preview what would change. Add
--delete-source-tags to remove the now-empty --from-prefix tag (and its
descendants) from the tag tree once every photo has been migrated -- this
never runs if any photo failed to migrate.

The token can also be supplied via the PHOSERV_API_TOKEN env var.
"""
from __future__ import annotations

import argparse
import os
import sys
import threading
from concurrent.futures import ThreadPoolExecutor, as_completed

import requests

DEFAULT_SERVER_URL = "http://localhost:4173"

print_lock = threading.Lock()


def log(msg: str) -> None:
    with print_lock:
        print(msg, flush=True)


class PhoservClient:
    def __init__(self, base_url: str, token: str):
        self.base_url = base_url.rstrip("/")
        self.session = requests.Session()
        self.session.headers["Authorization"] = f"Bearer {token}"

    def get_tag_tree(self) -> list[dict]:
        r = self.session.get(f"{self.base_url}/api/tags", timeout=30)
        r.raise_for_status()
        return r.json()

    def delete_tag(self, tag_id: int) -> None:
        r = self.session.delete(f"{self.base_url}/api/tags/{tag_id}", timeout=30)
        r.raise_for_status()

    def list_photos_by_tag(self, tag: str, cursor: str | None, limit: int = 200) -> dict:
        params = {"tag": tag, "limit": limit}
        if cursor:
            params["cursor"] = cursor
        r = self.session.get(f"{self.base_url}/api/photos", params=params, timeout=60)
        r.raise_for_status()
        return r.json()

    def add_tags(self, photo_id: str, tags: list[str]) -> None:
        r = self.session.post(
            f"{self.base_url}/api/photos/{photo_id}/tags",
            json={"tags": tags},
            timeout=30,
        )
        r.raise_for_status()

    def remove_tags(self, photo_id: str, tags: list[str]) -> None:
        r = self.session.delete(
            f"{self.base_url}/api/photos/{photo_id}/tags",
            json={"tags": tags},
            timeout=30,
        )
        r.raise_for_status()


def find_tag_node(nodes: list[dict], path: str) -> dict | None:
    for node in nodes:
        if node["path"] == path:
            return node
        found = find_tag_node(node["children"], path)
        if found is not None:
            return found
    return None


def map_tag(path: str, from_prefix: str, to_prefix: str) -> str | None:
    """Returns the --to-prefix equivalent of `path` if it falls under
    --from-prefix, else None."""
    if path == from_prefix:
        return to_prefix
    if path.startswith(from_prefix + "/"):
        return to_prefix + path[len(from_prefix):]
    return None


def fetch_all_photos(phoserv: PhoservClient, tag: str, limit: int | None) -> list[dict]:
    photos: list[dict] = []
    cursor = None
    while True:
        page = phoserv.list_photos_by_tag(tag, cursor, limit=200)
        photos.extend(page["photos"])
        if limit is not None and len(photos) >= limit:
            return photos[:limit]
        cursor = page.get("next_cursor")
        if not cursor:
            return photos


def process_one(phoserv: PhoservClient, photo: dict, from_prefix: str, to_prefix: str, dry_run: bool) -> str:
    """Returns one of: 'ok', 'skipped', 'error'."""
    tags = photo["tags"]
    matches = [(t, map_tag(t, from_prefix, to_prefix)) for t in tags]
    matches = [(t, m) for t, m in matches if m is not None]
    if not matches:
        return "skipped"

    to_remove = sorted({t for t, _ in matches})
    to_add = sorted({m for _, m in matches if m not in tags})

    if dry_run:
        log(f"[dry-run] {photo['id']}: add={to_add} remove={to_remove}")
        return "ok"

    try:
        if to_add:
            phoserv.add_tags(photo["id"], to_add)
        if to_remove:
            phoserv.remove_tags(photo["id"], to_remove)
        log(f"{photo['id']}: add={to_add} remove={to_remove}")
        return "ok"
    except requests.RequestException as e:
        log(f"FAILED {photo['id']}: {e}")
        return "error"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--server-url", default=DEFAULT_SERVER_URL)
    parser.add_argument("--server-token", default=os.environ.get("PHOSERV_API_TOKEN"))
    parser.add_argument("--from-prefix", required=True, help='source tag namespace, e.g. "series"')
    parser.add_argument("--to-prefix", required=True, help='destination tag namespace, e.g. "fandom"')
    parser.add_argument("--limit", type=int, default=None, help="only process the first N matching photos (for testing)")
    parser.add_argument("--workers", type=int, default=4)
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument(
        "--delete-source-tags",
        action="store_true",
        help="after migrating every photo, delete the (now empty) --from-prefix tag and its "
        "descendants from the tag tree; skipped if any photo failed to migrate or on --dry-run",
    )
    args = parser.parse_args()

    if not args.server_token:
        parser.error("--server-token or PHOSERV_API_TOKEN is required")

    from_prefix = args.from_prefix.strip("/")
    to_prefix = args.to_prefix.strip("/")
    if not from_prefix or not to_prefix:
        parser.error("--from-prefix and --to-prefix must not be empty")
    if from_prefix == to_prefix:
        parser.error("--from-prefix and --to-prefix must differ")
    if to_prefix == from_prefix or to_prefix.startswith(from_prefix + "/") or from_prefix.startswith(to_prefix + "/"):
        parser.error("--from-prefix and --to-prefix must not be nested inside one another")

    phoserv = PhoservClient(args.server_url, args.server_token)

    log("fetching tag tree...")
    tree = phoserv.get_tag_tree()
    source_node = find_tag_node(tree, from_prefix)
    if source_node is None:
        log(f"no tags found under prefix '{from_prefix}', nothing to do")
        return 0

    log(f"searching for photos tagged under '{from_prefix}' ...")
    photos = fetch_all_photos(phoserv, from_prefix, args.limit)
    log(f"found {len(photos)} photo(s) to migrate '{from_prefix}/*' -> '{to_prefix}/*'")

    counts = {"ok": 0, "skipped": 0, "error": 0}
    with ThreadPoolExecutor(max_workers=args.workers) as pool:
        futures = [
            pool.submit(process_one, phoserv, p, from_prefix, to_prefix, args.dry_run)
            for p in photos
        ]
        for fut in as_completed(futures):
            counts[fut.result()] += 1

    log(f"done. ok={counts['ok']} skipped={counts['skipped']} errors={counts['error']}")

    if args.delete_source_tags and not args.dry_run:
        if counts["error"]:
            log("skipping --delete-source-tags: some photos failed to migrate")
        else:
            log(f"deleting source tag '{from_prefix}' (and any descendants)...")
            tree = phoserv.get_tag_tree()
            node = find_tag_node(tree, from_prefix)
            if node is None:
                log(f"'{from_prefix}' no longer exists, nothing to delete")
            else:
                phoserv.delete_tag(node["id"])
                log(f"deleted tag '{from_prefix}' (id={node['id']})")

    return 1 if counts["error"] else 0


if __name__ == "__main__":
    sys.exit(main())
