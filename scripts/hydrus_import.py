#!/usr/bin/env python3
"""Pull files and tags from a Hydrus client and upload them to phoserv.

Requires: pip install requests

Usage:
  python3 hydrus_import.py \\
      --hydrus-url http://localhost:45869 --hydrus-key <hex key> \\
      --server-url http://localhost:4173 --server-token <bearer token>

Both keys can also be supplied via the HYDRUS_API_KEY / PHOSERV_API_TOKEN
env vars. Run with --dry-run first to preview what would happen.
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

    def fetch_file(self, file_hash: str) -> bytes:
        r = self.session.get(
            f"{self.base_url}/get_files/file",
            params={"hash": file_hash},
            timeout=300,
        )
        r.raise_for_status()
        return r.content


class PhoservClient:
    def __init__(self, base_url: str, token: str):
        self.base_url = base_url.rstrip("/")
        self.session = requests.Session()
        self.session.headers["Authorization"] = f"Bearer {token}"

    def exists(self, file_hash: str) -> bool:
        """Checks by content hash whether phoserv already has this file,
        without downloading it from Hydrus or uploading it."""
        r = self.session.get(f"{self.base_url}/api/photos/by-hash/{file_hash}", timeout=30)
        if r.status_code == 404:
            return False
        r.raise_for_status()
        return True

    def upload(self, filename: str, content_type: str, data: bytes, tags: list[str]) -> dict:
        files = {"file": (filename, data, content_type)}
        form = [("tags", t) for t in tags]
        r = self.session.post(f"{self.base_url}/api/photos", files=files, data=form, timeout=300)
        r.raise_for_status()
        return r.json()


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


def load_state(path: str | None) -> set[str]:
    if not path or not os.path.exists(path):
        return set()
    with open(path) as f:
        return {line.strip() for line in f if line.strip()}


def append_state(path: str | None, file_hash: str) -> None:
    if not path:
        return
    with print_lock:
        with open(path, "a") as f:
            f.write(file_hash + "\n")


def process_one(
    hydrus: HydrusClient,
    phoserv: PhoservClient,
    metadata: dict,
    flatten_namespace: bool,
    include_pending: bool,
    dry_run: bool,
    state_file: str | None,
) -> tuple[bool, str]:
    file_hash = metadata["hash"]
    ext = metadata.get("ext") or ""
    mime = metadata.get("mime") or "application/octet-stream"
    tags = [to_tag_path(t, flatten_namespace) for t in current_tags(metadata, include_pending)]
    filename = f"{file_hash}{ext}"

    if dry_run:
        log(f"[dry-run] {filename} mime={mime} tags={tags}")
        return True, file_hash

    try:
        if phoserv.exists(file_hash):
            append_state(state_file, file_hash)
            log(f"skipped {filename} (already on server)")
            return True, file_hash
        data = hydrus.fetch_file(file_hash)
        phoserv.upload(filename, mime, data, tags)
        append_state(state_file, file_hash)
        log(f"uploaded {filename} ({len(tags)} tags)")
        return True, file_hash
    except requests.HTTPError as e:
        body = e.response.text[-1500:] if e.response is not None else ""
        log(f"FAILED {filename}: {e} {body}")
        return False, file_hash
    except requests.RequestException as e:
        log(f"FAILED {filename}: {e}")
        return False, file_hash


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
    parser.add_argument("--flatten-namespace", action="store_true", help="keep namespace:tag as one flat tag instead of namespace/tag hierarchy")
    parser.add_argument("--include-pending", action="store_true", help="also upload tags that are pending (not yet processed by PTR)")
    parser.add_argument("--limit", type=int, default=None, help="only process the first N files (for testing)")
    parser.add_argument("--workers", type=int, default=4)
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--state-file", default=None, help="path to track uploaded hashes for resumable runs")
    args = parser.parse_args()

    if not args.hydrus_key:
        parser.error("--hydrus-key or HYDRUS_API_KEY is required")
    if not args.dry_run and not args.server_token:
        parser.error("--server-token or PHOSERV_API_TOKEN is required (unless --dry-run)")

    search_tags = args.tags or ["system:everything"]

    hydrus = HydrusClient(args.hydrus_url, args.hydrus_key)
    phoserv = PhoservClient(args.server_url, args.server_token or "")

    log("verifying hydrus access key...")
    hydrus.verify()

    log(f"searching hydrus for tags={search_tags} ...")
    hashes = hydrus.search_hashes(search_tags)
    log(f"found {len(hashes)} files")

    already_done = load_state(args.state_file)
    if already_done:
        hashes = [h for h in hashes if h not in already_done]
        log(f"{len(already_done)} already recorded in state file, {len(hashes)} remaining")

    if args.limit is not None:
        hashes = hashes[: args.limit]

    metadatas: list[dict] = []
    for batch in chunked(hashes, HASH_BATCH_SIZE):
        metadatas.extend(hydrus.file_metadata(batch))

    ok_count = 0
    fail_count = 0
    with ThreadPoolExecutor(max_workers=args.workers) as pool:
        futures = [
            pool.submit(
                process_one,
                hydrus,
                phoserv,
                m,
                args.flatten_namespace,
                args.include_pending,
                args.dry_run,
                args.state_file,
            )
            for m in metadatas
        ]
        for fut in as_completed(futures):
            ok, _ = fut.result()
            if ok:
                ok_count += 1
            else:
                fail_count += 1

    log(f"done. ok={ok_count} failed={fail_count}")
    return 1 if fail_count else 0


if __name__ == "__main__":
    sys.exit(main())
