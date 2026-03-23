# Code Rosetta

Cross-language codebase graph for LLM agents. Python, Terraform/HCL, YAML, Jinja2 — one unified, queryable map.

**The problem:** LLM coding agents re-read your entire codebase every session. Existing tools like [code-review-graph](https://github.com/tirth8205/code-review-graph) and [codebase-memory-mcp](https://github.com/DeusData/codebase-memory-mcp) handle Python/JS/Go well, but if your stack includes Terraform, YAML configs, and Jinja2 templates — they're blind to most of your code.

**Code Rosetta** parses all four languages into a single knowledge graph with cross-language and cross-repo reference detection. Change a Terraform variable and see which YAML configs, Python scripts, and Jinja templates are affected — across repository boundaries.

## What it does

- **4 language parsers** with a pluggable architecture (add new languages by writing one file)
- **Cross-language edges** — detects Python reading YAML configs, rendering Jinja2 templates, Terraform referencing YAML files
- **Cross-repo blast radius** — indexes multiple repos into one graph, resolves `terraform_remote_state` output references between repos
- **Weighted impact analysis** — structural edges (CONTAINS) don't fan out, only real dependency edges propagate
- **MCP server** for Claude Code integration — query the graph during sessions without reading files
- **Config groups** — organize repos into named groups with separate databases

## Supported languages

| Language | Parser | Node types |
|---|---|---|
| **Python** | tree-sitter | Module, Class, Function, Method, Import |
| **Terraform/HCL** | python-hcl2 | Resource, DataSource, Module, Variable, Output, Provider, Local |
| **YAML** | ruamel.yaml | Document, Section, K8sResource, Anchor |
| **Jinja2** | jinja2 AST | Template, Block, Macro, Variable, Extends, Include |

## Quick start

### Install

```bash
# Requires Python 3.10+ and uv
uv tool install code-rosetta-mcp

# Or with pip
pip install code-rosetta-mcp
```

### Single repo

```bash
cd /path/to/your/repo
code-rosetta build
code-rosetta status
```

### Multi-repo with config groups

```bash
# Create config
code-rosetta init

# Edit ~/.code-rosetta/config.yaml
```

```yaml
default_db: ~/.code-rosetta/graph.db

graphs:
  my-infra:
    db: ~/.code-rosetta/infra.db
    repos:
      - ~/projects/terraform-main
      - ~/projects/terraform-users
      - ~/projects/k8s-configs

  my-app:
    db: ~/.code-rosetta/app.db
    repos:
      - ~/projects/backend
      - ~/projects/frontend
```

```bash
# Build entire group into one graph
code-rosetta build-group my-infra

# List groups
code-rosetta groups

# Single repo build auto-detects its group
cd ~/projects/terraform-main
code-rosetta build  # uses infra.db automatically
```

### Claude Code integration

```bash
cd /path/to/your/repo
code-rosetta install   # creates .mcp.json
code-rosetta build     # build the graph
# Restart Claude Code to pick up the MCP server
```

## CLI commands

| Command | Description |
|---|---|
| `code-rosetta init` | Create config file at `~/.code-rosetta/config.yaml` |
| `code-rosetta install` | Set up `.mcp.json` for Claude Code MCP integration |
| `code-rosetta build` | Full graph build — parse all files |
| `code-rosetta build-group <name>` | Build all repos in a config group |
| `code-rosetta update` | Incremental update — only changed files |
| `code-rosetta status` | Show graph statistics |
| `code-rosetta groups` | List configured graph groups |
| `code-rosetta serve` | Start MCP server (stdio transport) |

All commands accept `--repo` to specify the repository root and `--db` to override the database path.

## MCP tools

When running as an MCP server, the following tools are available:

| Tool | Description |
|---|---|
| `build_or_update_graph_tool` | Build or incrementally update the graph |
| `get_impact_radius_tool` | Blast radius analysis for changed files |
| `query_graph_tool` | Predefined graph queries (callers, callees, imports, references, cross-language) |
| `search_nodes_tool` | Search nodes by name, kind, or language |
| `list_graph_stats_tool` | Graph statistics |
| `get_review_context_tool` | Token-efficient review context with source snippets |

### Query patterns

The `query_graph_tool` supports these patterns:

- `callers_of` — functions that call the target
- `callees_of` — functions called by the target
- `imports_of` / `importers_of` — import relationships
- `children_of` — nodes contained in a file or class
- `tests_for` — tests for a function or class
- `inheritors_of` — classes inheriting from the target
- `file_summary` — all nodes in a file
- `references_to` — all references to a resource/variable/module
- `cross_language` — all cross-language edges in the graph

## Edge types

### Within a language

| Edge | Meaning |
|---|---|
| `CONTAINS` | File/class contains a symbol (structural, not traversed in blast radius) |
| `CALLS` | Function/method calls another |
| `IMPORTS` | Module imports another |
| `INHERITS` | Class inherits from another |
| `REFERENCES` | Terraform resource references another (via interpolation) |
| `PASSES_VAR` | Terraform module passes a variable |
| `USES_MODULE` | Terraform module uses a local source path |
| `EXTENDS` / `INCLUDES` | Jinja2 template relationships |
| `USES_VARIABLE` / `CALLS_MACRO` | Jinja2 variable and macro usage |

### Cross-language (detected automatically)

| Edge | Meaning |
|---|---|
| `READS_CONFIG` | Python/Terraform reads a YAML config file |
| `RENDERS` | Python/Terraform renders a Jinja2 template |
| `REMOTE_STATE` | Terraform `terraform_remote_state` references outputs from another repo |

## How blast radius works

When you change a file, Code Rosetta traces the impact through the graph using BFS with **edge weighting**:

- **Strong edges** (CALLS, IMPORTS, REFERENCES, REMOTE_STATE, etc.) are always traversed — these represent real data/code dependencies
- **Structural edges** (CONTAINS) are only used for seeding — finding nodes in the changed file — but never for fan-out during traversal

This prevents a change in `main.tf` from falsely impacting every resource in the repo just because they're all contained in files referenced by `main.tf`.

## Architecture

```
code_rosetta/
  parsers/
    __init__.py       # Plugin registry + LanguageParser protocol
    python_parser.py  # tree-sitter
    hcl_parser.py     # python-hcl2
    yaml_parser.py    # ruamel.yaml
    jinja_parser.py   # jinja2 AST
  models.py           # NodeInfo, EdgeInfo — the parser contract
  graph.py            # SQLite-backed graph store + NetworkX traversal
  crossref.py         # Cross-language + cross-repo reference detection
  incremental.py      # Full/incremental build, file hashing, git integration
  config.py           # ~/.code-rosetta/config.yaml management
  tools.py            # MCP tool implementations
  main.py             # FastMCP server
  cli.py              # Click CLI
```

### Adding a new language

1. Create `code_rosetta/parsers/my_parser.py`
2. Implement the `LanguageParser` protocol:

```python
class MyParser:
    language = "mylang"
    extensions = [".ml"]

    def parse(self, file_path: Path, source: bytes) -> tuple[list[NodeInfo], list[EdgeInfo]]:
        # Parse source, return nodes and edges
        ...

my_parser = MyParser()
```

3. Register it in `cli.py`:

```python
from .parsers.my_parser import my_parser
register_parser(my_parser)
```

That's it. The graph store, incremental updates, MCP tools, and blast radius analysis all pick up new languages automatically.

## Storage

- Graph database: SQLite with WAL mode, stored in `.code-rosetta/graph.db` (per-repo) or `~/.code-rosetta/<group>.db` (per-group)
- All data is local — no cloud, no external services
- Incremental updates use SHA-256 file hashing — only changed files are re-parsed

## Known Limitations

- **Go templates (Helm)**: YAML files containing Go template directives (`{{ }}`, `{{- range }}`, etc.) are not parsed. A dedicated Go template parser is planned — it will extract template structure, `.Values` references, `include`/`template` cross-file edges, and static k8s resource metadata. See `feat/go-template-parser` branch.
- **Module output pass-through**: Terraform module-to-module output references within a single repo are not yet tracked as edges.
- **Cross-repo object references**: Beyond `terraform_remote_state`, references like objects.git pointing at cluster resources require both repos in the same named group + parser support for the reference pattern.

## Credits

Graph store and incremental update logic adapted from [code-review-graph](https://github.com/tirth8205/code-review-graph) (MIT license). Parser architecture, cross-language detection, multi-repo support, config groups, and edge weighting are original.

## License

MIT
