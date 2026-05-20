"""CLI entry point for code-rosetta."""

from __future__ import annotations

import json
import logging
import sys
from pathlib import Path

import click

from . import __version__


def _ensure_parsers():
    """Register all parsers. Called once at CLI entry."""
    from .parsers import register_parser
    from .parsers.python_parser import python_parser
    from .parsers.hcl_parser import hcl_parser
    from .parsers.yaml_parser import yaml_parser
    from .parsers.jinja_parser import jinja2_parser

    register_parser(python_parser)
    register_parser(hcl_parser)
    register_parser(yaml_parser)
    register_parser(jinja2_parser)


def _resolve_db(repo: str | None = None, db: str | None = None) -> tuple[Path, Path | None]:
    """Resolve the database path using config, flags, or defaults.

    Returns (db_path, repo_root_or_None).
    Priority: --db flag > config group > config default_db > repo-local
    """
    from .config import cfg
    from .incremental import find_project_root, get_db_path

    if db:
        db_path = Path(db).resolve()
        db_path.parent.mkdir(parents=True, exist_ok=True)
        repo_root = Path(repo) if repo else find_project_root()
        return db_path, repo_root

    repo_root = Path(repo) if repo else find_project_root()

    if cfg.exists:
        resolved = cfg.resolve_db_for_repo(repo_root)
        resolved.parent.mkdir(parents=True, exist_ok=True)
        return resolved, repo_root

    return get_db_path(repo_root), repo_root


@click.group(invoke_without_command=True)
@click.option("-v", "--version", is_flag=True, help="Show version and exit")
@click.pass_context
def main(ctx, version):
    """Code Rosetta — cross-language codebase graph for LLM agents."""
    if version:
        click.echo(f"code-rosetta {__version__}")
        return
    if ctx.invoked_subcommand is None:
        click.echo(f"""
  ╔══════════════════════════════════════╗
  ║         Code Rosetta  v{__version__}        ║
  ║  Cross-language codebase graph for   ║
  ║  token-efficient code understanding  ║
  ╚══════════════════════════════════════╝

  Commands:
    init        Create config file (~/.code-rosetta/config.yaml)
    install     Set up Claude Code MCP integration
    build       Full graph build (parse all files)
    build-group Build all repos in a config group
    update      Incremental update (changed files only)
    status      Show graph statistics
    groups      List configured groups
    serve       Start MCP server

  Run: code-rosetta <command> --help
""")


@main.command()
def init():
    """Create a default config file at ~/.code-rosetta/config.yaml."""
    from .config import init_config, _CONFIG_FILE

    if _CONFIG_FILE.exists():
        click.echo(f"Config already exists: {_CONFIG_FILE}")
        click.echo("Edit it to add your repos and groups.")
        return

    path = init_config()
    click.echo(f"Created config: {path}")
    click.echo("Edit it to add your repos and groups, then run:")
    click.echo("  code-rosetta build-group <group-name>")


@main.command()
@click.option("--repo", default=None, help="Repository root (auto-detected)")
def install(repo):
    """Set up .mcp.json for Claude Code integration."""
    from .incremental import find_repo_root

    repo_root = Path(repo) if repo else find_repo_root()
    if not repo_root:
        repo_root = Path.cwd()

    mcp_path = repo_root / ".mcp.json"
    mcp_config = {
        "mcpServers": {
            "code-rosetta": {
                "command": "uvx",
                "args": ["code-rosetta-mcp", "serve"],
            }
        }
    }

    if mcp_path.exists():
        try:
            existing = json.loads(mcp_path.read_text())
            if "code-rosetta" in existing.get("mcpServers", {}):
                click.echo(f"Already configured in {mcp_path}")
                return
            existing.setdefault("mcpServers", {}).update(mcp_config["mcpServers"])
            mcp_config = existing
        except (json.JSONDecodeError, KeyError, TypeError):
            click.echo(f"Warning: existing {mcp_path} has issues, overwriting.")

    mcp_path.write_text(json.dumps(mcp_config, indent=2) + "\n")
    click.echo(f"Created {mcp_path}")
    click.echo()
    click.echo("Next steps:")
    click.echo("  1. code-rosetta build    # build the knowledge graph")
    click.echo("  2. Restart Claude Code   # to pick up the MCP server")


@main.command()
@click.option("--repo", default=None, help="Repository root (auto-detected)")
@click.option("--db", default=None, help="Path to graph database (overrides config)")
def build(repo, db):
    """Full graph build — parse all files.

    Database resolution: --db flag > config group > config default_db > repo-local
    """
    _ensure_parsers()
    logging.basicConfig(level=logging.INFO, format="%(levelname)s: %(message)s")

    from .config import cfg
    from .graph import GraphStore
    from .incremental import full_build

    db_path, repo_root = _resolve_db(repo, db)

    # Show which db is being used if config is active
    group_name = cfg.find_group_for_repo(repo_root) if cfg.exists else None
    if group_name:
        click.echo(f"Using group '{group_name}' database: {db_path}")

    with GraphStore(db_path) as store:
        result = full_build(repo_root, store)
        click.echo(
            f"Full build: {result['files_parsed']} files, "
            f"{result['total_nodes']} nodes, {result['total_edges']} edges"
            f" ({result.get('cross_ref_edges', 0)} cross-language)"
        )
        if result["errors"]:
            click.echo(f"Errors: {len(result['errors'])}")
            for err in result["errors"][:5]:
                click.echo(f"  {err['file']}: {err['error']}")


@main.command("build-group")
@click.argument("group_name")
def build_group(group_name):
    """Build all repos in a config group.

    Usage: code-rosetta build-group quantlane
    """
    _ensure_parsers()
    logging.basicConfig(level=logging.INFO, format="%(levelname)s: %(message)s")

    from .config import cfg
    from .graph import GraphStore
    from .incremental import full_build

    if not cfg.exists:
        click.echo("No config file found. Run 'code-rosetta init' first.", err=True)
        sys.exit(1)

    group = cfg.get_group(group_name)
    if not group:
        available = cfg.list_groups()
        click.echo(f"Group '{group_name}' not found.", err=True)
        if available:
            click.echo(f"Available groups: {', '.join(available)}", err=True)
        sys.exit(1)

    db_path = cfg.get_group_db(group_name)
    repos = cfg.get_group_repos(group_name)

    if not repos:
        click.echo(f"Group '{group_name}' has no repos configured.", err=True)
        sys.exit(1)

    db_path.parent.mkdir(parents=True, exist_ok=True)
    click.echo(f"Building group '{group_name}' ({len(repos)} repos) -> {db_path}")

    with GraphStore(db_path) as store:
        for repo_root in repos:
            if not repo_root.is_dir():
                click.echo(f"  Skipping {repo_root}: not a directory", err=True)
                continue
            result = full_build(repo_root, store)
            click.echo(
                f"  {repo_root.name}: {result['files_parsed']} files, "
                f"{result['total_nodes']} nodes, {result['total_edges']} edges"
            )

        stats = store.get_stats()
        click.echo(
            f"\nCombined: {stats.total_nodes} nodes, {stats.total_edges} edges, "
            f"{stats.files_count} files, languages: {', '.join(stats.languages)}"
        )

        # Rebuild FTS index after build
        if store._has_fts():
            store.rebuild_fts()
            click.echo("FTS search index rebuilt.")


@main.command()
@click.option("--base", default="HEAD~1", help="Git diff base (default: HEAD~1)")
@click.option("--repo", default=None, help="Repository root (auto-detected)")
@click.option("--db", default=None, help="Path to graph database (overrides config)")
def update(base, repo, db):
    """Incremental update — only changed files."""
    _ensure_parsers()
    logging.basicConfig(level=logging.INFO, format="%(levelname)s: %(message)s")

    from .graph import GraphStore
    from .incremental import incremental_update

    db_path, repo_root = _resolve_db(repo, db)

    if not repo_root:
        click.echo("Not in a git repository. Use 'build' for a full parse.", err=True)
        sys.exit(1)

    with GraphStore(db_path) as store:
        result = incremental_update(repo_root, store, base=base)
        click.echo(
            f"Incremental: {result['files_updated']} files updated, "
            f"{result['total_nodes']} nodes, {result['total_edges']} edges"
        )


@main.command()
@click.option("--repo", default=None, help="Repository root (auto-detected)")
@click.option("--db", default=None, help="Path to graph database (overrides config)")
def status(repo, db):
    """Show graph statistics."""
    _ensure_parsers()

    from .graph import GraphStore

    db_path, _ = _resolve_db(repo, db)

    if not db_path.exists():
        click.echo("No graph found. Run 'code-rosetta build' first.")
        return

    with GraphStore(db_path) as store:
        stats = store.get_stats()
        click.echo(f"Database: {db_path}")
        click.echo(f"Nodes: {stats.total_nodes}")
        click.echo(f"Edges: {stats.total_edges}")
        click.echo(f"Files: {stats.files_count}")
        click.echo(f"Languages: {', '.join(stats.languages) if stats.languages else 'none'}")
        click.echo(f"Last updated: {stats.last_updated or 'never'}")
        click.echo()
        click.echo("Nodes by kind:")
        for kind, count in sorted(stats.nodes_by_kind.items()):
            click.echo(f"  {kind}: {count}")
        click.echo()
        click.echo("Edges by kind:")
        for kind, count in sorted(stats.edges_by_kind.items()):
            click.echo(f"  {kind}: {count}")


@main.command()
def groups():
    """List configured graph groups."""
    from .config import cfg, _CONFIG_FILE

    if not cfg.exists:
        click.echo(f"No config found at {_CONFIG_FILE}")
        click.echo("Run 'code-rosetta init' to create one.")
        return

    group_names = cfg.list_groups()
    if not group_names:
        click.echo("No groups configured.")
        return

    for name in group_names:
        db_path = cfg.get_group_db(name)
        repos = cfg.get_group_repos(name)
        click.echo(f"\n{name}:")
        click.echo(f"  db: {db_path}")
        click.echo(f"  repos ({len(repos)}):")
        for r in repos:
            exists = "✓" if r.is_dir() else "✗"
            click.echo(f"    {exists} {r}")


@main.command()
@click.argument("pattern", type=click.Choice([
    "callers_of", "callees_of", "imports_of", "importers_of",
    "children_of", "tests_for", "inheritors_of", "file_summary",
    "references_to", "cross_language",
]))
@click.argument("target", default="")
@click.option("--group", default=None, help="Config group name")
@click.option("--repo", default=None, help="Repository root (auto-detected)")
@click.option("--json-output", "as_json", is_flag=True, help="Output as JSON")
def query(pattern, target, group, repo, as_json):
    """Run a predefined graph query.

    Patterns: callers_of, callees_of, imports_of, importers_of,
    children_of, tests_for, inheritors_of, file_summary,
    references_to, cross_language
    """
    _ensure_parsers()

    from .tools import query_graph

    result = query_graph(pattern, target=target, repo_root=repo, group=group)
    if as_json:
        click.echo(json.dumps(result, indent=2))
    else:
        click.echo(result["summary"])
        if result.get("status") == "ambiguous":
            for c in result.get("candidates", []):
                click.echo(f"  {c['qualified_name']}  ({c.get('file_path', '?')})")
        else:
            for r in result.get("results", []):
                if isinstance(r, dict) and "qualified_name" in r:
                    click.echo(f"  {r['kind']:12s} {r['qualified_name']}  ({r.get('file_path', '?')}:{r.get('line', '?')})")
                else:
                    click.echo(f"  {r}")


@main.command("review-context")
@click.option("--base", default="HEAD~1", help="Git diff base (default: HEAD~1)")
@click.option("--files", default=None, help="Comma-separated changed files (auto-detected from git)")
@click.option("--depth", default=2, help="Max traversal depth (default: 2)")
@click.option("--no-source", is_flag=True, help="Omit source snippets")
@click.option("--group", default=None, help="Config group name")
@click.option("--repo", default=None, help="Repository root (auto-detected)")
@click.option("--json-output", "as_json", is_flag=True, help="Output as JSON")
def review_context(base, files, depth, no_source, group, repo, as_json):
    """Generate review context for changed files."""
    _ensure_parsers()

    from .tools import get_review_context

    changed = files.split(",") if files else None
    result = get_review_context(changed_files=changed, max_depth=depth,
                                include_source=not no_source, repo_root=repo,
                                base=base, group=group)
    if as_json:
        click.echo(json.dumps(result, indent=2))
    else:
        click.echo(result["summary"])
        ctx = result.get("context", {})
        if ctx.get("impacted_files"):
            click.echo("\nImpacted files:")
            for f in ctx["impacted_files"]:
                click.echo(f"  {f}")


@main.command()
@click.option("--repo", default=None, help="Repository root (auto-detected)")
@click.option("--db", default=None, help="Path to graph database (overrides config)")
def serve(repo, db):
    """Start MCP server (stdio transport)."""
    _ensure_parsers()
    from .main import main as serve_main
    serve_main(repo_root=repo)


@main.command()
@click.argument("query")
@click.option("--group", "-g", default=None, help="Config group to search")
@click.option("--kind", "-k", default=None, help="Filter by node kind (Function, Class, Method, ...)")
@click.option("--language", "-l", default=None, help="Filter by language (python, hcl, yaml, ...)")
@click.option("--limit", "-n", default=20, help="Max results (default 20)")
@click.option("--json-output", is_flag=True, help="Output as JSON")
@click.option("--repo", default=None, help="Repository root (auto-detected)")
@click.option("--db", default=None, help="Path to graph database")
def search(query, group, kind, language, limit, json_output, repo, db):
    """Search nodes by name with ranked results.

    Supports camelCase, snake_case, and prefix matching.
    Uses FTS5 with BM25 ranking when available.

    Examples:
        code-rosetta search getUserName -g quantlane
        code-rosetta search "deploy service" -g quantlane --kind Function
        code-rosetta search collector -g quantlane --language python -n 50
    """
    from .graph import GraphStore

    if group:
        from .config import cfg
        db_path = cfg.get_group_db(group)
        if db_path is None:
            click.echo(f"Unknown group '{group}'.", err=True)
            sys.exit(1)
    else:
        db_path, _ = _resolve_db(repo, db)

    if not db_path.exists():
        click.echo("No graph found. Run 'code-rosetta build' first.", err=True)
        sys.exit(1)

    with GraphStore(db_path) as store:
        results = store.search_nodes(query, limit=limit * 2)
        if kind:
            results = [r for r in results if r.kind == kind]
        if language:
            results = [r for r in results if r.language == language]
        results = results[:limit]

        if json_output:
            from .graph import node_to_dict
            click.echo(json.dumps([node_to_dict(r) for r in results], indent=2))
        else:
            if not results:
                click.echo(f"No results for '{query}'.")
                return
            click.echo(f"Found {len(results)} result(s) for '{query}':\n")
            for r in results:
                loc = f"{r.file_path}:{r.line_start}" if r.line_start else r.file_path
                click.echo(f"  {r.kind:<10} {r.name}")
                click.echo(f"             {loc}")
                if r.language:
                    click.echo(f"             [{r.language}]")
                click.echo()


@main.command("rebuild-fts")
@click.option("--group", "-g", default=None, help="Config group")
@click.option("--repo", default=None, help="Repository root")
@click.option("--db", default=None, help="Path to graph database")
def rebuild_fts(group, repo, db):
    """Rebuild the FTS5 full-text search index.

    Run this after upgrading to enable ranked search,
    or if search results seem stale.
    """
    from .graph import GraphStore

    if group:
        from .config import cfg
        db_path = cfg.get_group_db(group)
        if db_path is None:
            click.echo(f"Unknown group '{group}'.", err=True)
            sys.exit(1)
    else:
        db_path, _ = _resolve_db(repo, db)

    if not db_path.exists():
        click.echo("No graph found. Run 'code-rosetta build' first.", err=True)
        sys.exit(1)

    with GraphStore(db_path) as store:
        if not store._has_fts():
            click.echo("FTS5 not available in this SQLite build.", err=True)
            sys.exit(1)
        store.rebuild_fts()
        stats = store.get_stats()
        click.echo(f"FTS index rebuilt: {stats.total_nodes} nodes indexed.")


@main.command()
@click.option("--group", "-g", default=None, help="Config group")
@click.option("--target", "-t", default=None, help="Node qualified name to visualize neighborhood")
@click.option("--files", "-f", default=None, help="Comma-separated changed files for impact view")
@click.option("--depth", "-d", default=2, help="Max traversal depth (default 2)")
@click.option("--output", "-o", default=None, help="Output HTML file (default: /tmp/rosetta-viz.html)")
@click.option("--no-open", is_flag=True, help="Don't open in browser")
@click.option("--repo", default=None, help="Repository root")
@click.option("--db", default=None, help="Path to graph database")
def viz(group, target, files, depth, output, no_open, repo, db):
    """Generate an interactive graph visualization.

    Without --target or --files, visualizes the full graph structure
    (top-level files and their connections).

    Examples:
        code-rosetta viz -g quantlane -t "path::FunctionName"
        code-rosetta viz -g quantlane -f "path/a.py,path/b.tf" -d 3
        code-rosetta viz -g quantlane -o graph.html
    """
    from .graph import GraphStore, node_to_dict, edge_to_dict
    from .visualize import render_graph_html

    if group:
        from .config import cfg
        db_path = cfg.get_group_db(group)
        if db_path is None:
            click.echo(f"Unknown group '{group}'.", err=True)
            sys.exit(1)
        _, repo_root = _resolve_db(repo, db)
    else:
        db_path, repo_root = _resolve_db(repo, db)

    if not db_path.exists():
        click.echo("No graph found. Run 'code-rosetta build' first.", err=True)
        sys.exit(1)

    with GraphStore(db_path) as store:
        nodes = []
        edges = []
        title = "Code Rosetta Graph"

        if target:
            # Visualize neighborhood of a specific node
            node = store.get_node(target)
            if not node:
                candidates = store.search_nodes(target, limit=5)
                if len(candidates) == 1:
                    node = candidates[0]
                elif len(candidates) > 1:
                    click.echo(f"Ambiguous target '{target}'. Candidates:")
                    for c in candidates:
                        click.echo(f"  {c.qualified_name}")
                    sys.exit(1)
                else:
                    click.echo(f"No node found for '{target}'.", err=True)
                    sys.exit(1)

            title = f"Neighborhood: {node.name}"
            # Collect neighbors up to depth -- include CONTAINS for structure
            visited_qns = {node.qualified_name}
            frontier = {node.qualified_name}
            all_edges = []

            for _ in range(depth):
                next_frontier = set()
                for qn in frontier:
                    for e in store.get_edges_by_source(qn):
                        # Only follow edges where target is a real node
                        target_node = store.get_node(e.target_qualified)
                        if target_node:
                            all_edges.append(e)
                            if e.target_qualified not in visited_qns:
                                visited_qns.add(e.target_qualified)
                                next_frontier.add(e.target_qualified)
                    for e in store.get_edges_by_target(qn):
                        source_node = store.get_node(e.source_qualified)
                        if source_node:
                            all_edges.append(e)
                            if e.source_qualified not in visited_qns:
                                visited_qns.add(e.source_qualified)
                                next_frontier.add(e.source_qualified)
                frontier = next_frontier

            for qn in visited_qns:
                n = store.get_node(qn)
                if n:
                    nodes.append(node_to_dict(n))
            edges = [edge_to_dict(e) for e in all_edges]

        elif files:
            # Impact radius visualization
            file_list = [f.strip() for f in files.split(",")]
            if repo_root:
                abs_files = [str(Path(repo_root) / f) for f in file_list]
            else:
                abs_files = file_list
            result = store.get_impact_radius(abs_files, max_depth=depth)
            title = f"Impact: {', '.join(file_list)}"
            for n in result["changed_nodes"] + result["impacted_nodes"]:
                nodes.append(node_to_dict(n))
            edges = [edge_to_dict(e) for e in result["edges"]]

        else:
            # Overview: top connected nodes
            click.echo("No --target or --files specified. Generating overview...")
            stats = store.get_stats()
            title = f"Code Rosetta Overview ({stats.total_nodes} nodes, {stats.total_edges} edges)"
            # Get nodes with most connections (only those that exist as nodes)
            rows = store._conn.execute(
                "SELECT e.target_qualified, COUNT(*) as cnt FROM edges e "
                "JOIN nodes n ON n.qualified_name = e.target_qualified "
                "WHERE e.kind != 'CONTAINS' "
                "GROUP BY e.target_qualified ORDER BY cnt DESC LIMIT 50"
            ).fetchall()
            hub_qns = set()
            for r in rows:
                qn = r["target_qualified"]
                hub_qns.add(qn)
                n = store.get_node(qn)
                if n:
                    nodes.append(node_to_dict(n))
                # Also add their direct callers/importers for context
                for e in store.get_edges_by_target(qn):
                    if e.kind != "CONTAINS":
                        hub_qns.add(e.source_qualified)
                        src = store.get_node(e.source_qualified)
                        if src and src.qualified_name not in {nd["qualified_name"] for nd in nodes}:
                            nodes.append(node_to_dict(src))
            all_qns = {nd["qualified_name"] for nd in nodes}
            hub_edges = store.get_edges_among(all_qns)
            edges = [edge_to_dict(e) for e in hub_edges if e.kind != "CONTAINS"]

    if not nodes:
        click.echo("No nodes to visualize.")
        return

    out_path = Path(output) if output else Path("/tmp/rosetta-viz.html")
    html = render_graph_html(nodes, edges, title=title)
    out_path.write_text(html)
    click.echo(f"Visualization: {out_path} ({len(nodes)} nodes, {len(edges)} edges)")

    if not no_open:
        import subprocess
        subprocess.run(["open", str(out_path)], check=False)
