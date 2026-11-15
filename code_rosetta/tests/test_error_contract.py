"""Tool errors must be flagged as errors on the wire, and bad arguments rejected, not silently answered."""

import asyncio
import subprocess

import pytest
from fastmcp import Client

from code_rosetta import tools
from code_rosetta.main import mcp


@pytest.fixture
def repo(tmp_path, monkeypatch):
    monkeypatch.setenv("CODE_ROSETTA_CONFIG", str(tmp_path / "none.yaml"))
    root = tmp_path / "svc"
    root.mkdir()
    (root / "app.py").write_text("def handler():\n    return 'needle'\n")
    subprocess.run(["git", "init", "-q"], cwd=root, check=True)
    subprocess.run(["git", "add", "-A"], cwd=root, check=True)
    tools.build_or_update_graph(full_rebuild=True, repo_root=str(root))
    return root


def _call(name, args):
    async def go():
        async with Client(mcp) as c:
            return await c.call_tool(name, args, raise_on_error=False)

    return asyncio.run(go())


def test_invalid_regex_is_flagged_as_error(repo):
    r = _call("grep_code_tool", {"pattern": "(foo", "regex": True, "repo_root": str(repo)})
    assert r.is_error is True
    assert r.structured_content["status"] == "error"
    assert "invalid regex" in r.structured_content["error"]


def test_successful_call_is_not_an_error(repo):
    r = _call("grep_code_tool", {"pattern": "needle", "repo_root": str(repo)})
    assert r.is_error is False
    assert r.structured_content["status"] == "ok"


def test_unknown_query_pattern_is_flagged_as_error(repo):
    r = _call("query_graph_tool", {"pattern": "nope", "target": "x", "repo_root": str(repo)})
    assert r.is_error is True


def test_empty_grep_pattern_is_rejected(repo):
    r = _call("grep_code_tool", {"pattern": "", "repo_root": str(repo)})
    assert r.is_error is True
    assert "pattern" in r.structured_content["error"]


def test_empty_search_query_is_rejected(repo):
    r = _call("search_nodes_tool", {"query": "  ", "repo_root": str(repo)})
    assert r.is_error is True
    assert "query" in r.structured_content["error"]


@pytest.mark.parametrize("limit", [0, -1])
def test_limit_below_one_is_rejected(repo, limit):
    for name, args in (("grep_code_tool", {"pattern": "needle"}), ("search_nodes_tool", {"query": "handler"})):
        r = _call(name, {**args, "limit": limit, "repo_root": str(repo)})
        assert r.is_error is True, name
        assert "limit" in r.structured_content["error"]
