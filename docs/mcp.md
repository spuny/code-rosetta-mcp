# MCP clients and HTTP deployment

Both transports use the same six native Rosetta tools, schemas, validation and
query engine. Serving is read-only; index repositories separately with the CLI.
No Sourcebot-compatible tool catalog or browser UI is included.

## Stdio with SQLite

Example client configuration; adapt the paths and wrapper format to your client:

```json
{
  "mcpServers": {
    "rosetta": {
      "command": "/absolute/path/to/rosetta",
      "args": ["--storage", "sqlite", "--database", "/absolute/path/graph.db", "serve", "--transport", "stdio"]
    }
  }
}
```

Index into that same database before starting the client. Rosetta talks directly
to its selected storage backend; stdio does not start or proxy an HTTP listener.
Stdout is newline-delimited MCP JSON-RPC only, diagnostics go to stderr, and stdin
EOF or a termination signal ends the process. `RUST_LOG` controls diagnostics.

For PostgreSQL, set `ROSETTA_STORAGE=postgres` and `ROSETTA_DATABASE` in the client's
protected environment and use `args: ["serve", "--transport", "stdio"]`. The URL
must refer to a database the client process can reach. Never commit credentials in
an MCP configuration file.

## HTTP with either database

```sh
rosetta --storage sqlite --database /data/graph.db serve --transport http
# Or configure ROSETTA_STORAGE=postgres and ROSETTA_DATABASE, then:
rosetta serve --transport http --listen 127.0.0.1:3000
```

Connect a Streamable HTTP-capable MCP client to `http://127.0.0.1:3000/mcp`.
The SDK negotiates the protocol and handles initialization, legacy sessions,
notifications and cancellation. Follow the negotiated protocol's headers; legacy
clients must return the `Mcp-Session-Id` they receive on initialization.

Rosetta has a 30-second tool budget, 16 concurrent tool computations and bounded
request/result sizes. Tool errors are MCP results with `isError: true`; protocol
errors remain JSON-RPC errors. Cancellation stops waiting for the tool; bounded
blocking computations already running may finish in the background and keep their
concurrency slot until completion.

## Remote access

**No built-in authentication or TLS termination is provided.** Every reachable
client can search all indexed repositories. Do not expose the service directly to
an untrusted network. Put it behind an authenticated TLS proxy, restrict network
access, and protect the database and repository clones.

For a service behind such a boundary, explicitly acknowledge remote listening and
set its expected host/origin:

```sh
rosetta serve --transport http --listen 0.0.0.0:3000 --allow-remote \
  --allowed-host rosetta.example.com \
  --allowed-origin https://rosetta.example.com
```

Repeat those options for additional allowed hosts/origins. Loopback hosts are the
default allow-list; an explicit list replaces it. Browser requests with an Origin
header are rejected unless explicitly allowed. Requests without Origin are allowed
if Host is valid. These checks mitigate DNS rebinding and cross-origin access;
they are not authentication. The proxy must preserve the expected Host header and
handle authentication before forwarding requests.

Legacy HTTP MCP sessions live in process memory. If several Rosetta instances are
behind a load balancer, use session affinity for legacy clients; sharing PostgreSQL
does not share MCP sessions.

## Native API

The HTTP transport also exposes:

- `POST /api/v1/tools/<name>` with a JSON arguments object: native tool result JSON.
- `GET /api/v1/stats`: global graph statistics.
- `GET /healthz`: storage availability.

Host/Origin validation applies to the native API too. MCP and native API calls use
the same result limits and backend. No indexing or filesystem mutation endpoint is
exposed. Paths such as `/search`, `/browse/*` and `/api/repos` are deliberately absent.
