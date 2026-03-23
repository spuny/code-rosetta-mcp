"""MCP tool definitions for Code Rosetta.

Exposes tools for building, querying, and analyzing cross-language codebases.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

from .graph import GraphStore, edge_to_dict, node_to_dict
from .incremental import (
    find_project_root,
    full_build,
    get_changed_files,
    get_db_path,
    get_staged_and_unstaged,
    incremental_update,
)


def _validate_repo_root(path: Path) -> Path:
    resolved = path.resolve()
    if not resolved.is_dir():
        raise ValueError(f"repo_root is not an existing directory: {resolved}")
    if not (resolved / ".git").exists() and not (resolved / ".code-rosetta").exists():
        raise ValueError(
            f"repo_root does not look like a project root (no .git or "
            f".code-rosetta directory found): {resolved}"
        )
    return resolved


def _get_store(repo_root: str | None = None) -> tuple[GraphStore, Path]:
    root = _validate_repo_root(Path(repo_root)) if repo_root else find_project_root()
    db_path = get_db_path(root)
    return GraphStore(db_path), root


# --- Tool 1: build_or_update_graph ---

def build_or_update_graph(
    full_rebuild: bool = False,
    repo_root: str | None = None,
    base: str = "HEAD~1",
) -> dict[str, Any]:
    """Build or incrementally update the code knowledge graph."""
    store, root = _get_store(repo_root)
    try:
        if full_rebuild:
            result = full_build(root, store)
            return {
                "status": "ok",
                "build_type": "full",
                "summary": (
                    f"Full build: {result['files_parsed']} files, "
                    f"{result['total_nodes']} nodes, {result['total_edges']} edges"
                    f" ({result.get('cross_ref_edges', 0)} cross-language)"
                ),
                **result,
            }
        else:
            result = incremental_update(root, store, base=base)
            if result["files_updated"] == 0:
                return {
                    "status": "ok",
                    "build_type": "incremental",
                    "summary": "No changes detected. Graph is up to date.",
                    **result,
                }
            return {
                "status": "ok",
                "build_type": "incremental",
                "summary": (
                    f"Incremental: {result['files_updated']} files updated, "
                    f"{result['total_nodes']} nodes, {result['total_edges']} edges."
                ),
                **result,
            }
    finally:
        store.close()


# --- Tool 2: get_impact_radius ---

def get_impact_radius(
    changed_files: list[str] | None = None,
    max_depth: int = 2,
    max_results: int = 500,
    repo_root: str | None = None,
    base: str = "HEAD~1",
) -> dict[str, Any]:
    """Analyze the blast radius of changed files."""
    store, root = _get_store(repo_root)
    try:
        if changed_files is None:
            changed_files = get_changed_files(root, base)
            if not changed_files:
                changed_files = get_staged_and_unstaged(root)

        if not changed_files:
            return {"status": "ok", "summary": "No changed files detected.",
                    "changed_nodes": [], "impacted_nodes": [], "impacted_files": []}

        abs_files = [str(root / f) for f in changed_files]
        result = store.get_impact_radius(abs_files, max_depth=max_depth, max_nodes=max_results)

        changed_dicts = [node_to_dict(n) for n in result["changed_nodes"]]
        impacted_dicts = [node_to_dict(n) for n in result["impacted_nodes"]]
        edge_dicts = [edge_to_dict(e) for e in result["edges"]]

        summary_parts = [
            f"Blast radius for {len(changed_files)} changed file(s):",
            f"  {len(changed_dicts)} nodes directly changed",
            f"  {len(impacted_dicts)} nodes impacted (within {max_depth} hops)",
            f"  {len(result['impacted_files'])} additional files affected",
        ]

        return {
            "status": "ok",
            "summary": "\n".join(summary_parts),
            "changed_files": changed_files,
            "changed_nodes": changed_dicts,
            "impacted_nodes": impacted_dicts,
            "impacted_files": result["impacted_files"],
            "edges": edge_dicts,
        }
    finally:
        store.close()


# --- Tool 3: query_graph ---

_QUERY_PATTERNS = {
    "callers_of": "Find all functions that call a given function",
    "callees_of": "Find all functions called by a given function",
    "imports_of": "Find all imports of a given file or module",
    "importers_of": "Find all files that import a given file or module",
    "children_of": "Find all nodes contained in a file or class",
    "tests_for": "Find all tests for a given function or class",
    "inheritors_of": "Find all classes that inherit from a given class",
    "file_summary": "Get a summary of all nodes in a file",
    "references_to": "Find all references to a resource/variable/module",
    "cross_language": "Find all cross-language edges in the graph",
}


def query_graph(
    pattern: str,
    target: str = "",
    repo_root: str | None = None,
) -> dict[str, Any]:
    """Run a predefined graph query."""
    store, root = _get_store(repo_root)
    try:
        if pattern not in _QUERY_PATTERNS:
            return {
                "status": "error",
                "error": f"Unknown pattern '{pattern}'. Available: {list(_QUERY_PATTERNS.keys())}",
            }

        results: list[dict] = []
        edges_out: list[dict] = []

        # Resolve target
        node = store.get_node(target)
        if not node:
            abs_target = str(root / target)
            node = store.get_node(abs_target)
        if not node and target:
            candidates = store.search_nodes(target, limit=5)
            if len(candidates) == 1:
                node = candidates[0]
                target = node.qualified_name
            elif len(candidates) > 1:
                return {
                    "status": "ambiguous",
                    "summary": f"Multiple matches for '{target}'. Please use a qualified name.",
                    "candidates": [node_to_dict(c) for c in candidates],
                }

        if not node and pattern not in ("file_summary", "cross_language"):
            return {"status": "not_found", "summary": f"No node found matching '{target}'."}

        qn = node.qualified_name if node else target

        if pattern == "callers_of":
            for e in store.get_edges_by_target(qn):
                if e.kind == "CALLS":
                    caller = store.get_node(e.source_qualified)
                    if caller:
                        results.append(node_to_dict(caller))
                    edges_out.append(edge_to_dict(e))

        elif pattern == "callees_of":
            for e in store.get_edges_by_source(qn):
                if e.kind == "CALLS":
                    callee = store.get_node(e.target_qualified)
                    if callee:
                        results.append(node_to_dict(callee))
                    edges_out.append(edge_to_dict(e))

        elif pattern == "imports_of":
            for e in store.get_edges_by_source(qn):
                if e.kind in ("IMPORTS", "IMPORTS_FROM"):
                    results.append({"import_target": e.target_qualified})
                    edges_out.append(edge_to_dict(e))

        elif pattern == "importers_of":
            abs_target = str(root / target) if node is None else node.file_path
            for e in store.get_edges_by_target(abs_target):
                if e.kind in ("IMPORTS", "IMPORTS_FROM"):
                    results.append({"importer": e.source_qualified, "file": e.file_path})
                    edges_out.append(edge_to_dict(e))

        elif pattern == "children_of":
            for e in store.get_edges_by_source(qn):
                if e.kind == "CONTAINS":
                    child = store.get_node(e.target_qualified)
                    if child:
                        results.append(node_to_dict(child))

        elif pattern == "tests_for":
            for e in store.get_edges_by_target(qn):
                if e.kind == "TESTED_BY":
                    test = store.get_node(e.source_qualified)
                    if test:
                        results.append(node_to_dict(test))
            name = node.name if node else target
            for t in store.search_nodes(f"test_{name}", limit=10):
                if t.is_test and t.qualified_name not in {r.get("qualified_name") for r in results}:
                    results.append(node_to_dict(t))

        elif pattern == "inheritors_of":
            for e in store.get_edges_by_target(qn):
                if e.kind in ("INHERITS", "IMPLEMENTS"):
                    child = store.get_node(e.source_qualified)
                    if child:
                        results.append(node_to_dict(child))
                    edges_out.append(edge_to_dict(e))

        elif pattern == "file_summary":
            abs_path = str(root / target)
            for n in store.get_nodes_by_file(abs_path):
                results.append(node_to_dict(n))

        elif pattern == "references_to":
            for e in store.get_edges_by_target(qn):
                if e.kind in ("REFERENCES", "READS_CONFIG", "RENDERS", "PASSES_VAR", "USES_MODULE"):
                    referrer = store.get_node(e.source_qualified)
                    if referrer:
                        results.append(node_to_dict(referrer))
                    edges_out.append(edge_to_dict(e))

        elif pattern == "cross_language":
            xref_edges = store.get_cross_language_edges()
            for e in xref_edges:
                edges_out.append(edge_to_dict(e))
            results = [{"total_cross_language_edges": len(xref_edges)}]

        return {
            "status": "ok",
            "pattern": pattern,
            "target": target,
            "description": _QUERY_PATTERNS[pattern],
            "summary": f"Found {len(results)} result(s) for {pattern}('{target}')",
            "results": results,
            "edges": edges_out,
        }
    finally:
        store.close()


# --- Tool 4: search_nodes ---

def search_nodes(
    query: str,
    kind: str | None = None,
    language: str | None = None,
    limit: int = 20,
    repo_root: str | None = None,
) -> dict[str, Any]:
    """Search for nodes by name, optionally filtered by kind and language."""
    store, root = _get_store(repo_root)
    try:
        results = store.search_nodes(query, limit=limit * 2)
        if kind:
            results = [r for r in results if r.kind == kind]
        if language:
            results = [r for r in results if r.language == language]
        results = results[:limit]

        return {
            "status": "ok",
            "query": query,
            "summary": f"Found {len(results)} node(s) matching '{query}'"
            + (f" (kind={kind})" if kind else "")
            + (f" (language={language})" if language else ""),
            "results": [node_to_dict(r) for r in results],
        }
    finally:
        store.close()


# --- Tool 5: list_graph_stats ---

def list_graph_stats(repo_root: str | None = None) -> dict[str, Any]:
    """Get aggregate statistics about the knowledge graph."""
    store, root = _get_store(repo_root)
    try:
        stats = store.get_stats()

        summary_parts = [
            f"Graph statistics for {root.name}:",
            f"  Files: {stats.files_count}",
            f"  Total nodes: {stats.total_nodes}",
            f"  Total edges: {stats.total_edges}",
            f"  Languages: {', '.join(stats.languages) if stats.languages else 'none'}",
            f"  Last updated: {stats.last_updated or 'never'}",
            "",
            "Nodes by kind:",
        ]
        for kind, count in sorted(stats.nodes_by_kind.items()):
            summary_parts.append(f"  {kind}: {count}")
        summary_parts.append("")
        summary_parts.append("Edges by kind:")
        for kind, count in sorted(stats.edges_by_kind.items()):
            summary_parts.append(f"  {kind}: {count}")

        return {
            "status": "ok",
            "summary": "\n".join(summary_parts),
            "total_nodes": stats.total_nodes,
            "total_edges": stats.total_edges,
            "nodes_by_kind": stats.nodes_by_kind,
            "edges_by_kind": stats.edges_by_kind,
            "languages": stats.languages,
            "files_count": stats.files_count,
            "last_updated": stats.last_updated,
        }
    finally:
        store.close()


# --- Tool 6: get_review_context ---

def get_review_context(
    changed_files: list[str] | None = None,
    max_depth: int = 2,
    include_source: bool = True,
    max_lines_per_file: int = 200,
    repo_root: str | None = None,
    base: str = "HEAD~1",
) -> dict[str, Any]:
    """Generate a focused review context from changed files."""
    store, root = _get_store(repo_root)
    try:
        if changed_files is None:
            changed_files = get_changed_files(root, base)
            if not changed_files:
                changed_files = get_staged_and_unstaged(root)

        if not changed_files:
            return {"status": "ok", "summary": "No changes detected.", "context": {}}

        abs_files = [str(root / f) for f in changed_files]
        impact = store.get_impact_radius(abs_files, max_depth=max_depth)

        context: dict[str, Any] = {
            "changed_files": changed_files,
            "impacted_files": impact["impacted_files"],
            "graph": {
                "changed_nodes": [node_to_dict(n) for n in impact["changed_nodes"]],
                "impacted_nodes": [node_to_dict(n) for n in impact["impacted_nodes"]],
                "edges": [edge_to_dict(e) for e in impact["edges"]],
            },
        }

        if include_source:
            snippets = {}
            for rel_path in changed_files:
                full_path = root / rel_path
                if full_path.is_file():
                    try:
                        lines = full_path.read_text(errors="replace").splitlines()
                        if len(lines) > max_lines_per_file:
                            snippets[rel_path] = "\n".join(
                                f"{i+1}: {line}" for i, line in enumerate(lines[:max_lines_per_file])
                            ) + f"\n... ({len(lines) - max_lines_per_file} more lines)"
                        else:
                            snippets[rel_path] = "\n".join(
                                f"{i+1}: {line}" for i, line in enumerate(lines)
                            )
                    except (OSError, UnicodeDecodeError):
                        snippets[rel_path] = "(could not read file)"
            context["source_snippets"] = snippets

        return {
            "status": "ok",
            "summary": f"Review context for {len(changed_files)} file(s), "
                       f"{len(impact['impacted_nodes'])} impacted nodes",
            "context": context,
        }
    finally:
        store.close()
