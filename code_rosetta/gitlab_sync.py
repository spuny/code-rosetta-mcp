"""Mirror GitLab groups' default branches into a repos dir and keep one group graph in sync."""

from __future__ import annotations

import base64
import json
import logging
import os
import shutil
import subprocess
import time
import urllib.parse
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

logger = logging.getLogger(__name__)

_GIT_TIMEOUT = 600


def list_projects(url: str, token: str, groups: list[str]) -> list[dict]:
    """Non-archived projects under the groups (incl. subgroups), deduplicated by id."""
    seen: dict[int, dict] = {}
    for group in groups:
        page = 1
        while True:
            q = urllib.parse.urlencode({
                "include_subgroups": "true", "archived": "false", "simple": "true",
                "per_page": 100, "page": page,
            })
            req = urllib.request.Request(
                f"{url.rstrip('/')}/api/v4/groups/{urllib.parse.quote(group, safe='')}/projects?{q}",
                headers={"PRIVATE-TOKEN": token},
            )
            with urllib.request.urlopen(req, timeout=60) as resp:
                batch = json.load(resp)
            if not batch:
                break
            for p in batch:
                seen.setdefault(p["id"], p)
            page += 1
    return sorted(seen.values(), key=lambda p: p["path_with_namespace"])


def _git_env(token: str) -> dict:
    # Auth via env-only config: the token never lands in .git/config on the volume.
    basic = base64.b64encode(f"oauth2:{token}".encode()).decode()
    env = dict(os.environ)
    env.update({
        "GIT_TERMINAL_PROMPT": "0",
        "GIT_CONFIG_COUNT": "1",
        "GIT_CONFIG_KEY_0": "http.extraHeader",
        "GIT_CONFIG_VALUE_0": f"Authorization: Basic {basic}",
    })
    return env


def _git(args: list[str], cwd: Path | None, env: dict) -> str:
    r = subprocess.run(
        ["git", *args], cwd=str(cwd) if cwd else None, env=env,
        capture_output=True, text=True, timeout=_GIT_TIMEOUT,
    )
    if r.returncode != 0:
        raise RuntimeError(f"git {args[0]} failed: {r.stderr.strip()[-300:]}")
    return r.stdout.strip()


def mirror_project(project: dict, repos_dir: Path, env: dict) -> dict:
    """Clone or fast-forward one project's default branch (depth 1).

    Returns {"path", "action": cloned|updated|unchanged|skipped|failed, "changed": [...]}.
    """
    branch = project.get("default_branch")
    dest = repos_dir / project["path_with_namespace"]
    out = {"path": dest, "action": "skipped", "changed": []}
    if not branch:  # empty repository
        return out
    try:
        if not (dest / ".git").is_dir():
            if dest.exists():
                shutil.rmtree(dest)
            dest.parent.mkdir(parents=True, exist_ok=True)
            _git(["clone", "--quiet", "--depth", "1", "--single-branch", "--branch", branch,
                  project["http_url_to_repo"], str(dest)], None, env)
            out["action"] = "cloned"
            return out
        old = _git(["rev-parse", "HEAD"], dest, env)
        _git(["remote", "set-url", "origin", project["http_url_to_repo"]], dest, env)
        _git(["fetch", "--quiet", "--depth", "1", "origin", branch], dest, env)
        new = _git(["rev-parse", "FETCH_HEAD"], dest, env)
        if new == old:
            out["action"] = "unchanged"
            return out
        changed = _git(["diff", "--name-only", old, new], dest, env).splitlines()
        _git(["reset", "--quiet", "--hard", new], dest, env)
        out.update(action="updated", changed=[c for c in changed if c])
    except Exception as e:  # noqa: BLE001 -- one bad repo must not stop the sync
        out.update(action="failed", error=str(e))
    return out


def write_config(config_path: Path, group_name: str, db_path: Path, repos: list[Path]) -> None:
    from ruamel.yaml import YAML

    config_path.parent.mkdir(parents=True, exist_ok=True)
    tmp = config_path.with_suffix(".tmp")
    yaml = YAML()
    yaml.default_flow_style = False
    with open(tmp, "w") as f:
        yaml.dump({"graphs": {group_name: {"db": str(db_path), "repos": [str(r) for r in repos]}}}, f)
    tmp.replace(config_path)


def purge_repo(store, repo_root: Path) -> None:
    """Drop graph and content data for a repo that left the scope (dir already deleted)."""
    from .content import sync_repo as sync_content

    prefix = str(repo_root).rstrip(os.sep) + os.sep
    for f in store.get_all_files():
        if f.startswith(prefix):
            store.remove_file_data(f)
    sync_content(store, repo_root)  # repo dir is gone, so every content row is removed
    store.commit()


def sync(
    url: str, token: str, groups: list[str], repos_dir: Path, db_path: Path,
    group_name: str = "default", workers: int = 8, limit: int = 0,
) -> dict:
    from .config import _CONFIG_FILE, Config
    from .graph import GraphStore
    from .incremental import full_build, incremental_update

    t0 = time.time()
    repos_dir = repos_dir.resolve()
    projects = list_projects(url, token, groups)
    if limit:
        projects = projects[:limit]
    logger.info("%d projects in scope", len(projects))

    env = _git_env(token)
    with ThreadPoolExecutor(max_workers=workers) as pool:
        results = list(pool.map(lambda p: mirror_project(p, repos_dir, env), projects))
    t_git = time.time() - t0

    counts: dict[str, int] = {}
    for r in results:
        counts[r["action"]] = counts.get(r["action"], 0) + 1
        if r["action"] == "failed":
            logger.warning("%s: %s", r["path"], r.get("error"))

    in_scope = {r["path"] for r in results if r["action"] != "skipped" and (r["path"] / ".git").is_dir()}

    db_path.parent.mkdir(parents=True, exist_ok=True)
    removed = 0
    with GraphStore(db_path) as store:
        for r in results:
            if r["action"] == "cloned":
                full_build(r["path"], store)
            elif r["action"] == "updated":
                incremental_update(r["path"], store, changed_files=r["changed"])

        # Repos that were archived, deleted or moved out of scope.
        if not limit:
            for root in Config(_CONFIG_FILE).get_group_repos(group_name):
                if root not in in_scope:
                    shutil.rmtree(root, ignore_errors=True)
                    purge_repo(store, root)
                    removed += 1

        if counts.get("cloned") or counts.get("updated") or removed:
            store.resolve_cross_file_calls()
            if store._has_fts():
                store.rebuild_fts()
            store.commit()
        stats = store.get_stats()

    write_config(_CONFIG_FILE, group_name, db_path, sorted(in_scope))

    return {
        "projects": len(projects),
        **counts,
        "removed": removed,
        "failed": counts.get("failed", 0),
        "ok": len(in_scope) > 0,
        "nodes": stats.total_nodes,
        "files": stats.files_count,
        "git_seconds": round(t_git, 1),
        "total_seconds": round(time.time() - t0, 1),
    }
