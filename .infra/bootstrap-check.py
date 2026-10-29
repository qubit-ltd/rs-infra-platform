#!/usr/bin/env python3
"""Check managed bootstrap files against the last installed source snapshot."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import stat
import sys


def main() -> int:
    root = Path(__file__).resolve().parent.parent
    snapshot_path = root / ".infra/bootstrap-source.json"
    try:
        snapshot = json.loads(snapshot_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        print(f"infra bootstrap snapshot unavailable: {error}", file=sys.stderr)
        return 1
    drift: list[str] = []
    for target, expected in snapshot.get("files", {}).items():
        rel = PurePosixPath(target)
        if rel.is_absolute() or ".." in rel.parts or "\\" in target:
            print(f"invalid path in bootstrap snapshot: {target}", file=sys.stderr)
            return 2
        path = root.joinpath(*rel.parts)
        if path.is_symlink() or not path.is_file():
            drift.append(target)
            continue
        # Git for Windows may materialize managed text files with CRLF endings.
        # Compare canonical LF content, matching the hashes written by sync.py.
        content = path.read_bytes().replace(b"\r\n", b"\n")
        digest = hashlib.sha256(content).hexdigest()
        executable = bool(path.stat().st_mode & stat.S_IXUSR)
        mode_matches = os.name == "nt" or executable == expected.get("executable")
        if digest != expected.get("sha256") or not mode_matches:
            drift.append(target)
    if drift:
        print("managed bootstrap files differ from the installed upstream snapshot:")
        for path in drift:
            print(f"  {path}")
        print("run ./update-infra.sh to restore the upstream versions", file=sys.stderr)
        return 1
    print("managed bootstrap files match the installed upstream snapshot")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
