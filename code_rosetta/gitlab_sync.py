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
                # not simple=true: that view omits empty_repo
                "include_subgroups": "true", "archived": "false",
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
    # GitLab still reports default_branch (e.g. "main") for a repo with no commits.
    if not branch or project.get("empty_repo"):
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


def write_config(config_path: Path, group_name: str, db_path: Path, repos: list[Path],
                 repos_dir: Path | None = None, gitlab_url: str | None = None) -> None:
    from ruamel.yaml import YAML

    config_path.parent.mkdir(parents=True, exist_ok=True)
    tmp = config_path.with_suffix(".tmp")
    yaml = YAML()
    yaml.default_flow_style = False
    group: dict = {"db": str(db_path), "repos": [str(r) for r in repos]}
    # Lets the server turn a hit into <project, path, GitLab link> without any extra environment.
    if repos_dir is not None:
        group["repos_dir"] = str(repos_dir)
    if gitlab_url:
        group["gitlab_url"] = gitlab_url.rstrip("/")
    with open(tmp, "w") as f:
        yaml.dump({"graphs": {group_name: group}}, f)
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


def has_indexable_files(repo: Path) -> bool:
    """True when the clone holds something the graph or content index would take."""
    from .content import _candidate_files
    from .incremental import collect_all_files

    return bool(collect_all_files(repo) or _candidate_files(repo))


def indexed_file_counts(store, repos: set[Path]) -> dict[Path, int]:
    """Indexed files per repo root: graph File nodes plus content rows, whichever is larger."""
    from .content import ensure_schema

    roots = {str(r): r for r in repos}
    graph: dict[Path, int] = {r: 0 for r in repos}
    content: dict[Path, int] = {r: 0 for r in repos}

    def owner(path: str) -> Path | None:
        p = Path(path)
        for parent in p.parents:
            if str(parent) in roots:
                return roots[str(parent)]
        return None

    for f in store.get_all_files():
        r = owner(f)
        if r is not None:
            graph[r] += 1
    if ensure_schema(store._conn):
        for (path,) in store._conn.execute("SELECT path FROM file_content_meta"):
            r = owner(path)
            if r is not None:
                content[r] += 1
    return {r: max(graph[r], content[r]) for r in repos}


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

        # A sync killed after the clone but before indexing leaves clones that every later run calls
        # "unchanged" and never indexes. Any in-scope clone with nothing indexed gets a full build.
        counts_now = indexed_file_counts(store, in_scope)
        built = {r["path"] for r in results if r["action"] in ("cloned", "updated")}
        reindexed = 0
        for repo in sorted(p for p, n in counts_now.items()
                           if n == 0 and p not in built and has_indexable_files(p)):
            logger.warning("%s: cloned but nothing indexed, indexing now", repo)
            full_build(repo, store)
            reindexed += 1

        # Repos that were archived, deleted or moved out of scope.
        if not limit:
            for root in Config(_CONFIG_FILE).get_group_repos(group_name):
                if root not in in_scope:
                    shutil.rmtree(root, ignore_errors=True)
                    purge_repo(store, root)
                    removed += 1

        if counts.get("cloned") or counts.get("updated") or removed or reindexed:
            store.resolve_cross_file_calls()
            if store._has_fts():
                store.rebuild_fts()
            store.commit()
        stats = store.get_stats()

        # Index health: every clone that has something indexable must have at least one indexed file.
        failed_paths = {r["path"] for r in results if r["action"] == "failed"}
        final_counts = indexed_file_counts(store, in_scope)
        expected = {p for p in in_scope if p not in failed_paths and has_indexable_files(p)}
        missing = sorted(p for p in expected if final_counts[p] == 0)
        for p in missing:
            logger.warning("%s: not indexed (cloned, has indexable files, index has none)", p.relative_to(repos_dir))

    write_config(_CONFIG_FILE, group_name, db_path, sorted(in_scope), repos_dir=repos_dir, gitlab_url=url)

    return {
        "projects": len(projects),
        **counts,
        "removed": removed,
        "reindexed": reindexed,
        "failed": counts.get("failed", 0),
        "indexed_repos": len(expected) - len(missing),
        "expected_repos": len(expected),
        "ok": len(in_scope) > 0 and not missing,
        "nodes": stats.total_nodes,
        "files": stats.files_count,
        "git_seconds": round(t_git, 1),
        "total_seconds": round(time.time() - t0, 1),
    }
