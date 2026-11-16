# Storage

Storage selection is independent of MCP transport. SQLite/stdin-stdout,
SQLite/HTTP, PostgreSQL/stdin-stdout and PostgreSQL/HTTP are supported.

## Configuration

Global options work before or after the command:

- `--storage sqlite|postgres`, or `ROSETTA_STORAGE`; default: `sqlite`.
- `--database <file-or-url>`, or `ROSETTA_DATABASE`.
- SQLite default: `.code-rosetta/rust.db`, relative to the current working directory.
- PostgreSQL requires an explicit connection URL; it is never selected or provisioned
  just because the HTTP transport is selected.

Use the same database configuration for indexing, serving and CLI queries. An
absolute SQLite path is useful when clients launch from different directories.
Keep PostgreSQL credentials in a protected environment/configuration mechanism,
not in committed files, command-line arguments or shell history.

## SQLite

Recommended for local stdio use; suitable for small, single-instance HTTP setups.
SQLite is compiled into the binary. Rosetta creates the parent directory and uses
WAL mode, foreign keys, a busy timeout and one connection per process. Newly created
files use owner-only permissions on Unix; protect the containing directory too.

Concurrent readers and an index writer can use the same local database. Writes
serialize; a busy timeout is an error, not permission to discard a generation.
Do not share a SQLite file over a network filesystem or use it as a multi-host
service database. Use PostgreSQL for those deployments. Each serving process still
needs enough memory for its materialized graph and source text.

For backup, stop all Rosetta writers and use SQLite's backup API or the `sqlite3`
CLI `.backup` command. Copying only the `.db` file while WAL writes are active is
not a safe backup. Restore with Rosetta stopped and verify file/directory permissions.

## PostgreSQL

Recommended for a shared HTTP service, concurrent indexers and multiple service
processes. It is also supported with stdio. Create a dedicated database and role;
Rosetta does not create database users or databases. The role needs permission to
create and migrate its tables, and read/write indexed repository data.

For a disposable local development database, `compose.yaml` provides PostgreSQL 17:

```sh
# Set POSTGRES_PASSWORD securely first; the file contains no default password.
docker compose up -d postgres
export ROSETTA_STORAGE=postgres
# Set ROSETTA_DATABASE to the matching local URL, with sslmode=disable only for local testing.
rosetta migrate
rosetta index --path /src/my-project --name my-project
rosetta serve --transport http
```

For remote databases, configure TLS and certificate validation appropriate to your
PostgreSQL deployment (`sslmode=verify-full` and `sslrootcert` where applicable).
Use the connection URL parameters supported by SQLx's PostgreSQL driver. A failed
connection never falls back to SQLite. The pool has at most eight connections per
process, with connection-acquisition, statement and lock timeouts.

Use `pg_dump`/`pg_restore` or your managed database's backup system. Backups contain
indexed source text; handle them as sensitively as the original repositories.

## Schema, consistency and updates

The public Rust schema is versioned separately from the Python SQLite and internal
Rust PostgreSQL schemas. Both public storage backends use the same portable schema,
with backend-specific connection, locking and transaction setup. Startup applies
known migrations transactionally and rejects unsupported schema versions.

Repository publication updates metadata, files and the global revision in one
transaction. Unchanged files reuse their parsed result during scanning; the stored
repository generation is replaced atomically, removing deleted/renamed paths.
Optimistic generation checks reject a stale concurrent publication. Readers load
a consistent snapshot and rebuild their cache when the revision changes, including
updates made by a different process.

No in-place conversion of existing Python/internal indexes, or automatic
SQLite-to-PostgreSQL migration, is provided. Select an empty public Rust database
and re-index the repositories. Do not point the public binary at the internal
application's production database.
