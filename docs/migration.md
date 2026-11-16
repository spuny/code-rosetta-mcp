# Migrating from Python Rosetta

The Rust branch is a 2.0 alpha, not an in-place upgrade of Python's database format.
Keep the existing Python binary/index until you have checked the new results.

1. Build Rust with `cargo build --release --locked`.
2. Select a new SQLite file or an empty dedicated PostgreSQL database.
3. Index each repository with an explicit name. Use the same database for every
   command and MCP client; existing Python config groups are not imported.
4. Replace the MCP executable/arguments or HTTP endpoint in your client.
5. Compare graph/search/review results before retiring the old setup.

## Important changes

- Binary: `rosetta` rather than `code-rosetta`. Installing this binary does not
  overwrite an existing Python tool installation.
- Default transport: stdio. HTTP is opt-in with `serve --transport http`.
- Storage: SQLite is the default; PostgreSQL is independently selectable.
- Database: `.code-rosetta/rust.db` by default, not Python's `graph.db`.
- Native MCP names have no `_tool` suffix. The six names are listed in the README.
  Indexing is a CLI operation; no `build_or_update_graph_tool` is exposed.
- Index a group of repositories into one selected database. `group` is accepted
  and ignored for legacy MCP requests; `repo_root` is rejected rather than reading
  an arbitrary client-supplied filesystem path.
- Qualified symbol names include their kind, for example
  `team/project/app.py::Function::handler`; files are `team/project/app.py`.
- Ambiguous targets require a qualified name. Cross-repository package/state links
  are resolved only when their target is unambiguous.
- Git repositories index tracked working-tree files. Dirty working trees do not
  advertise a commit ref for their source snippets.
- Query CLI commands return JSON directly from the selected storage, without a
  running HTTP service. Current Rust CLI options are listed by `--help`; Python's
  full command/flag surface is not preserved.
- The Rust native HTTP API is `/api/v1/tools/<tool-name>` plus stats/health. It is
  not a drop-in replacement for Sourcebot or the previous Python HTTP server.
- GitLab sync is one-shot and explicitly configured; schedule it externally.
- Browser UI and visualization commands are not included in this public extraction.

The original Python sources, Git history and release tags remain available.
Python-specific MIT attribution continues to apply to those sources.
