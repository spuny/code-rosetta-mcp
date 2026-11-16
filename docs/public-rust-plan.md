# Public Rust Rosetta plan

Status: agreed target design, not implemented in this public repository. The current
public implementation is Python. Commands below illustrate the proposed Rust interface.

## Goal

Publish a standalone Rust code graph and search tool with MCP over stdio or HTTP,
SQLite or PostgreSQL storage, and no Sourcebot/Saucebot UI or compatibility layer.
Do not change the separate internal application or deployment as part of this work.

## Public scope

Keep:

- Python, Terraform/HCL, YAML, Jinja2, Dockerfile, Bash and Go template parsers.
- Graph storage, cross-file and cross-repository references, incremental indexing.
- Node search, text/regex grep, impact analysis and review context.
- Local-directory indexing, optional GitLab synchronization and native CLI/API.
- Six native MCP tools: `search_nodes`, `grep_code`, `query_graph`,
  `get_impact_radius`, `get_review_context` and `list_graph_stats`.

Exclude:

- Sourcebot/Saucebot UI, browser assets and UI-specific tests.
- Sourcebot-compatible HTTP routes, compatibility MCP tools and branding.
- Company deployment manifests, private CI templates and internal documentation.

## Transport and storage are independent

Support all four combinations: stdio with SQLite, stdio with PostgreSQL, HTTP with
SQLite, and HTTP with PostgreSQL. Neither transport forces a storage backend.
An explicit storage choice must never be silently overridden by the transport.

### Storage

- SQLite is the no-configuration local default and the normal choice for stdio.
  It needs no separate database service.
- PostgreSQL is recommended for shared HTTP deployments, particularly where
  concurrent indexing and queries or multiple service processes are needed.
  It is a recommendation, not a requirement.
- HTTP with SQLite remains supported for local or small single-instance use.
  Document write contention, file permissions and the supported deployment model.
- Stdio with PostgreSQL remains supported for users who prefer a managed database.
- Add a storage boundary shared by indexing, graph queries and content search.
  Implement SQLite support; the existing Rust implementation currently uses PostgreSQL.
- Maintain backend-specific migrations and test equivalent graph/search behavior,
  transactional index publication and recovery. Do not assume SQL or search-index
  implementations are interchangeable.
- Document backend configuration, database-file locations, connection URLs,
  backup/restore and concurrency limitations. Do not promise automatic conversion
  of existing Python indexes or migration between storage backends.

### MCP transports

Proposed interface:

```sh
rosetta serve --transport stdio
rosetta serve --transport http --listen 127.0.0.1:3000
```

- Default to stdio. Database selection is configured separately from transport.
- Both transports use the same tool backend, schemas, results and error semantics.
- Stdio calls the backend directly; it must not require a hidden HTTP listener.
- Reserve stdout for MCP messages. Send diagnostics and startup logs to stderr.
- Cover initialization, cancellation, request limits, timeouts, EOF and shutdown.
- HTTP uses MCP Streamable HTTP, retains session and Host/Origin protections,
  and binds to loopback by default.
- Document that Host/Origin checks are not authentication. Shared HTTP access
  needs authentication and TLS at an appropriate boundary; never expose an
  unauthenticated code index to untrusted clients.
- Local indexing must not require GitLab credentials or network access.

## Implementation sequence

1. Inventory the selected Rust sources and their provenance. Confirm publication
   rights and retain required third-party licenses and notices. Removing the UI
   alone does not establish that the remaining code can be published as MIT.
2. Extract reviewed sources onto a feature branch in the existing public project,
   without importing internal Git history. Preserve existing public releases/tags.
3. Remove excluded routes, assets, dependencies and compatibility tool dispatch.
   Give the MCP server its own Rosetta identity.
4. Introduce the storage boundary and SQLite implementation alongside PostgreSQL.
   Run the existing parser, graph and incremental-indexing regressions on both.
5. Separate backend startup from transport startup. Retain native HTTP MCP and add
   stdio against the same backend, with explicit transport selection.
6. Replace private build dependencies with public CI, generic configuration,
   synthetic fixtures, a container build and an optional local PostgreSQL example.
7. Write the Rust README, database setup guides, transport/client examples and
   Python-to-Rust migration notes. Explain recommendations without restricting
   the user's database choice.
8. Audit the exact proposed public tree and new commits for secrets, internal
   references and licensing; prepare binaries, container release workflow and
   release notes. Push, PR creation and publication require separate approval.

## Acceptance checks

- Build and test without company infrastructure or private dependencies.
- Run parser, graph, search, incremental-indexing and recovery tests on both stores.
- Exercise every native MCP tool through both transports on both stores and
  compare semantic results, including errors and limits.
- Test stdio as a real subprocess: protocol-only stdout, diagnostics on stderr,
  cancellation and clean exit on stdin EOF, with no listening network socket.
- Test HTTP initialization, sessions, cancellation, limits, Host/Origin validation
  and graceful shutdown.
- Verify documentation examples for all four transport/storage combinations.
- Confirm the public package contains no excluded UI/compatibility code, private
  build dependencies, credentials or uncleared third-party material.
