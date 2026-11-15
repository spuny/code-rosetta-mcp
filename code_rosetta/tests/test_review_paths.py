"""changed_files comes from the client: it must never reach a file outside the configured repo roots."""

import asyncio
import importlib
import subprocess

import pytest
from fastmcp import Client

from code_rosetta import config, gitlab_sync, tools
from code_rosetta.main import mcp

SECRET = "TOP-SECRET-TOKEN-VALUE"


@pytest.fixture
def repo(tmp_path, monkeypatch):
    from code_rosetta.cli import _ensure_parsers

    _ensure_parsers()
    monkeypatch.setenv("CODE_ROSETTA_CONFIG", str(tmp_path / "none.yaml"))
    root = tmp_path / "svc"
    root.mkdir()
    (root / "app.py").write_text("def handler():\n    return 'needle'\n")
    (tmp_path / "rosetta.db").write_text(SECRET)  # stands in for /data/rosetta.db
    secrets = tmp_path / "var" / "run" / "secrets" / "kubernetes.io" / "serviceaccount"
    secrets.mkdir(parents=True)
    (secrets / "token").write_text(SECRET)  # stands in for the service-account token
    (root / "link.py").symlink_to(tmp_path / "rosetta.db")
    subprocess.run(["git", "init", "-q"], cwd=root, check=True)
    subprocess.run(["git", "add", "-A"], cwd=root, check=True)
    tools.build_or_update_graph(full_rebuild=True, repo_root=str(root))
    return root


def _escapes(repo):
    tmp = repo.parent
    return {
        "absolute db": str(tmp / "rosetta.db"),
        "absolute token": str(tmp / "var/run/secrets/kubernetes.io/serviceaccount/token"),
        "absolute /etc/passwd": "/etc/passwd",
        "dot-dot": "../rosetta.db",
        "dot-dot deep": "../../../../../../etc/passwd",
        "symlink out": "link.py",
    }


@pytest.mark.parametrize("tool", ["get_review_context", "get_impact_radius"])
def test_paths_outside_the_repo_are_rejected(repo, tool):
    for label, path in _escapes(repo).items():
        res = getattr(tools, tool)(changed_files=[path], repo_root=str(repo))
        assert res["status"] == "error", label
        assert SECRET not in repr(res), label
        assert "root" in res["error"].lower() or "outside" in res["error"].lower(), label


def test_one_bad_path_rejects_the_whole_request(repo):
    res = tools.get_review_context(changed_files=["app.py", "../rosetta.db"], repo_root=str(repo))
    assert res["status"] == "error"
    assert SECRET not in repr(res)


def test_rejection_is_isError_on_the_wire(repo):
    async def go():
        async with Client(mcp) as c:
            return await c.call_tool("get_review_context_tool",
                                     {"changed_files": [str(repo.parent / "rosetta.db")], "repo_root": str(repo)},
                                     raise_on_error=False)

    r = asyncio.run(go())
    assert r.is_error is True
    assert SECRET not in repr(r.content) + repr(r.structured_content)


@pytest.mark.parametrize("path", ["app.py", "./app.py", "sub/../app.py"])
def test_paths_inside_the_repo_still_work(repo, path):
    res = tools.get_review_context(changed_files=[path], repo_root=str(repo))
    assert res["status"] == "ok"
    assert "def handler" in res["context"]["source_snippets"][path]


def test_absolute_path_inside_the_repo_works(repo):
    p = str(repo / "app.py")
    res = tools.get_review_context(changed_files=[p], repo_root=str(repo))
    assert res["status"] == "ok"
    assert "def handler" in res["context"]["source_snippets"][p]
    assert tools.get_impact_radius(changed_files=[p], repo_root=str(repo))["status"] == "ok"


def test_changed_files_count_is_capped(repo):
    names = [f"f{i}.py" for i in range(tools.MAX_CHANGED_FILES + 1)]
    for tool in ("get_review_context", "get_impact_radius"):
        res = getattr(tools, tool)(changed_files=names, repo_root=str(repo))
        assert res["status"] == "error"
        assert str(tools.MAX_CHANGED_FILES) in res["error"]
    ok = tools.get_review_context(changed_files=names[:tools.MAX_CHANGED_FILES], repo_root=str(repo))
    assert ok["status"] == "ok"


def test_big_files_are_not_read_into_memory(repo, monkeypatch):
    monkeypatch.setattr(tools, "MAX_SNIPPET_FILE_BYTES", 100)
    (repo / "big.py").write_text("x = 1\n" * 500)
    res = tools.get_review_context(changed_files=["big.py", "app.py"], repo_root=str(repo))
    assert res["status"] == "ok"
    snippets = res["context"]["source_snippets"]
    assert "too large" in snippets["big.py"]
    assert "x = 1" not in snippets["big.py"]
    assert "def handler" in snippets["app.py"]


def test_group_mode_allows_any_group_repo_and_nothing_else(tmp_path, monkeypatch):
    from code_rosetta.cli import _ensure_parsers

    monkeypatch.setenv("CODE_ROSETTA_CONFIG", str(tmp_path / "data" / "config.yaml"))
    importlib.reload(config)
    importlib.reload(gitlab_sync)
    importlib.reload(tools)
    _ensure_parsers()
    bares = {}
    for name in ("a", "b"):
        bare, work = tmp_path / "remote" / f"{name}.git", tmp_path / "work" / name
        subprocess.run(["git", "init", "-q", "--bare", "-b", "main", str(bare)], check=True)
        work.mkdir(parents=True)
        subprocess.run(["git", "init", "-q", "-b", "main"], cwd=work, check=True)
        (work / "f.py").write_text(f"def fn_{name}():\n    return 1\n")
        subprocess.run(["git", "add", "-A"], cwd=work, check=True)
        subprocess.run(["git", "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "c"], cwd=work, check=True)
        subprocess.run(["git", "remote", "add", "origin", str(bare)], cwd=work, check=True)
        subprocess.run(["git", "push", "-q", "origin", "main"], cwd=work, check=True)
        bares[name] = bare
    projects = [{"id": i, "path_with_namespace": f"g/{n}", "default_branch": "main",
                 "http_url_to_repo": f"file://{b}", "empty_repo": False} for i, (n, b) in enumerate(bares.items(), 1)]
    monkeypatch.setattr(gitlab_sync, "list_projects", lambda *a, **k: projects)
    gitlab_sync.sync(url="https://gl.example", token="t", groups=["g"], repos_dir=tmp_path / "data" / "repos",
                     db_path=tmp_path / "data" / "q.db", group_name="grp")
    (tmp_path / "data" / "outside.txt").write_text(SECRET)

    other_repo_file = str(tmp_path / "data" / "repos" / "g" / "b" / "f.py")  # not the group's first repo
    ok = tools.get_review_context(changed_files=[other_repo_file], group="grp")
    assert ok["status"] == "ok"
    assert "fn_b" in ok["context"]["source_snippets"][other_repo_file]

    for bad in (str(tmp_path / "data" / "outside.txt"), str(tmp_path / "data" / "q.db"), "../../outside.txt"):
        res = tools.get_review_context(changed_files=[bad], group="grp")
        assert res["status"] == "error", bad
        assert SECRET not in repr(res)

    importlib.reload(config)
    importlib.reload(tools)
