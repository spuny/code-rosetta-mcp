"""Hits must say which project they belong to and where to open them (QA r1, finding 5)."""

import importlib
import subprocess

import pytest

from code_rosetta import config, gitlab_sync, hits, tools

BASE = "https://gitlab.example"


def _run(args, cwd):
    subprocess.run(args, cwd=cwd, check=True, capture_output=True)


def _origin(tmp, name, files):
    bare = tmp / "remote" / f"{name}.git"
    work = tmp / "work" / name
    _run(["git", "init", "-q", "--bare", "-b", "main", str(bare)], tmp)
    work.mkdir(parents=True)
    _run(["git", "init", "-q", "-b", "main"], work)
    for rel, data in files.items():
        p = work / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(data)
    _run(["git", "add", "-A"], work)
    _run(["git", "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "c"], work)
    _run(["git", "remote", "add", "origin", str(bare)], work)
    _run(["git", "push", "-q", "origin", "main"], work)
    return bare, work


@pytest.fixture
def synced(tmp_path, monkeypatch):
    """Two sibling projects in a nested group, mirrored the way the sync does it, config + db written."""
    monkeypatch.setenv("CODE_ROSETTA_CONFIG", str(tmp_path / "data" / "config.yaml"))
    importlib.reload(config)
    importlib.reload(gitlab_sync)
    importlib.reload(tools)
    from code_rosetta.cli import _ensure_parsers

    _ensure_parsers()
    a_bare, a_work = _origin(tmp_path, "a", {"src/deploy.yaml": "image: ghcr.io/x/web:v1\n"})
    b_bare, _ = _origin(tmp_path, "b", {"x.tf": 'resource "aws_s3_bucket" "logs" {}\n'})
    projects = [
        {"id": 1, "path_with_namespace": "acme/apis/orders-api", "default_branch": "main",
         "http_url_to_repo": f"file://{a_bare}", "empty_repo": False},
        {"id": 2, "path_with_namespace": "acme/apis/clients", "default_branch": "main",
         "http_url_to_repo": f"file://{b_bare}", "empty_repo": False},
    ]
    monkeypatch.setattr(gitlab_sync, "list_projects", lambda *a, **k: projects)
    gitlab_sync.sync(url=BASE, token="t", groups=["data"], repos_dir=tmp_path / "data" / "repos",
                     db_path=tmp_path / "data" / "q.db", group_name="acme")
    sha = subprocess.run(["git", "rev-parse", "HEAD"], cwd=a_work, capture_output=True, text=True).stdout.strip()
    yield tmp_path, sha
    monkeypatch.delenv("CODE_ROSETTA_CONFIG")
    importlib.reload(config)
    importlib.reload(tools)
    importlib.reload(hits)


def test_grep_hit_names_repo_ref_and_gitlab_link(synced):
    _, sha = synced
    res = tools.grep_content("ghcr.io/x/web", group="acme")
    (m,) = res["matches"]
    assert m["repo"] == "acme/apis/orders-api"
    assert m["path"] == "src/deploy.yaml"
    assert m["line"] == 1
    assert m["ref"] == sha
    assert m["web_url"] == f"{BASE}/acme/apis/orders-api/-/blob/{sha}/src/deploy.yaml#L1"
    assert m["abs_path"].endswith("/repos/acme/apis/orders-api/src/deploy.yaml")


def test_nested_groups_resolve_to_the_right_project(synced):
    res = tools.grep_content("aws_s3_bucket", group="acme")
    (m,) = res["matches"]
    assert (m["repo"], m["path"]) == ("acme/apis/clients", "x.tf")


def test_files_only_hits_carry_repo_and_link(synced):
    _, sha = synced
    res = tools.grep_content("ghcr.io/x/web", group="acme", files_only=True)
    assert res["files"][0].endswith("/src/deploy.yaml")  # old shape kept
    (h,) = res["file_hits"]
    assert h["repo"] == "acme/apis/orders-api" and h["path"] == "src/deploy.yaml"
    assert h["web_url"] == f"{BASE}/acme/apis/orders-api/-/blob/{sha}/src/deploy.yaml"


def test_search_nodes_results_carry_repo_and_link(synced):
    res = tools.search_nodes("aws_s3_bucket", group="acme")
    r = res["results"][0]
    assert r["repo"] == "acme/apis/clients" and r["path"] == "x.tf"
    assert r["web_url"].startswith(f"{BASE}/acme/apis/clients/-/blob/")
    assert r["file_path"].endswith("/repos/acme/apis/clients/x.tf")  # old key kept


def test_ref_is_cached_until_the_config_changes(synced, monkeypatch):
    calls = []
    real = hits._git_head

    def counting(root):
        calls.append(root)
        return real(root)

    monkeypatch.setattr(hits, "_git_head", counting)
    tools.grep_content("ghcr.io/x/web", group="acme")
    tools.grep_content("ghcr.io/x/web", group="acme")
    assert len(calls) == 1
    # a sync rewrites the config file: the next query must look again
    cfg_file = config._CONFIG_FILE
    cfg_file.write_text(cfg_file.read_text() + "\n")
    import os, time
    os.utime(cfg_file, (time.time() + 5, time.time() + 5))
    tools.grep_content("ghcr.io/x/web", group="acme")
    assert len(calls) == 2


def test_without_repos_dir_the_hit_still_has_a_repo_name_but_no_link(tmp_path):
    root = tmp_path / "checkout" / "svc"
    root.mkdir(parents=True)
    r = hits.HitResolver([root], repos_dir=None, gitlab_url=None)
    h = r.hit(str(root / "a" / "b.py"), 7)
    assert (h["repo"], h["path"], h["web_url"]) == ("svc", "a/b.py", None)


def test_path_outside_all_repos_is_left_alone(tmp_path):
    r = hits.HitResolver([tmp_path / "x"], repos_dir=tmp_path, gitlab_url=BASE)
    h = r.hit("/elsewhere/file.py", 3)
    assert h["repo"] is None and h["web_url"] is None and h["abs_path"] == "/elsewhere/file.py"
