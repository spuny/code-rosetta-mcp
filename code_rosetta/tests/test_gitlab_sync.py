import importlib
import io
import json
import subprocess
import urllib.parse

import pytest

from code_rosetta import gitlab_sync
from code_rosetta.content import grep
from code_rosetta.graph import GraphStore


def _run(args, cwd):
    subprocess.run(args, cwd=cwd, check=True, capture_output=True)


def _origin(tmp_path, name, files):
    """A bare 'remote' with one commit on main, plus a work clone to push from."""
    bare = tmp_path / "remote" / f"{name}.git"
    work = tmp_path / "work" / name
    _run(["git", "init", "-q", "--bare", "-b", "main", str(bare)], tmp_path)
    work.mkdir(parents=True)
    _run(["git", "init", "-q", "-b", "main"], work)
    _commit(work, files, bare)
    return bare, work


def _commit(work, files, bare=None):
    for rel, data in files.items():
        p = work / rel
        if data is None:
            p.unlink()
            continue
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(data)
    _run(["git", "add", "-A"], work)
    _run(["git", "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "c"], work)
    if bare:
        _run(["git", "remote", "add", "origin", str(bare)], work)
    _run(["git", "push", "-q", "origin", "main"], work)


def _project(pid, path, bare, branch="main", empty=False):
    return {"id": pid, "path_with_namespace": path, "default_branch": branch,
            "http_url_to_repo": f"file://{bare}", "empty_repo": empty}


@pytest.fixture
def env(tmp_path, monkeypatch):
    monkeypatch.setenv("CODE_ROSETTA_CONFIG", str(tmp_path / "data" / "config.yaml"))
    from code_rosetta import config
    importlib.reload(config)
    importlib.reload(gitlab_sync)
    from code_rosetta.cli import _ensure_parsers
    _ensure_parsers()
    yield tmp_path
    monkeypatch.delenv("CODE_ROSETTA_CONFIG")
    importlib.reload(config)


def _sync(tmp_path, monkeypatch, projects):
    monkeypatch.setattr(gitlab_sync, "list_projects", lambda *a, **k: projects)
    return gitlab_sync.sync(
        url="https://gitlab.example", token="t", groups=["devops"],
        repos_dir=tmp_path / "data" / "repos", db_path=tmp_path / "data" / "q.db",
    )


def _grep(tmp_path, pattern):
    with GraphStore(tmp_path / "data" / "q.db") as store:
        return grep(store, pattern)


def _paths(res):
    return sorted({m["path"].split("/repos/")[1] for m in res.get("matches", [])})


def test_clone_update_and_purge(env, monkeypatch):
    tmp = env
    a_bare, a_work = _origin(tmp, "a", {"deploy.yaml": "image: ghcr.io/x/web:v1\n", "app.py": "def handler():\n    pass\n"})
    b_bare, _ = _origin(tmp, "b", {"main.tf": 'resource "aws_s3_bucket" "logs" {}\n'})
    empty = tmp / "remote" / "empty.git"
    _run(["git", "init", "-q", "--bare", str(empty)], tmp)
    projects = [_project(1, "devops/a", a_bare), _project(2, "devops/b", b_bare),
                _project(3, "devops/empty", empty, empty=True)]  # GitLab sends default_branch=main

    r = _sync(tmp, monkeypatch, projects)
    assert (r["cloned"], r["skipped"], r["ok"]) == (2, 1, True)
    assert _paths(_grep(tmp, "ghcr.io/x/web")) == ["devops/a/deploy.yaml"]
    assert _paths(_grep(tmp, "aws_s3_bucket")) == ["devops/b/main.tf"]

    # Token must not be persisted in the clone's git config.
    assert "Authorization" not in (tmp / "data/repos/devops/a/.git/config").read_text()

    cfg_text = (tmp / "data" / "config.yaml").read_text()
    assert "devops/a" in cfg_text and "devops/empty" not in cfg_text

    _commit(a_work, {"deploy.yaml": "image: ghcr.io/x/web:v2\n", "app.py": None})
    r = _sync(tmp, monkeypatch, projects)
    assert (r.get("updated"), r.get("unchanged")) == (1, 1)
    assert _grep(tmp, "web:v1").get("matches", []) == []
    assert _paths(_grep(tmp, "web:v2")) == ["devops/a/deploy.yaml"]
    assert _grep(tmp, "def handler").get("matches", []) == []

    r = _sync(tmp, monkeypatch, [projects[0]])
    assert r["removed"] == 1
    assert _grep(tmp, "aws_s3_bucket").get("matches", []) == []
    assert not (tmp / "data/repos/devops/b").exists()
    with GraphStore(tmp / "data" / "q.db") as store:
        assert not [f for f in store.get_all_files() if "/devops/b/" in f]


def _drop_index_rows(tmp_path, repo_rel):
    """Remove a repo's graph and content rows but leave its clone on disk (what a killed sync leaves)."""
    prefix = str((tmp_path / "data" / "repos" / repo_rel).resolve()) + "/"
    with GraphStore(tmp_path / "data" / "q.db") as store:
        for f in store.get_all_files():
            if f.startswith(prefix):
                store.remove_file_data(f)
        conn = store._conn
        for path, rowid in conn.execute("SELECT path, fts_rowid FROM file_content_meta").fetchall():
            if path.startswith(prefix):
                conn.execute("DELETE FROM file_content WHERE rowid = ?", (rowid,))
                conn.execute("DELETE FROM file_content_meta WHERE path = ?", (path,))
        store.commit()


def test_sync_heals_clone_that_was_never_indexed(env, monkeypatch):
    """A sync killed after clone but before indexing leaves a clone later runs call 'unchanged'."""
    tmp = env
    a_bare, _ = _origin(tmp, "a", {"deploy.yaml": "image: ghcr.io/x/web:v1\n"})
    b_bare, _ = _origin(tmp, "b", {"main.tf": 'resource "aws_s3_bucket" "logs" {}\n'})
    projects = [_project(1, "devops/a", a_bare), _project(2, "devops/b", b_bare)]
    _sync(tmp, monkeypatch, projects)

    # Simulate the kill: b stays cloned on disk but its index rows are gone.
    _drop_index_rows(tmp, "devops/b")
    assert _grep(tmp, "aws_s3_bucket").get("matches", []) == []

    r = _sync(tmp, monkeypatch, projects)
    assert r["unchanged"] == 2
    assert r["reindexed"] == 1
    assert _paths(_grep(tmp, "aws_s3_bucket")) == ["devops/b/main.tf"]
    assert _paths(_grep(tmp, "ghcr.io/x/web")) == ["devops/a/deploy.yaml"]

    r = _sync(tmp, monkeypatch, projects)
    assert r["reindexed"] == 0


def test_repo_with_nothing_indexable_is_not_rebuilt_every_run(env, monkeypatch):
    tmp = env
    bare, work = _origin(tmp, "blob", {"x.txt": "placeholder\n"})
    (work / "x.txt").unlink()
    (work / "data.bin").write_bytes(b"\x00\x01\x02binary")
    _run(["git", "add", "-A"], work)
    _run(["git", "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "bin"], work)
    _run(["git", "push", "-q", "origin", "main"], work)
    a_bare, _ = _origin(tmp, "a", {"deploy.yaml": "image: ghcr.io/x/web:v1\n"})
    projects = [_project(1, "devops/a", a_bare), _project(2, "devops/blob", bare)]
    _sync(tmp, monkeypatch, projects)
    r = _sync(tmp, monkeypatch, projects)
    assert r["reindexed"] == 0


def test_failed_fetch_keeps_existing_repo(env, monkeypatch):
    tmp = env
    a_bare, _ = _origin(tmp, "a", {"x.yaml": "key: keepme\n"})
    projects = [_project(1, "devops/a", a_bare)]
    _sync(tmp, monkeypatch, projects)
    broken = [_project(1, "devops/a", tmp / "nope.git")]
    r = _sync(tmp, monkeypatch, broken)
    assert r["failed"] == 1 and r["removed"] == 0
    assert _paths(_grep(tmp, "keepme")) == ["devops/a/x.yaml"]


def test_list_projects_paginates_and_dedups(monkeypatch):
    pages = {
        ("devops", 1): [{"id": 1, "path_with_namespace": "devops/a"}, {"id": 2, "path_with_namespace": "devops/b"}],
        ("devops", 2): [],
        ("data", 1): [{"id": 2, "path_with_namespace": "devops/b"}, {"id": 3, "path_with_namespace": "data/c"}],
        ("data", 2): [],
    }
    seen_headers = []

    def fake_urlopen(req, timeout):
        seen_headers.append(req.get_header("Private-token"))
        group = req.full_url.split("/groups/")[1].split("/")[0]
        page = int(urllib.parse.parse_qs(urllib.parse.urlsplit(req.full_url).query)["page"][0])
        return io.BytesIO(json.dumps(pages[(group, page)]).encode())

    monkeypatch.setattr(gitlab_sync.urllib.request, "urlopen", fake_urlopen)
    out = gitlab_sync.list_projects("https://gl", "tok", ["devops", "data"])
    assert [p["path_with_namespace"] for p in out] == ["data/c", "devops/a", "devops/b"]
    assert set(seen_headers) == {"tok"}


def test_list_projects_requests_empty_repo_field(monkeypatch):
    urls = []

    def fake_urlopen(req, timeout):
        urls.append(req.full_url)
        return io.BytesIO(b"[]")

    monkeypatch.setattr(gitlab_sync.urllib.request, "urlopen", fake_urlopen)
    gitlab_sync.list_projects("https://gl", "tok", ["devops"])
    assert "simple=" not in urls[0]


def test_config_path_from_env(tmp_path, monkeypatch):
    monkeypatch.setenv("CODE_ROSETTA_CONFIG", str(tmp_path / "x" / "c.yaml"))
    from code_rosetta import config
    importlib.reload(config)
    try:
        assert config._CONFIG_FILE == tmp_path / "x" / "c.yaml"
        assert config._CONFIG_DIR == tmp_path / "x"
    finally:
        monkeypatch.delenv("CODE_ROSETTA_CONFIG")
        importlib.reload(config)
