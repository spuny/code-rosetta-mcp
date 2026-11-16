# Source provenance and third-party notices

## Rosetta Rust extraction

The Rust model, seven parsers and parser regression fixtures were selected from
Code Rosetta's Rust rewrite at commit `459991ca2a3e3bdfd3dc10ba2e71fa98e3c3d446`.
The pure reference-resolution index/path helpers were retained from its graph
linker; persistence, graph query orchestration, service startup, logging setup,
GitLab synchronization and MCP transport integration were implemented separately
for this public version. The author has designated their Rosetta code as MIT.

No internal repository history, deployment configuration or CI templates are
imported. Sourcebot/Saucebot UI assets, Sourcebot-compatible API/model/tool modules,
generated language tables, PostgreSQL database/pool code and adapted MCP transport
sources are excluded. Removing UI assets alone would not establish this boundary.
No Sourcebot UI/icon bundle or FSL-licensed UI material is distributed here.

## Earlier Python sources

Graph storage and incremental-update logic in the retained Python implementation
were adapted from [code-review-graph](https://github.com/tirth8205/code-review-graph),
MIT licensed, copyright 2026 Tirth Kanani. Its complete notice is retained in
[`LICENSES/code-review-graph-MIT.txt`](LICENSES/code-review-graph-MIT.txt).

## Rust dependencies

Dependencies retain their respective licenses; Rosetta's MIT license does not
relicense them. Cargo's lockfile records the exact source packages used to build.
Important components include:

- `rmcp`: the official Rust MCP SDK, Apache-2.0. Rosetta integrates its transport
  and protocol handling as a dependency rather than copying its source.
- `sqlx`: MIT OR Apache-2.0; SQLite/PostgreSQL persistence and TLS connections.
- `tree-sitter`, `tree-sitter-python`, `tree-sitter-bash`: parser runtime/grammars.
- `hcl-edit` and `saphyr-parser`: HCL/YAML parsing.
- `regex`, `globset`, `tokio`, `axum`, `reqwest` and their transitive dependencies.

`scripts/license_bundle.py` collects dependency license texts for the resolved
build, including nested notices. CI and artifact/container builds run it. It fails
on missing texts rather than silently shipping an incomplete bundle.

Some published crates omit their root license text. Version-specific supplements
are kept in `LICENSES/dependencies`:

- `tree-sitter` 0.25.10: root MIT license from source commit
  `da6fe9beb4f7f67beb75914ca8e0d48ae48d6406`; its packaged Unicode notice is also bundled.
- `crc-catalog` 2.5.0: MIT option from `LICENSES/MIT.txt` at source commit
  `ed4ad631f22b05055c21a3a4127eb7cf6d75bb62` (MIT OR Apache-2.0).
- `pratt` 0.4.0: its manifest declares MIT but source commit
  `4a00d5eaf44aabc0856ae4e1d05fcd8eab646a94` has no standalone license text.
  The supplement explicitly records that fact, the manifest's author attribution,
  and standard MIT terms; it does not pretend to be an upstream copyright notice.
  Its manifest is included as evidence of the upstream declaration.

Include the generated bundle with distributed artifacts. This source inventory
is not a substitute for those dependency license texts.
