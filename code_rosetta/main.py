"""MCP server entry point for Code Rosetta."""

from __future__ import annotations

from typing import Optional

from fastmcp import FastMCP

from .tools import (
    build_or_update_graph,
    get_impact_radius,
    get_review_context,
    list_graph_stats,
    query_graph,
    search_nodes,
)

_default_repo_root: str | None = None

mcp = FastMCP(
    "code-rosetta",
    instructions=(
        "Cross-language codebase graph for token-efficient code understanding. "
        "Parses Python, Terraform/HCL, YAML, and Jinja2 into a unified knowledge graph "
        "with cross-language reference detection."
    ),
)


@mcp.tool()
def build_or_update_graph_tool(
    full_rebuild: bool = False,
    repo_root: Optional[str] = None,
    base: str = "HEAD~1",
) -> dict:
    """Build or incrementally update the code knowledge graph.

    Parses Python, Terraform/HCL, YAML, and Jinja2 files into a unified graph.
    Call this first to initialize, or after making changes.

    Args:
        full_rebuild: If True, re-parse all files. Default: False (incremental).
        repo_root: Repository root path. Auto-detected if omitted.
        base: Git ref for incremental diff. Default: HEAD~1.
    """
    return build_or_update_graph(
        full_rebuild=full_rebuild,
        repo_root=repo_root or _default_repo_root,
        base=base,
    )


@mcp.tool()
def get_impact_radius_tool(
    changed_files: Optional[list[str]] = None,
    max_depth: int = 2,
    repo_root: Optional[str] = None,
    base: str = "HEAD~1",
) -> dict:
    """Analyze the blast radius of changed files across all languages.

    Shows impacted functions, classes, resources, and templates.
    Includes cross-language impact (e.g., Python change affecting YAML config).

    Args:
        changed_files: Changed file paths. Auto-detected from git if omitted.
        max_depth: Hops to traverse. Default: 2.
        repo_root: Repository root path. Auto-detected if omitted.
        base: Git ref for change detection. Default: HEAD~1.
    """
    return get_impact_radius(
        changed_files=changed_files, max_depth=max_depth,
        repo_root=repo_root or _default_repo_root, base=base,
    )


@mcp.tool()
def query_graph_tool(
    pattern: str,
    target: str = "",
    repo_root: Optional[str] = None,
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
    """
    return query_graph(
        pattern=pattern, target=target,
        repo_root=repo_root or _default_repo_root,
    )


@mcp.tool()
def search_nodes_tool(
    query: str,
    kind: Optional[str] = None,
    language: Optional[str] = None,
    limit: int = 20,
    repo_root: Optional[str] = None,
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
    """
    return search_nodes(
        query=query, kind=kind, language=language,
        limit=limit, repo_root=repo_root or _default_repo_root,
    )


@mcp.tool()
def list_graph_stats_tool(
    repo_root: Optional[str] = None,
) -> dict:
    """Get aggregate statistics about the code knowledge graph.

    Shows nodes, edges, languages, files, and last update time.
    Useful for checking if the graph is built and current.

    Args:
        repo_root: Repository root path. Auto-detected if omitted.
    """
    return list_graph_stats(repo_root=repo_root or _default_repo_root)


@mcp.tool()
def get_review_context_tool(
    changed_files: Optional[list[str]] = None,
    max_depth: int = 2,
    include_source: bool = True,
    max_lines_per_file: int = 200,
    repo_root: Optional[str] = None,
    base: str = "HEAD~1",
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
    """
    return get_review_context(
        changed_files=changed_files, max_depth=max_depth,
        include_source=include_source, max_lines_per_file=max_lines_per_file,
        repo_root=repo_root or _default_repo_root, base=base,
    )


def main(repo_root: str | None = None) -> None:
    """Run the MCP server via stdio."""
    global _default_repo_root
    _default_repo_root = repo_root
    mcp.run(transport="stdio")


if __name__ == "__main__":
    main()
