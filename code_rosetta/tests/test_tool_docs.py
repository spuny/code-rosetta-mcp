"""What an agent reads in tools/list is its only manual: caps, truncation and hit fields must be in it."""

import asyncio

import pytest
from fastmcp import Client

from code_rosetta import tools
from code_rosetta.main import mcp


def _manual(tool) -> str:
    """Everything an agent reads about a tool: its description plus every parameter's description."""
    params = " ".join(p.get("description", "") for p in tool.inputSchema.get("properties", {}).values())
    return (tool.description or "") + "\n" + params


@pytest.fixture(scope="module")
def tools_list():
    async def go():
        async with Client(mcp) as c:
            return {t.name: t for t in await c.list_tools()}

    return asyncio.run(go())


@pytest.fixture(scope="module")
def descriptions(tools_list):
    return {name: _manual(t) for name, t in tools_list.items()}


def test_every_documented_parameter_keeps_its_own_description(tools_list):
    """A docstring that splits the Args block makes one parameter swallow the next one's text."""
    for name in ("grep_code_tool", "search_nodes_tool", "get_review_context_tool", "get_impact_radius_tool"):
        props = tools_list[name].inputSchema["properties"]
        assert "repository root" in props["repo_root"]["description"].lower(), name
        assert "config group" in props["group"]["description"].lower(), name
        assert "config group" not in props["repo_root"]["description"].lower(), name


def test_grep_describes_caps_truncation_and_hit_fields(descriptions):
    d = descriptions["grep_code_tool"]
    for word in ("limit_clamped", "truncated", "repo", "path", "ref", "web_url", "abs_path", "path_glob"):
        assert word in d, word
    assert f"{tools.GREP_MAX_MATCHES:,}" in d or str(tools.GREP_MAX_MATCHES) in d
    assert f"{tools.GREP_MAX_FILES:,}" in d or str(tools.GREP_MAX_FILES) in d
    assert "repo/path" in d or "repo + path" in d  # path_glob accepts the form a hit reports


def test_search_nodes_describes_cap_and_hit_fields(descriptions):
    d = descriptions["search_nodes_tool"]
    for word in ("limit_clamped", "repo", "path", "ref", "web_url", "file_path"):
        assert word in d, word
    assert f"{tools.SEARCH_MAX_RESULTS:,}" in d or str(tools.SEARCH_MAX_RESULTS) in d


@pytest.mark.parametrize("name", ["get_review_context_tool", "get_impact_radius_tool"])
def test_changed_files_rules_are_documented(descriptions, name):
    d = descriptions[name]
    assert str(tools.MAX_CHANGED_FILES) in d
    assert "repo" in d and ("outside" in d or "under" in d)


def test_review_context_documents_the_file_size_limit(descriptions):
    d = descriptions["get_review_context_tool"]
    assert f"{tools.MAX_SNIPPET_FILE_BYTES // 1_000_000} MB" in d
