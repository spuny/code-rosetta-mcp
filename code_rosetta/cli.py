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
@click.option("--repo", default=None, help="Repository root (auto-detected)")
@click.option("--db", default=None, help="Path to graph database (overrides config)")
def serve(repo, db):
    """Start MCP server (stdio transport)."""
    _ensure_parsers()
    from .main import main as serve_main
    serve_main(repo_root=repo)
