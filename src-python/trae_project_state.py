#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Preserve Trae global project/recently-opened state across account snapshots.

Only the two global project navigation keys are copied.  Account credentials,
session data and other state.vscdb keys are intentionally left untouched.
"""

from __future__ import annotations

import argparse
import copy
import json
import os
import shutil
import sqlite3
import sys
import time
from pathlib import Path
from typing import Any


KEY_PARTS = ("recentlyopenedpathslist", "local-project-folders", "localprojectfolders")


def is_project_key(key: str) -> bool:
    lower = key.lower()
    return any(part in lower for part in KEY_PARTS)


def open_db(path: Path, read_only: bool) -> sqlite3.Connection:
    if read_only:
        uri = f"file:{path.resolve().as_posix()}?mode=ro"
        return sqlite3.connect(uri, uri=True, timeout=8)
    return sqlite3.connect(str(path), timeout=12)


def capture(db_path: Path, output_path: Path) -> int:
    if not db_path.is_file():
        return 2
    try:
        with open_db(db_path, True) as conn:
            rows = conn.execute(
                "SELECT key, value FROM ItemTable "
                "WHERE lower(key) LIKE '%recentlyopenedpathslist%' "
                "OR lower(key) LIKE '%local-project-folders%' "
                "OR lower(key) LIKE '%localprojectfolders%'"
            ).fetchall()
    except (OSError, sqlite3.Error):
        return 3

    output_path.parent.mkdir(parents=True, exist_ok=True)
    payload = {
        "schema_version": 1,
        "captured_at": int(time.time()),
        "items": [
            {
                "key": key,
                "value": value.decode("utf-8", errors="replace") if isinstance(value, bytes) else str(value),
            }
            for key, value in rows
        ],
    }
    output_path.write_text(json.dumps(payload, ensure_ascii=False, separators=(",", ":")), encoding="utf-8")
    return 0


def decode_json(raw: str) -> tuple[Any, int] | tuple[None, int]:
    try:
        value: Any = json.loads(raw)
    except (TypeError, ValueError):
        return None, 0
    wrapped = 0
    # VS Code versions differ: some values are JSON objects, others are a JSON
    # string containing a serialized object. Preserve the wrapping on write.
    while isinstance(value, str) and value[:1] in "[{":
        try:
            value = json.loads(value)
            wrapped += 1
        except (TypeError, ValueError):
            break
    return value, wrapped


def encode_json(value: Any, wrapped: int) -> str:
    encoded = json.dumps(value, ensure_ascii=False, separators=(",", ":"))
    for _ in range(wrapped):
        encoded = json.dumps(encoded, ensure_ascii=False, separators=(",", ":"))
    return encoded


def item_identity(value: Any) -> str:
    if isinstance(value, dict):
        for key in ("folderUri", "fileUri", "workspaceUri", "path", "uri"):
            if value.get(key):
                return f"{key}:{value[key]}"
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def merge_arrays(target: list[Any], source: list[Any]) -> list[Any]:
    result = copy.deepcopy(target)
    seen = {item_identity(item) for item in result}
    for item in source:
        identity = item_identity(item)
        if identity not in seen:
            result.append(copy.deepcopy(item))
            seen.add(identity)
    return result


def merge_project_value(target: Any, source: Any, key: str) -> Any:
    """Merge navigation values with target snapshot taking precedence."""
    lower = key.lower()
    if isinstance(target, list) and isinstance(source, list):
        return merge_arrays(target, source)
    if isinstance(target, dict) and isinstance(source, dict):
        result = copy.deepcopy(target)
        # recentlyOpenedPathsList normally stores entries under one of these
        # arrays. Keep target order and append missing paths from the source.
        for array_key in ("entries", "workspaces", "files", "folders"):
            tv = result.get(array_key)
            sv = source.get(array_key)
            if isinstance(tv, list) and isinstance(sv, list):
                result[array_key] = merge_arrays(tv, sv)
        if "local-project-folders" in lower or "localprojectfolders" in lower:
            for field, value in source.items():
                if field not in result:
                    result[field] = copy.deepcopy(value)
        return result
    # If the target shape changed between versions, retain the snapshot value
    # instead of replacing account-specific state with an older shape.
    return copy.deepcopy(target)


def merge(db_path: Path, input_path: Path) -> int:
    if not db_path.is_file() or not input_path.is_file():
        return 2
    try:
        payload = json.loads(input_path.read_text(encoding="utf-8"))
        items = payload.get("items", [])
    except (OSError, ValueError, AttributeError):
        return 3
    if payload.get("schema_version") != 1:
        return 4

    backup = db_path.with_name(db_path.name + ".aiwork-pre-project-merge.bak")
    try:
        shutil.copy2(db_path, backup)
        with open_db(db_path, False) as conn:
            existing = {
                key: value
                for key, value in conn.execute(
                    "SELECT key, value FROM ItemTable WHERE lower(key) LIKE '%recentlyopenedpathslist%' "
                    "OR lower(key) LIKE '%local-project-folders%' "
                    "OR lower(key) LIKE '%localprojectfolders%'"
                ).fetchall()
            }
            changed = 0
            for item in items:
                key = str(item.get("key", ""))
                source_raw = item.get("value")
                if not key or not is_project_key(key) or not isinstance(source_raw, str):
                    continue
                target_raw = existing.get(key)
                if target_raw is None:
                    conn.execute("INSERT OR REPLACE INTO ItemTable(key, value) VALUES (?, ?)", (key, source_raw))
                    changed += 1
                    continue
                target, target_wrapped = decode_json(target_raw)
                source, source_wrapped = decode_json(source_raw)
                if target is None or source is None or target_wrapped != source_wrapped:
                    continue
                merged = merge_project_value(target, source, key)
                merged_raw = encode_json(merged, target_wrapped)
                if merged_raw != target_raw:
                    conn.execute("UPDATE ItemTable SET value = ? WHERE key = ?", (merged_raw, key))
                    changed += 1
            conn.commit()
    except (OSError, sqlite3.Error):
        return 5
    return 0


def main() -> int:
    parser = argparse.ArgumentParser()
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--capture", action="store_true")
    mode.add_argument("--merge", action="store_true")
    parser.add_argument("--db", required=True)
    parser.add_argument("--out")
    parser.add_argument("--in", dest="input_path")
    args = parser.parse_args()
    db_path = Path(args.db)
    if args.capture:
        if not args.out:
            parser.error("--capture requires --out")
        return capture(db_path, Path(args.out))
    if not args.input_path:
        parser.error("--merge requires --in")
    return merge(db_path, Path(args.input_path))


if __name__ == "__main__":
    sys.exit(main())
