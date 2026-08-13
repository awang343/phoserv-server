#!/usr/bin/env python3
"""Example phoserv downloader script.

This is the contract every script in the `downloaders_path` directory
(configured in config.toml) must follow to be run from the Upload tab's
downloader panel:

  * Invoked as `<script> <url>` (argv[1] is the URL the user typed in).
  * `PHOSERV_STAGING_DIR` is set (and is also the script's cwd) to a fresh,
    empty directory it should download files into. The directory and
    anything left in it are deleted once the job finishes.
  * For each file it wants imported, the script writes it into the staging
    directory and prints one JSON line to stdout:
        {"file": "<path relative to the staging dir>", "tags": ["a", "b/c"]}
    The server reads stdout line by line as the script runs; any line that
    isn't valid JSON in this shape is just recorded as a log line (so the
    script is free to print ordinary progress messages too).
  * Non-JSON stdout/stderr output is shown to the user as job log lines.
  * Exit 0 on success. A non-zero exit is reported as a failed job, but any
    files already reported via manifest lines are still imported.

Requires: pip install requests
"""
from __future__ import annotations

import json
import os
import sys
from urllib.parse import urlparse

import requests


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: example.py <url>", file=sys.stderr)
        return 1

    url = sys.argv[1]
    staging_dir = os.environ.get("PHOSERV_STAGING_DIR", ".")

    filename = os.path.basename(urlparse(url).path) or "download"
    dest = os.path.join(staging_dir, filename)

    print(f"downloading {url}")
    response = requests.get(url, timeout=30)
    response.raise_for_status()
    with open(dest, "wb") as f:
        f.write(response.content)

    manifest_line = {"file": filename, "tags": ["downloaded/example"]}
    print(json.dumps(manifest_line))
    return 0


if __name__ == "__main__":
    sys.exit(main())
