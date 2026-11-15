"""Default-group routing must not change local repository selection."""

import asyncio
import json

import pytest
from fastmcp import Client
from ruamel.yaml import YAML

from code_rosetta import config
import code_rosetta.main as server
from code_rosetta.main import mcp
from code_rosetta.tests.test_hits import synced  # noqa: F401  (fixture: group "acme", two projects)


def _call(name, args):
    async def go():
        async with Client(mcp) as c:
            return await c.call_tool(name, args, raise_on_error=False)

    r = asyncio.run(go())
    return r, (json.loads(r.content[0].text) if r.content else None)


@pytest.fixture
def elsewhere(synced, monkeypatch):  # noqa: F811
    """The server's cwd is somewhere that is not a project, like /app in the container."""
    tmp, _ = synced
    cwd = tmp / "app"
    cwd.mkdir()
    monkeypatch.chdir(cwd)
    monkeypatch.delenv("ROSETTA_DEFAULT_GROUP", raising=False)
    return tmp


def _add_group(tmp, name, **extra):
    path = tmp / "data" / "config.yaml"
    yaml = YAML()
    data = yaml.load(path.read_text())
    data["graphs"][name] = {"db": str(tmp / "data" / f"{name}.db"), "repos": []}
    data.update(extra)
    with open(path, "w") as f:
        yaml.dump(data, f)
    config.cfg._refresh()


def test_the_only_group_is_the_default(elsewhere):
    for tool, args in (("grep_code_tool", {"pattern": "aws_s3_bucket"}),
                       ("search_nodes_tool", {"query": "aws_s3_bucket"}),
                       ("list_graph_stats_tool", {})):
        r, body = _call(tool, args)
        assert r.is_error is False, (tool, body)
    _, body = _call("grep_code_tool", {"pattern": "aws_s3_bucket"})
    assert [m["repo"] for m in body["matches"]] == ["acme/apis/clients"]
    assert not (elsewhere / "app" / ".code-rosetta").exists()  # the cwd is never touched


def test_env_setting_names_the_default_among_several_groups(elsewhere, monkeypatch):
    _add_group(elsewhere, "other")
    monkeypatch.setenv("ROSETTA_DEFAULT_GROUP", "acme")
    r, body = _call("grep_code_tool", {"pattern": "aws_s3_bucket"})
    assert r.is_error is False and body["matches"]


def test_config_key_names_the_default_among_several_groups(elsewhere):
    _add_group(elsewhere, "other", default_group="acme")
    r, body = _call("grep_code_tool", {"pattern": "aws_s3_bucket"})
    assert r.is_error is False and body["matches"]


def test_several_groups_and_no_default_is_an_error_that_says_how_to_pass_it(elsewhere):
    _add_group(elsewhere, "other")
    r, body = _call("grep_code_tool", {"pattern": "aws_s3_bucket"})
    assert r.is_error is True
    assert "group=" in body["error"] and "acme" in body["error"] and "other" in body["error"]
    assert not (elsewhere / "app" / ".code-rosetta").exists()


def test_an_explicit_group_beats_the_default(elsewhere, monkeypatch):
    _add_group(elsewhere, "other")
    monkeypatch.setenv("ROSETTA_DEFAULT_GROUP", "other")
    r, body = _call("grep_code_tool", {"pattern": "aws_s3_bucket", "group": "acme"})
    assert r.is_error is False and body["matches"]


def test_the_tool_manual_says_group_is_optional(elsewhere):
    async def go():
        async with Client(mcp) as c:
            return {t.name: t for t in await c.list_tools()}

    tools = asyncio.run(go())
    for name in ("grep_code_tool", "search_nodes_tool", "query_graph_tool", "list_graph_stats_tool"):
        text = tools[name].inputSchema["properties"]["group"]["description"]
        assert "ROSETTA_DEFAULT_GROUP" in text and "optional" in text.lower(), name


@pytest.mark.parametrize("marker", [".git", ".code-rosetta"])
def test_stdio_in_a_repo_keeps_using_the_local_repo(elsewhere, monkeypatch, marker):
    root = elsewhere / "local"
    root.mkdir()
    (root / marker).mkdir()
    monkeypatch.chdir(root)
    r, body = _call("list_graph_stats_tool", {})
    assert r.is_error is False
    assert body["files_count"] == 0
    assert (root / ".code-rosetta" / "graph.db").exists()


def test_build_tool_uses_the_server_default(elsewhere, monkeypatch):
    calls = []
    monkeypatch.setattr(server, "build_or_update_graph", lambda **kwargs: calls.append(kwargs) or {"status": "ok"})
    r, body = _call("build_or_update_graph_tool", {})
    assert r.is_error is False and body["status"] == "ok"
    assert calls[0]["group"] == "acme"


def test_http_uses_the_default_group_even_when_started_in_a_repo(elsewhere, monkeypatch):
    root = elsewhere / "local"
    root.mkdir()
    (root / ".git").mkdir()
    monkeypatch.chdir(root)
    monkeypatch.setattr(server, "_transport", "http", raising=False)
    r, body = _call("grep_code_tool", {"pattern": "aws_s3_bucket"})
    assert r.is_error is False and body["matches"]
    assert not (root / ".code-rosetta").exists()


def test_http_without_config_returns_a_setup_error(elsewhere, monkeypatch):
    path = elsewhere / "data" / "config.yaml"
    path.write_text("graphs: {}\n")
    monkeypatch.setattr(server, "_transport", "http", raising=False)
    r, body = _call("list_graph_stats_tool", {})
    assert r.is_error is True
    assert "sync-gitlab" in body["error"]
    assert not (elsewhere / "app" / ".code-rosetta").exists()


def test_explicit_repo_wins_over_the_server_default(elsewhere):
    root = elsewhere / "local"
    root.mkdir()
    (root / ".git").mkdir()
    r, body = _call("list_graph_stats_tool", {"repo_root": str(root)})
    assert r.is_error is False
    assert body["files_count"] == 0
    assert (root / ".code-rosetta" / "graph.db").exists()
