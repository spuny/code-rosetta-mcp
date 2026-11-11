"""MCP server entry point for Code Rosetta."""

from __future__ import annotations

import json
from typing import Any, Optional

from fastmcp import FastMCP
from fastmcp.tools.tool import ToolResult

from .tools import (
    build_or_update_graph,
    get_impact_radius,
    get_review_context,
    grep_content,
    list_graph_stats,
    query_graph,
    search_nodes,
)

_default_repo_root: str | None = None


def _flag_errors(result: dict[str, Any]) -> Any:
    """A result with status "error" must reach the client as isError, so agents don't read it as empty."""
    if isinstance(result, dict) and result.get("status") == "error":
        return ToolResult(content=json.dumps(result), structured_content=result, is_error=True)
    return result

mcp = FastMCP(
    "code-rosetta",
    instructions=(
        "Cross-language codebase graph for token-efficient code understanding. "
        "Parses Python, Terraform/HCL, YAML, and Jinja2 into a unified knowledge graph "
        "with cross-language reference detection. "
        "Use the 'group' parameter to query named config groups (e.g. group='mygroup') "
        "for cross-repo analysis. Available groups: use list_graph_stats_tool to check."
    ),
)


@mcp.tool()
def build_or_update_graph_tool(
    full_rebuild: bool = False,
    repo_root: Optional[str] = None,
    base: str = "HEAD~1",
    group: Optional[str] = None,
) -> dict:
    """Build or incrementally update the code knowledge graph.

    Parses Python, Terraform/HCL, YAML, and Jinja2 files into a unified graph.
    Call this first to initialize, or after making changes.

    Args:
        full_rebuild: If True, re-parse all files. Default: False (incremental).
        repo_root: Repository root path. Auto-detected if omitted.
        base: Git ref for incremental diff. Default: HEAD~1.
        group: Named config group (e.g. 'mygroup'). Builds all repos in the group.
               Takes precedence over repo_root.
    """
    return _flag_errors(build_or_update_graph(
        full_rebuild=full_rebuild,
        repo_root=repo_root or _default_repo_root,
        base=base,
        group=group,
    ))


@mcp.tool()
def get_impact_radius_tool(
    changed_files: Optional[list[str]] = None,
    max_depth: int = 2,
    repo_root: Optional[str] = None,
    base: str = "HEAD~1",
    group: Optional[str] = None,
) -> dict:
    """Analyze the blast radius of changed files across all languages.

    Shows impacted functions, classes, resources, and templates.
    Includes cross-language impact (e.g., Python change affecting YAML config).

    Args:
        changed_files: Changed file paths. Auto-detected from git if omitted.
        max_depth: Hops to traverse. Default: 2.
        repo_root: Repository root path. Auto-detected if omitted.
        base: Git ref for change detection. Default: HEAD~1.
        group: Named config group. Opens the group's shared DB for cross-repo analysis.
    """
    return _flag_errors(get_impact_radius(
        changed_files=changed_files, max_depth=max_depth,
        repo_root=repo_root or _default_repo_root, base=base,
        group=group,
    ))


@mcp.tool()
def query_graph_tool(
    pattern: str,
    target: str = "",
    repo_root: Optional[str] = None,
    group: Optional[str] = None,
) -> dict:
    """Run a predefined graph query to explore code relationships.

    Available patterns:
    - callers_of: Functions that call the target
    - callees_of: Functions called by the target
    - imports_of: What the target imports
    - importers_of: Files that import the target
    - children_of: Nodes contained in a file or class
    - tests_for: Tests for the target
    - inheritors_of: Classes inheriting from the target
    - file_summary: All nodes in a file
    - references_to: All references to a resource/variable/module
    - cross_language: All cross-language edges in the graph

    Args:
        pattern: Query pattern name.
        target: Node name, qualified name, or file path.
        repo_root: Repository root path. Auto-detected if omitted.
        group: Named config group. Opens the group's shared DB for cross-repo queries.
    """
    return _flag_errors(query_graph(
        pattern=pattern, target=target,
        repo_root=repo_root or _default_repo_root,
        group=group,
    ))


@mcp.tool()
def search_nodes_tool(
    query: str,
    kind: Optional[str] = None,
    language: Optional[str] = None,
    limit: int = 20,
    repo_root: Optional[str] = None,
    group: Optional[str] = None,
) -> dict:
    """Search for code entities by name across all languages.

    Supports filtering by node kind (Function, Class, Resource, Variable, etc.)
    and by language (python, hcl, yaml, jinja2).

    Args:
        query: Search string.
        kind: Filter by node kind.
        language: Filter by language.
        limit: Max results. Default: 20.
        repo_root: Repository root path. Auto-detected if omitted.
        group: Named config group. Searches across all repos in the group.
    """
    return _flag_errors(search_nodes(
        query=query, kind=kind, language=language,
        limit=limit, repo_root=repo_root or _default_repo_root,
        group=group,
    ))


@mcp.tool()
def list_graph_stats_tool(
    repo_root: Optional[str] = None,
    group: Optional[str] = None,
) -> dict:
    """Get aggregate statistics about the code knowledge graph.

    Shows nodes, edges, languages, files, and last update time.
    Useful for checking if the graph is built and current.

    Args:
        repo_root: Repository root path. Auto-detected if omitted.
        group: Named config group. Shows stats for the group's shared graph.
    """
    return _flag_errors(list_graph_stats(repo_root=repo_root or _default_repo_root, group=group))


@mcp.tool()
def get_review_context_tool(
    changed_files: Optional[list[str]] = None,
    max_depth: int = 2,
    include_source: bool = True,
    max_lines_per_file: int = 200,
    repo_root: Optional[str] = None,
    base: str = "HEAD~1",
    group: Optional[str] = None,
) -> dict:
    """Generate a focused, token-efficient review context for code changes.

    Combines cross-language impact analysis with source snippets.

    Args:
        changed_files: Files to review. Auto-detected if omitted.
        max_depth: Impact radius depth. Default: 2.
        include_source: Include source snippets. Default: True.
        max_lines_per_file: Max source lines per file. Default: 200.
        repo_root: Repository root path. Auto-detected if omitted.
        base: Git ref for change detection. Default: HEAD~1.
        group: Named config group. Uses the group's shared graph for cross-repo context.
    """
    return _flag_errors(get_review_context(
        changed_files=changed_files, max_depth=max_depth,
        include_source=include_source, max_lines_per_file=max_lines_per_file,
        repo_root=repo_root or _default_repo_root, base=base,
        group=group,
    ))


@mcp.tool()
def grep_code_tool(
    pattern: str,
    regex: bool = False,
    ignore_case: bool = False,
    path_glob: Optional[str] = None,
    limit: int = 100,
    files_only: bool = False,
    repo_root: Optional[str] = None,
    group: Optional[str] = None,
) -> dict:
    """Grep file contents across every tracked text file, any language.

    Use for literal strings, env vars, image names, URLs, error messages and
    "is X anywhere in code". Use search_nodes_tool / query_graph_tool for
    structure (who calls, what references).

    Args:
        pattern: Literal text, or a Python regex when regex=True.
        regex: Treat pattern as a regex.
        ignore_case: Case-insensitive match.
        path_glob: fnmatch filter on the absolute path, e.g. "*/infra*/cluster/*".
        limit: Max matching lines (or files with files_only). Default: 100.
        files_only: Return matching file paths only.
        repo_root: Repository root path. Auto-detected if omitted.
        group: Named config group. Searches across all repos in the group.
    """
    return _flag_errors(grep_content(
        pattern=pattern, regex=regex, ignore_case=ignore_case,
        path_glob=path_glob, limit=limit, files_only=files_only,
        repo_root=repo_root or _default_repo_root, group=group,
    ))


@mcp.custom_route("/healthz", methods=["GET"])
async def healthz(_request):
    from starlette.responses import PlainTextResponse

    return PlainTextResponse("ok")


_LANDING = """<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>Code Rosetta</title>
<style>body{{font:16px/1.5 system-ui,sans-serif;max-width:46rem;margin:3rem auto;padding:0 1rem;color:#222}}
code,pre{{background:#f3f3f3;border-radius:4px;padding:.1rem .3rem}}pre{{padding:.7rem;overflow-x:auto}}</style>
</head><body>
<h1>Code Rosetta</h1>
<p>A shared code-search server for agents and tools: grep over every file in the indexed GitLab projects,
plus a cross-language graph (who calls what, what references what). It speaks MCP, not HTML.</p>
<h2>Endpoint</h2>
<p><code>{endpoint}</code> (streamable HTTP MCP)</p>
<h2>Connect</h2>
<p>Claude Code:</p>
<pre>claude mcp add --transport http rosetta {endpoint}</pre>
<p>Any other MCP client: add an HTTP server with the URL above.</p>
<h2>Tools</h2>
<ul>
{tools}
</ul>
<p>Each hit carries <code>repo</code>, <code>path</code>, <code>ref</code> and <code>web_url</code> (a GitLab link at the indexed commit).</p>
<h2>For humans</h2>
<p>There is no web search UI yet: use Sourcebot for searching in a browser.</p>
</body></html>
"""


@mcp.custom_route("/", methods=["GET"])
async def landing(request):
    import html

    from starlette.responses import HTMLResponse

    scheme = request.headers.get("x-forwarded-proto") or request.url.scheme
    host = request.headers.get("host") or request.url.netloc
    endpoint = html.escape(f"{scheme}://{host}/mcp", quote=True)
    try:
        tools = sorted(await mcp.list_tools(), key=lambda t: t.name)
        items = "\n".join(
            f"<li><code>{html.escape(t.name)}</code> &mdash; "
            f"{html.escape((t.description or '').strip().splitlines()[0] if t.description else '')}</li>"
            for t in tools
        )
    except Exception:  # noqa: BLE001 -- the page must render even if tool listing breaks
        items = "<li>see <code>tools/list</code> on the endpoint</li>"
    return HTMLResponse(_LANDING.format(endpoint=endpoint, tools=items))


def main(
    repo_root: str | None = None,
    transport: str = "stdio",
    host: str = "127.0.0.1",
    port: int = 8000,
    read_only: bool = False,
) -> None:
    """Run the MCP server (stdio by default, or streamable HTTP)."""
    global _default_repo_root
    _default_repo_root = repo_root
    if read_only:
        # Shared server: the index is owned by sync-gitlab, clients must not rebuild it.
        mcp.remove_tool("build_or_update_graph_tool")
    if transport == "stdio":
        mcp.run(transport="stdio")
    else:
        mcp.run(transport="http", host=host, port=port, path="/mcp", stateless_http=True)


if __name__ == "__main__":
    main()
