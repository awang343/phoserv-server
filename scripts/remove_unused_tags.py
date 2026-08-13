#!/usr/bin/env python3
"""Prunes tags from phoserv's tag tree that aren't attached to any photo.

A tag counts as "unused" only if *no photo* is tagged with it or with any of
its descendants -- e.g. "animals/cats" only gets removed once nothing is
tagged "animals/cats" and nothing is tagged "animals/cats/<anything below
it>" either. Deleting a tag (DELETE /api/tags/{id}) cascades to its
descendants server-side, so this only issues one delete per topmost unused
subtree rather than one per tag.

Usage counts come from GET /api/photos?q=<path>, which (per phoserv's search
semantics) already matches a tag path or any of its descendants -- so a
single request per tag tells us whether its whole subtree is empty. Note
this only reflects non-trashed photos (the default listing view); a tag used
only by photos currently in the trash is still treated as unused, since
there's no way to query trashed photos by tag through the current API.

Requires: pip install requests

Usage:
  python3 remove_unused_tags.py --server-url http://localhost:4173 \\
      --server-token <bearer token>

Run with --dry-run first to see what would be deleted without changing
anything. The token can also be supplied via the PHOSERV_API_TOKEN env var.
"""
from __future__ import annotations

import argparse
import os
import sys
from concurrent.futures import ThreadPoolExecutor, as_completed

import requests

DEFAULT_SERVER_URL = "http://localhost:4173"


def log(msg: str) -> None:
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

    def count_photos(self, tag_path: str) -> int:
        r = self.session.get(
            f"{self.base_url}/api/photos",
            params={"q": tag_path, "limit": 1},
            timeout=30,
        )
        r.raise_for_status()
        return r.json()["total"]

    def delete_tag(self, tag_id: int) -> None:
        r = self.session.delete(f"{self.base_url}/api/tags/{tag_id}", timeout=30)
        r.raise_for_status()


def find_unused_subtrees(phoserv: PhoservClient, tree: list[dict], workers: int) -> list[dict]:
    """Returns the topmost tag nodes whose entire subtree (the tag itself
    plus every descendant) has zero matching photos. Descendants of a node
    already in the result aren't checked or returned separately, since
    deleting their ancestor removes them too."""
    unused: list[dict] = []
    frontier = list(tree)

    while frontier:
        with ThreadPoolExecutor(max_workers=workers) as pool:
            futures = {pool.submit(phoserv.count_photos, node["path"]): node for node in frontier}
            counts = {futures[fut]["path"]: fut.result() for fut in as_completed(futures)}

        next_frontier = []
        for node in frontier:
            if counts[node["path"]] == 0:
                unused.append(node)
            else:
                next_frontier.extend(node["children"])
        frontier = next_frontier

    return unused


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--server-url", default=DEFAULT_SERVER_URL)
    parser.add_argument("--server-token", default=os.environ.get("PHOSERV_API_TOKEN"))
    parser.add_argument("--workers", type=int, default=8)
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()

    if not args.server_token:
        parser.error("--server-token or PHOSERV_API_TOKEN is required")

    phoserv = PhoservClient(args.server_url, args.server_token)

    log("fetching tag tree...")
    tree = phoserv.get_tag_tree()
    if not tree:
        log("tag tree is empty, nothing to do")
        return 0

    log("checking tag usage...")
    unused = find_unused_subtrees(phoserv, tree, args.workers)
    if not unused:
        log("no unused tags found")
        return 0

    unused.sort(key=lambda n: n["path"])
    log(f"found {len(unused)} unused tag(s):")
    for node in unused:
        log(f"  {node['path']} (id={node['id']})")

    if args.dry_run:
        log("[dry-run] no changes made")
        return 0

    deleted = 0
    failed = 0
    for node in unused:
        try:
            phoserv.delete_tag(node["id"])
            log(f"deleted {node['path']}")
            deleted += 1
        except requests.RequestException as e:
            log(f"FAILED to delete {node['path']}: {e}")
            failed += 1

    log(f"done. deleted={deleted} failed={failed}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
