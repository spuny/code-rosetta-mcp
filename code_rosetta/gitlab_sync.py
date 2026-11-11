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


def _parseable_files(repo: Path) -> list[str]:
    """Files the graph index would take."""
    from .incremental import collect_all_files

    return collect_all_files(repo)


def _content_candidates(repo: Path) -> list[str]:
    """Files the content (grep) index would take."""
    from .content import _candidate_files

    return _candidate_files(repo)


def index_counts(store, repos: set[Path]) -> tuple[dict[Path, int], dict[Path, int] | None]:
    """Indexed files per repo root, graph File nodes and content rows kept apart.

    Graph rows are committed per file, content rows only when a repo's content sync ends, so a killed build
    leaves one without the other. The content side is None when the content index is unavailable.
    """
    from .content import ensure_schema

    roots = {str(r): r for r in repos}
    graph: dict[Path, int] = {r: 0 for r in repos}

    def owner(path: str) -> Path | None:
        for parent in Path(path).parents:
            if str(parent) in roots:
                return roots[str(parent)]
        return None

    for f in store.get_all_files():
        r = owner(f)
        if r is not None:
            graph[r] += 1
    if not ensure_schema(store._conn):
        return graph, None
    content: dict[Path, int] = {r: 0 for r in repos}
    for (path,) in store._conn.execute("SELECT path FROM file_content_meta"):
        r = owner(path)
        if r is not None:
            content[r] += 1
    return graph, content


class _IndexGaps:
    """Which indexes a repo is missing. The file scans only run for a zero count, once per repo per sync."""

    def __init__(self) -> None:
        self._parseable: dict[Path, bool] = {}
        self._candidates: dict[Path, bool] = {}

    def parseable(self, repo: Path) -> bool:
        if repo not in self._parseable:
            self._parseable[repo] = bool(_parseable_files(repo))
        return self._parseable[repo]

    def candidates(self, repo: Path) -> bool:
        if repo not in self._candidates:
            self._candidates[repo] = bool(_content_candidates(repo))
        return self._candidates[repo]

    def missing_graph(self, repo: Path, graph: dict[Path, int]) -> bool:
        return graph[repo] == 0 and self.parseable(repo)

    def missing_content(self, repo: Path, content: dict[Path, int] | None) -> bool:
        return content is not None and content[repo] == 0 and self.candidates(repo)

    def indexable(self, repo: Path, graph: dict[Path, int], content: dict[Path, int] | None) -> bool:
        if graph[repo] or (content is not None and content[repo]):
            return True
        return self.parseable(repo) or (content is not None and self.candidates(repo))


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
    gaps = _IndexGaps()
    with GraphStore(db_path) as store:
        # Look at the index BEFORE building. A clone left without graph or content rows (a sync killed
        # after the clone, or between the graph and the content commit) must get a full build whatever
        # git says: incremental_update only builds the graph for changed files, so an update on top of
        # an empty index would leave it half-indexed.
        graph_before, content_before = index_counts(store, in_scope)
        reindexed = 0
        for r in results:
            path = r["path"]
            if r["action"] == "cloned":
                full_build(path, store)
            elif path in in_scope and (gaps.missing_graph(path, graph_before)
                                       or gaps.missing_content(path, content_before)):
                logger.warning("%s: index incomplete (graph %d, content %s), indexing now", path,
                               graph_before[path], content_before[path] if content_before else "n/a")
                full_build(path, store)
                reindexed += 1
            elif r["action"] == "updated":
                incremental_update(path, store, changed_files=r["changed"])

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

        # Index health: every clone with something indexable must have its graph rows (if it has parseable
        # files) and its content rows (if it has searchable files).
        failed_paths = {r["path"] for r in results if r["action"] == "failed"}
        graph_now, content_now = index_counts(store, in_scope)
        expected = sorted(p for p in in_scope - failed_paths if gaps.indexable(p, graph_now, content_now))
        missing = [p for p in expected if gaps.missing_graph(p, graph_now) or gaps.missing_content(p, content_now)]
        for p in missing:
            logger.warning("%s: not indexed completely (graph %d, content %s)", p.relative_to(repos_dir), graph_now[p],
                           content_now[p] if content_now else "n/a")

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
