"""Turn an on-disk hit into "which project, which file, which commit, where on GitLab".

Mirrored clones live at <repos_dir>/<path_with_namespace>, so a hit's project is its repo root's
path relative to repos_dir. With nested groups the on-disk path alone cannot tell where the repo
ends and the file begins; the configured repo roots can.
"""

from __future__ import annotations

import subprocess
import urllib.parse
from pathlib import Path
from typing import Any

# Commit sha per repo root. Valid until the config file changes: the sync rewrites it every run,
# so "per repo per sync" without a subprocess per query.
_REF_CACHE: dict[Path, str | None] = {}
_REF_STAMP: float | None = None


def _git_head(root: Path) -> str | None:
    try:
        r = subprocess.run(["git", "rev-parse", "HEAD"], cwd=str(root), capture_output=True,
                           text=True, timeout=10)
    except (OSError, subprocess.SubprocessError):
        return None
    return r.stdout.strip() or None if r.returncode == 0 else None


_UNSET = object()


def _ref_for(root: Path, stamp: float | None) -> str | None:
    """Tools run in a threadpool and another thread may clear the cache on a config change, so work with
    a local value and never read the shared dict back after writing it."""
    global _REF_STAMP
    if stamp != _REF_STAMP:
        _REF_CACHE.clear()
        _REF_STAMP = stamp
    ref = _REF_CACHE.get(root, _UNSET)
    if ref is _UNSET:
        ref = _git_head(root)
        _REF_CACHE[root] = ref
    return ref  # type: ignore[return-value]


class HitResolver:
    def __init__(self, roots: list[Path], repos_dir: Path | None = None,
                 gitlab_url: str | None = None, stamp: float | None = None):
        self._roots = {str(r): r for r in roots}
        self._repos_dir = repos_dir
        self._url = gitlab_url.rstrip("/") if gitlab_url else None
        self._stamp = stamp

    def _owner(self, path: str) -> Path | None:
        for parent in Path(path).parents:  # nearest first, so the longest matching root wins
            hit = self._roots.get(str(parent))
            if hit is not None:
                return hit
        return None

    def _repo_name(self, root: Path) -> str:
        if self._repos_dir is not None:
            try:
                return root.relative_to(self._repos_dir).as_posix()
            except ValueError:
                pass
        return root.name

    def qualified_name(self, abs_path: str) -> str | None:
        """"<repo>/<path in repo>" for a file under a known root: the form a hit reports as repo + path."""
        root = self._owner(abs_path)
        if root is None:
            return None
        return f"{self._repo_name(root)}/{Path(abs_path).relative_to(root).as_posix()}"

    def hit(self, abs_path: str, line: int | None = None) -> dict[str, Any]:
        root = self._owner(abs_path)
        if root is None:
            return {"repo": None, "path": abs_path, "ref": None, "web_url": None, "abs_path": abs_path}
        rel = Path(abs_path).relative_to(root).as_posix()
        repo = self._repo_name(root)
        ref = _ref_for(root, self._stamp)
        url = None
        if self._url and self._repos_dir is not None and ref:
            url = f"{self._url}/{repo}/-/blob/{ref}/{urllib.parse.quote(rel, safe='/')}"
            if line:
                url += f"#L{line}"
        return {"repo": repo, "path": rel, "ref": ref, "web_url": url, "abs_path": abs_path}
