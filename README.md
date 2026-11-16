# Code Rosetta

A cross-language code graph and search tool for coding agents, written in Rust.
Parse Python, Terraform/HCL, YAML, Jinja2, Dockerfile, Bash and Go templates (Helm)
into one graph, then query names, source text, references and change impact.

This branch introduces the **2.0 alpha Rust implementation**. It has no
Sourcebot/Saucebot UI or compatibility API. The previous Python source remains in
`code_rosetta/`; existing Python releases and tags are unchanged.

## Build

The Rust 1.98.1 toolchain is pinned in `rust-toolchain.toml`. Git is needed to index
tracked files and synchronize GitLab repositories. SQLite is bundled. Local
SQLite use requires neither a database service nor GitLab credentials.

```sh
cargo build --release --locked
./target/release/rosetta --help
```

## Local quick start

```sh
# Run inside your project. Stores its index in .code-rosetta/rust.db.
rosetta index --path . --name my-project
rosetta search handler
rosetta grep DATABASE_URL
rosetta query callers_of 'my-project/app.py::Function::handler'
rosetta impact --files my-project/app.py
rosetta review-context --files my-project/app.py
rosetta stats
rosetta serve                         # MCP over stdio; logs go to stderr
```

In a Git working tree, indexing uses tracked working-tree files, not untracked
files or just the committed revision. For a plain directory, Rosetta walks regular
files. Re-index to pick up changes; unchanged content reuses its parsed result,
renames/deletions replace the previous repository generation, and publication is
transactional. If another indexer publishes the same repository while it is being
scanned, retry rather than overwrite its generation. Query commands read storage
directly and do not require an HTTP server.

You can index multiple repositories into the same database using distinct names.
Use one explicit database path when indexing from different working directories:

```sh
rosetta --database /absolute/path/graph.db index --path /src/backend --name team/backend
rosetta --database /absolute/path/graph.db index --path /src/infra --name team/infra
rosetta --database /absolute/path/graph.db serve
```

## Choose transport and storage independently

- **Stdio:** SQLite is the default and normal local choice. PostgreSQL also works.
- **HTTP:** PostgreSQL is recommended for shared deployments. SQLite remains
  supported for local/small single-instance use.
- Choosing a transport never changes your database choice. No configuration means
  SQLite, including with HTTP. PostgreSQL is always an explicit selection.

All four combinations:

```sh
rosetta --storage sqlite --database /data/graph.db serve --transport stdio
rosetta --storage sqlite --database /data/graph.db serve --transport http

# Keep credentials out of shell history and process arguments.
export ROSETTA_STORAGE=postgres
export ROSETTA_DATABASE='postgres://rosetta:YOUR_PASSWORD@localhost:5432/rosetta?sslmode=require'
rosetta serve --transport stdio
rosetta serve --transport http
```

[`docs/storage.md`](docs/storage.md) covers configuration, deployment limits and
backup. [`docs/mcp.md`](docs/mcp.md) has client examples and HTTP security guidance.

## MCP tools

Both transports expose the same six read-only tools:

- `search_nodes`: tokenized node-name/YAML-key search; optional kind/language/repo filters.
- `grep_code`: literal or regex source search; repository/file globs and files-only mode.
- `query_graph`: callers, callees, imports, children, tests, inheritance, references,
  file summaries and cross-language edges.
- `get_impact_radius`: bounded dependency traversal; structural `CONTAINS` edges do
  not fan out to unrelated siblings.
- `get_review_context`: impact plus bounded source snippets and optional caller-supplied text.
- `list_graph_stats`: repository, file, node, edge and language counts.

Repository filters are globs: `*` stays in one path segment and `**` crosses
segments. Use qualified targets such as `team/backend/app.py::Function::handler`
when a bare name is ambiguous. Files are addressed as `<repo>/<path>`.

## HTTP

```sh
rosetta serve --transport http --listen 127.0.0.1:3000
```

- MCP Streamable HTTP: `/mcp`.
- Native JSON tool calls: `POST /api/v1/tools/<tool-name>`.
- Stats: `GET /api/v1/stats`; storage health: `GET /healthz`.
- No browser UI, Sourcebot-compatible routes, or built-in user accounts.

**There is no authentication in Rosetta.** Whoever reaches the service can query
its entire index. Bind to loopback, or put an authenticated TLS proxy in front of
it. Non-loopback binding requires `--allow-remote`. Configure `--allowed-host` and
`--allowed-origin` for your deployment; Host/Origin checks are not authentication.

## Optional GitLab synchronization

```sh
# Set GITLAB_TOKEN securely in the environment (read_api and repository-read access).
rosetta sync --url https://gitlab.example.com --group my-team \
  --repos-dir /data/rosetta-repos
```

Synchronizes the group and its subgroups once, using default-branch shallow clones.
Tokens are passed through headers/environment, not saved in clone URLs or CLI
arguments. Dirty or mismatched checkouts are not overwritten. Failed fetches leave
the old index intact and cause a nonzero exit. Schedule this command externally
for periodic refresh. Projects absent from a listing are not automatically deleted;
use `rosetta remove team/project` when appropriate.

## Data and security

Rosetta stores source code in its database and materializes the indexed graph and
text in each serving process's memory. PostgreSQL provides shared durable storage;
it does not make the in-memory query engine distributed. Processes refresh their
snapshot after another indexer publishes a generation.

Credential-like filenames (`.env`, private keys, kubeconfigs, Terraform state,
credential files), symlinks, binaries and files over 1 MB have no stored content.
Vendored/generated directories and lockfiles are excluded. **This is not a secret
scanner:** credentials committed in ordinary source files can still be indexed and
returned by search or review snippets. Protect database files, clones, backups and
HTTP access accordingly. Plain-directory traversal does not apply `.gitignore`.

## Tests

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --locked
python3 tests/transport_matrix.py
python3 scripts/audit_public.py
python3 scripts/license_bundle.py target/third-party
```

The default tests exercise SQLite; the explicitly ignored PostgreSQL contract
requires a disposable test database. To run the complete storage/transport matrix:

```sh
export ROSETTA_TEST_POSTGRES_URL='postgres://rosetta:test-only@127.0.0.1:5432/rosetta_test?sslmode=disable'
cargo test -p rosetta --test storage_graph --locked -- --ignored
python3 tests/transport_matrix.py
```

Never point these tests at production data. The contract tests create and remove
synthetic indexed repositories. Public CI runs both stores and both transports.

## Migration and licensing

- [`docs/migration.md`](docs/migration.md): Python-to-Rust changes and re-indexing.
- [`THIRD_PARTY.md`](THIRD_PARTY.md): source provenance and third-party notices.
- [`docs/public-rust-plan.md`](docs/public-rust-plan.md): the agreed scope and acceptance checks.

Rosetta's original code and Rust implementation are MIT licensed; see [`LICENSE`](LICENSE).
Third-party dependencies retain their own licenses. The Rust MCP transport uses
`rmcp`, the official Rust MCP SDK, rather than Sourcebot/Saucebot transport code.
