"""Full-text and regex search over every tracked text file (not just parsed languages)."""

from __future__ import annotations

import fnmatch
import hashlib
import logging
import os
import re
from pathlib import Path
from typing import Any

logger = logging.getLogger(__name__)

MAX_FILE_BYTES = int(os.environ.get("CRG_CONTENT_MAX_BYTES", str(1_000_000)))
MAX_LINE_CHARS = 300

_SCHEMA = (
    'CREATE VIRTUAL TABLE IF NOT EXISTS file_content USING fts5(path UNINDEXED, body, tokenize="trigram")',
    "CREATE TABLE IF NOT EXISTS file_content_meta (path TEXT PRIMARY KEY, hash TEXT NOT NULL, fts_rowid INTEGER NOT NULL)",
)


def ensure_schema(conn) -> bool:
    try:
        for sql in _SCHEMA:
            conn.execute(sql)
        conn.commit()
        return True
    except Exception as e:  # trigram tokenizer needs SQLite >= 3.34
        logger.warning("Content index unavailable: %s", e)
        return False


def _candidate_files(repo_root: Path) -> list[str]:
    from .incremental import _is_binary, _load_ignore_patterns, _should_ignore, get_all_tracked_files

    patterns = _load_ignore_patterns(repo_root)
    rels = get_all_tracked_files(repo_root) or [
        str(p.relative_to(repo_root)) for p in repo_root.rglob("*") if p.is_file()
    ]
    out = []
    for rel in rels:
        if _should_ignore(rel, patterns):
            continue
        p = repo_root / rel
        try:
            if not p.is_file() or p.is_symlink() or p.stat().st_size > MAX_FILE_BYTES:
                continue
        except OSError:
            continue
        if _is_binary(p):
            continue
        out.append(rel)
    return out


def sync_repo(store, repo_root: Path) -> dict[str, int]:
    """Bring the content index for one repo in line with its tracked files."""
    conn = store._conn
    if not ensure_schema(conn):
        return {"indexed": 0, "unchanged": 0, "removed": 0}

    prefix = str(repo_root).rstrip(os.sep) + os.sep
    existing = {
        r[0]: (r[1], r[2])
        for r in conn.execute(
            "SELECT path, hash, fts_rowid FROM file_content_meta WHERE substr(path, 1, ?) = ?",
            (len(prefix), prefix),
        )
    }
    indexed = unchanged = 0
    seen = set()
    for rel in _candidate_files(repo_root):
        path = str(repo_root / rel)
        seen.add(path)
        try:
            data = (repo_root / rel).read_bytes()
        except OSError:
            continue
        h = hashlib.sha256(data).hexdigest()
        old = existing.get(path)
        if old and old[0] == h:
            unchanged += 1
            continue
        if old:
            conn.execute("DELETE FROM file_content WHERE rowid = ?", (old[1],))
        cur = conn.execute(
            "INSERT INTO file_content(path, body) VALUES (?, ?)",
            (path, data.decode("utf-8", "replace")),
        )
        conn.execute(
            "INSERT OR REPLACE INTO file_content_meta(path, hash, fts_rowid) VALUES (?, ?, ?)",
            (path, h, cur.lastrowid),
        )
        indexed += 1

    removed = 0
    for path, (_, rowid) in existing.items():
        if path not in seen:
            conn.execute("DELETE FROM file_content WHERE rowid = ?", (rowid,))
            conn.execute("DELETE FROM file_content_meta WHERE path = ?", (path,))
            removed += 1
    conn.commit()
    return {"indexed": indexed, "unchanged": unchanged, "removed": removed}


def _required_literal(pattern: str) -> str | None:
    """Longest literal run every match must contain, for the trigram pre-filter."""
    if "|" in pattern:
        return None
    runs, cur, depth, i = [], [], 0, 0
    while i < len(pattern):
        ch = pattern[i]
        nxt = pattern[i + 1] if i + 1 < len(pattern) else ""
        if ch == "\\" and nxt:
            if nxt.isalnum():
                runs.append("".join(cur)); cur = []
            elif depth == 0:
                cur.append(nxt)
            i += 2
            continue
        if ch == "(":
            depth += 1; runs.append("".join(cur)); cur = []
        elif ch == ")":
            depth = max(0, depth - 1)
        elif ch == "[":
            runs.append("".join(cur)); cur = []
            j = pattern.find("]", i + 2)
            i = j if j != -1 else len(pattern)
        elif ch in "?*":
            if cur:
                cur.pop()
            runs.append("".join(cur)); cur = []
        elif ch == "{":
            if cur and pattern[i + 1:i + 2] == "0":
                cur.pop()
            runs.append("".join(cur)); cur = []
            j = pattern.find("}", i)
            i = j if j != -1 else len(pattern)
        elif ch in ".^$+":
            runs.append("".join(cur)); cur = []
        elif depth == 0:
            cur.append(ch)
        i += 1
    runs.append("".join(cur))
    best = max(runs, key=len)
    return best if len(best) >= 3 else None


def grep(
    store,
    pattern: str,
    regex: bool = False,
    ignore_case: bool = False,
    path_glob: str | None = None,
    limit: int = 100,
    files_only: bool = False,
) -> dict[str, Any]:
    conn = store._conn
    if not ensure_schema(conn):
        return {"status": "error", "error": "content index unavailable (SQLite trigram tokenizer missing)"}
    if not conn.execute("SELECT 1 FROM file_content_meta LIMIT 1").fetchone():
        return {"status": "error", "error": "content index is empty; run a build first"}

    flags = re.IGNORECASE if ignore_case else 0
    try:
        rx = re.compile(pattern if regex else re.escape(pattern), flags)
    except re.error as e:
        return {"status": "error", "error": f"invalid regex: {e}"}

    needle = _required_literal(pattern) if regex else (pattern if len(pattern) >= 3 else None)
    if needle:
        rows = conn.execute(
            "SELECT path, body FROM file_content WHERE file_content MATCH ?",
            ('"' + needle.replace('"', '""') + '"',),
        )
    else:
        rows = conn.execute("SELECT path, body FROM file_content")

    matches: list[dict[str, Any]] = []
    files: list[str] = []
    truncated = False
    for path, body in rows:
        if path_glob and not fnmatch.fnmatch(path, path_glob):
            continue
        if files_only:
            if rx.search(body):
                files.append(path)
                if len(files) >= limit:
                    truncated = True
                    break
            continue
        hit = False
        for n, line in enumerate(body.splitlines(), 1):
            if rx.search(line):
                hit = True
                matches.append({"path": path, "line": n, "text": line[:MAX_LINE_CHARS]})
                if len(matches) >= limit:
                    truncated = True
                    break
        if hit:
            files.append(path)
        if truncated:
            break

    result: dict[str, Any] = {
        "status": "ok",
        "pattern": pattern,
        "regex": regex,
        "summary": (
            f"{len(files)} file(s)" if files_only else f"{len(matches)} match(es) in {len(files)} file(s)"
        ) + (" (truncated)" if truncated else ""),
        "files": files,
        "truncated": truncated,
    }
    if not files_only:
        result["matches"] = matches
    return result
