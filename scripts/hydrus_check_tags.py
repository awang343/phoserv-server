#!/usr/bin/env python3
"""Compare tags between a Hydrus client and phoserv without uploading anything.

Read-only reconciliation check: for each file matched in Hydrus, looks up the
same file on phoserv by content hash and diffs the tag sets. Useful after a
hydrus_import.py run that had partial failures (e.g. a tag-creation race),
since a failed upload can leave a photo on the server with only some of its
tags attached, and a plain rerun of hydrus_import.py won't notice because the
photo already exists.

Requires: pip install requests

Usage:
  python3 hydrus_check_tags.py \\
      --hydrus-url http://localhost:45869 --hydrus-key <hex key> \\
      --server-url http://localhost:4173 --server-token <bearer token>

Add --fix to push any missing tags to phoserv via POST /api/photos/{id}/tags
(this does not touch Hydrus and never uploads files -- it only adds tags to
photos that already exist on the server).

Add --interactive-sync to be prompted on each mismatch, with the option to
force phoserv's tags to match Hydrus exactly for that photo -- adding
missing tags (POST) and removing extra ones (DELETE). Mutually exclusive
with --fix; processes files sequentially so prompts don't interleave.

Both keys can also be supplied via the HYDRUS_API_KEY / PHOSERV_API_TOKEN
env vars.
"""
from __future__ import annotations

import argparse
import json
import os
import sys
import threading
from concurrent.futures import ThreadPoolExecutor, as_completed

import requests

DEFAULT_HYDRUS_URL = "http://localhost:45869"
DEFAULT_SERVER_URL = "http://localhost:4173"
HASH_BATCH_SIZE = 64

print_lock = threading.Lock()


def log(msg: str) -> None:
    with print_lock:
        print(msg, flush=True)


class HydrusClient:
    def __init__(self, base_url: str, api_key: str):
        self.base_url = base_url.rstrip("/")
        self.session = requests.Session()
        self.session.headers["Hydrus-Client-API-Access-Key"] = api_key

    def verify(self) -> None:
        r = self.session.get(f"{self.base_url}/verify_access_key", timeout=30)
        r.raise_for_status()

    def search_hashes(self, tags: list[str]) -> list[str]:
        r = self.session.get(
            f"{self.base_url}/get_files/search_files",
            params={
                "tags": json.dumps(tags),
                "return_hashes": "true",
                "return_file_ids": "false",
            },
            timeout=120,
        )
        r.raise_for_status()
        return r.json()["hashes"]

    def file_metadata(self, hashes: list[str]) -> list[dict]:
        r = self.session.get(
            f"{self.base_url}/get_files/file_metadata",
            params={"hashes": json.dumps(hashes)},
            timeout=120,
        )
        r.raise_for_status()
        return r.json()["metadata"]


class PhoservClient:
    def __init__(self, base_url: str, token: str):
        self.base_url = base_url.rstrip("/")
        self.session = requests.Session()
        self.session.headers["Authorization"] = f"Bearer {token}"

    def get_by_hash(self, file_hash: str) -> dict | None:
        r = self.session.get(f"{self.base_url}/api/photos/by-hash/{file_hash}", timeout=30)
        if r.status_code == 404:
            return None
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


def current_tags(metadata: dict, include_pending: bool) -> list[str]:
    statuses = {"0"} | ({"1"} if include_pending else set())
    tags: set[str] = set()
    for service in metadata.get("tags", {}).values():
        storage = service.get("storage_tags", {})
        for status in statuses:
            tags.update(storage.get(status, []))
    return sorted(tags)


def to_tag_path(hydrus_tag: str, flatten_namespace: bool) -> str:
    if flatten_namespace or ":" not in hydrus_tag:
        return hydrus_tag
    namespace, subtag = hydrus_tag.split(":", 1)
    namespace, subtag = namespace.strip(), subtag.strip()
    if not namespace or not subtag:
        return hydrus_tag
    return f"{namespace}/{subtag}"


def chunked(items: list, size: int):
    for i in range(0, len(items), size):
        yield items[i : i + size]


class SyncState:
    """Shared across --interactive-sync prompts so 'all'/'quit' persist between files."""

    def __init__(self):
        self.mode = "ask"  # "ask" | "all" | "quit"


def prompt_sync(sync_state: SyncState, file_hash: str, photo_id: str, missing: set[str], extra: set[str]) -> bool:
    if sync_state.mode == "quit":
        return False
    if sync_state.mode == "all":
        return True

    parts = []
    if missing:
        parts.append(f"add={sorted(missing)}")
    if extra:
        parts.append(f"remove={sorted(extra)}")
    prompt = f"sync {file_hash} (id={photo_id}) {' '.join(parts)}? [y/N/a=all/q=quit]: "

    while True:
        try:
            ans = input(prompt).strip().lower()
        except EOFError:
            sync_state.mode = "quit"
            return False
        if ans in ("y", "yes"):
            return True
        if ans in ("", "n", "no"):
            return False
        if ans in ("a", "all"):
            sync_state.mode = "all"
            return True
        if ans in ("q", "quit"):
            sync_state.mode = "quit"
            return False
        print("please answer y, n, a, or q", file=sys.stderr)


def check_one(
    phoserv: PhoservClient,
    metadata: dict,
    flatten_namespace: bool,
    include_pending: bool,
    fix: bool,
    sync_state: SyncState | None,
) -> str:
    """Returns one of: 'ok', 'missing', 'not_uploaded', 'synced', 'error'."""
    file_hash = metadata["hash"]
    expected = set(to_tag_path(t, flatten_namespace) for t in current_tags(metadata, include_pending))

    try:
        photo = phoserv.get_by_hash(file_hash)
    except requests.RequestException as e:
        log(f"ERROR {file_hash}: could not look up on server: {e}")
        return "error"

    if photo is None:
        log(f"NOT UPLOADED {file_hash} (expected {len(expected)} tags)")
        return "not_uploaded"

    actual = set(photo["tags"])
    missing = expected - actual
    extra = actual - expected

    if not missing and not extra:
        return "ok"

    parts = []
    if missing:
        parts.append(f"missing={sorted(missing)}")
    if extra:
        parts.append(f"extra={sorted(extra)}")
    log(f"MISMATCH {file_hash} (id={photo['id']}): {' '.join(parts)}")

    if sync_state is not None:
        if not prompt_sync(sync_state, file_hash, photo["id"], missing, extra):
            return "missing"
        try:
            if missing:
                phoserv.add_tags(photo["id"], sorted(missing))
            if extra:
                phoserv.remove_tags(photo["id"], sorted(extra))
            log(f"  synced: added {len(missing)}, removed {len(extra)} tag(s) on {photo['id']}")
            return "synced"
        except requests.RequestException as e:
            log(f"  FAILED to sync {photo['id']}: {e}")
            return "error"

    if fix and missing:
        try:
            phoserv.add_tags(photo["id"], sorted(missing))
            log(f"  fixed: added {len(missing)} missing tag(s) to {photo['id']}")
        except requests.RequestException as e:
            log(f"  FAILED to fix {photo['id']}: {e}")
            return "error"

    return "missing"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--hydrus-url", default=DEFAULT_HYDRUS_URL)
    parser.add_argument("--hydrus-key", default=os.environ.get("HYDRUS_API_KEY"))
    parser.add_argument("--server-url", default=DEFAULT_SERVER_URL)
    parser.add_argument("--server-token", default=os.environ.get("PHOSERV_API_TOKEN"))
    parser.add_argument(
        "--tag",
        dest="tags",
        action="append",
        default=None,
        help="Hydrus search predicate, repeatable (default: system:everything)",
    )
    parser.add_argument("--flatten-namespace", action="store_true", help="must match the flag used during import")
    parser.add_argument("--include-pending", action="store_true", help="must match the flag used during import")
    parser.add_argument("--limit", type=int, default=None, help="only check the first N files (for testing)")
    parser.add_argument("--workers", type=int, default=4)
    parser.add_argument("--fix", action="store_true", help="push missing tags to phoserv for photos that already exist there")
    parser.add_argument(
        "--interactive-sync",
        action="store_true",
        help=(
            "for each mismatch, prompt [y/N/a=all/q=quit] to force phoserv's tags to match "
            "Hydrus exactly (adds missing tags and removes extra ones). Mutually exclusive "
            "with --fix; runs sequentially so prompts don't interleave (--workers is ignored)"
        ),
    )
    args = parser.parse_args()

    if not args.hydrus_key:
        parser.error("--hydrus-key or HYDRUS_API_KEY is required")
    if not args.server_token:
        parser.error("--server-token or PHOSERV_API_TOKEN is required")
    if args.fix and args.interactive_sync:
        parser.error("--fix and --interactive-sync are mutually exclusive")

    search_tags = args.tags or ["system:everything"]

    hydrus = HydrusClient(args.hydrus_url, args.hydrus_key)
    phoserv = PhoservClient(args.server_url, args.server_token)

    log("verifying hydrus access key...")
    hydrus.verify()

    log(f"searching hydrus for tags={search_tags} ...")
    hashes = hydrus.search_hashes(search_tags)
    log(f"found {len(hashes)} files")

    if args.limit is not None:
        hashes = hashes[: args.limit]

    metadatas: list[dict] = []
    for batch in chunked(hashes, HASH_BATCH_SIZE):
        metadatas.extend(hydrus.file_metadata(batch))

    counts = {"ok": 0, "missing": 0, "not_uploaded": 0, "synced": 0, "error": 0}

    if args.interactive_sync:
        sync_state = SyncState()
        for i, m in enumerate(metadatas):
            result = check_one(phoserv, m, args.flatten_namespace, args.include_pending, args.fix, sync_state)
            counts[result] += 1
            if sync_state.mode == "quit":
                remaining = len(metadatas) - i - 1
                if remaining:
                    log(f"quit: skipping {remaining} remaining file(s)")
                break
    else:
        with ThreadPoolExecutor(max_workers=args.workers) as pool:
            futures = [
                pool.submit(check_one, phoserv, m, args.flatten_namespace, args.include_pending, args.fix, None)
                for m in metadatas
            ]
            for fut in as_completed(futures):
                counts[fut.result()] += 1

    log(
        f"done. ok={counts['ok']} missing_tags={counts['missing']} "
        f"not_uploaded={counts['not_uploaded']} synced={counts['synced']} errors={counts['error']}"
    )
    return 1 if (counts["missing"] or counts["not_uploaded"] or counts["error"]) else 0


if __name__ == "__main__":
    sys.exit(main())
