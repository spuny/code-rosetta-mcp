"""SQLite-backed knowledge graph storage and query engine.

Adapted from code-review-graph (MIT), extended for cross-language support.
Stores code structure as nodes and edges with flexible string-typed kinds.
"""

from __future__ import annotations

import json
import sqlite3
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Optional

import networkx as nx

from .models import EdgeInfo, NodeInfo

_SCHEMA_SQL = """
CREATE TABLE IF NOT EXISTS nodes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    qualified_name TEXT NOT NULL UNIQUE,
    file_path TEXT NOT NULL,
    line_start INTEGER,
    line_end INTEGER,
    language TEXT,
    parent_name TEXT,
    params TEXT,
    return_type TEXT,
    modifiers TEXT,
    is_test INTEGER DEFAULT 0,
    file_hash TEXT,
    extra TEXT DEFAULT '{}',
    updated_at REAL NOT NULL
);

CREATE TABLE IF NOT EXISTS edges (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,
    source_qualified TEXT NOT NULL,
    target_qualified TEXT NOT NULL,
    file_path TEXT NOT NULL,
    line INTEGER DEFAULT 0,
    extra TEXT DEFAULT '{}',
    updated_at REAL NOT NULL
);

CREATE TABLE IF NOT EXISTS metadata (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_nodes_file ON nodes(file_path);
CREATE INDEX IF NOT EXISTS idx_nodes_kind ON nodes(kind);
CREATE INDEX IF NOT EXISTS idx_nodes_qualified ON nodes(qualified_name);
CREATE INDEX IF NOT EXISTS idx_nodes_language ON nodes(language);
CREATE INDEX IF NOT EXISTS idx_edges_source ON edges(source_qualified);
CREATE INDEX IF NOT EXISTS idx_edges_target ON edges(target_qualified);
CREATE INDEX IF NOT EXISTS idx_edges_kind ON edges(kind);
CREATE INDEX IF NOT EXISTS idx_edges_file ON edges(file_path);
"""

_FTS_SCHEMA_SQL = (
    "CREATE VIRTUAL TABLE IF NOT EXISTS nodes_fts USING fts5"
    '(name, qualified_name, tokenize="unicode61", content=nodes, content_rowid=id)'
)


@dataclass
class GraphNode:
    id: int
    kind: str
    name: str
    qualified_name: str
    file_path: str
    line_start: int
    line_end: int
    language: str
    parent_name: Optional[str]
    params: Optional[str]
    return_type: Optional[str]
    is_test: bool
    file_hash: Optional[str]
    extra: dict


@dataclass
class GraphEdge:
    id: int
    kind: str
    source_qualified: str
    target_qualified: str
    file_path: str
    line: int
    extra: dict


@dataclass
class GraphStats:
    total_nodes: int
    total_edges: int
    nodes_by_kind: dict[str, int]
    edges_by_kind: dict[str, int]
    languages: list[str]
    files_count: int
    last_updated: Optional[str]


class GraphStore:
    """SQLite-backed code knowledge graph."""

    def __init__(self, db_path: str | Path) -> None:
        self.db_path = Path(db_path)
        self.db_path.parent.mkdir(parents=True, exist_ok=True)
        self._conn = sqlite3.connect(
            str(self.db_path), timeout=30, check_same_thread=False
        )
        self._conn.row_factory = sqlite3.Row
        self._conn.execute("PRAGMA journal_mode=WAL")
        self._conn.execute("PRAGMA busy_timeout=5000")
        self._init_schema()
        self._nxg_cache: nx.DiGraph | None = None
        self._cache_lock = threading.Lock()

    def __enter__(self) -> "GraphStore":
        return self

    def __exit__(self, exc_type, exc_val, exc_tb) -> None:
        self.close()

    def _init_schema(self) -> None:
        self._conn.executescript(_SCHEMA_SQL)
        try:
            self._conn.executescript(_FTS_SCHEMA_SQL)
        except Exception:
            pass  # FTS5 not available -- degrade gracefully
        self._conn.commit()

    def _has_fts(self) -> bool:
        """Check if FTS5 table exists."""
        row = self._conn.execute(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='nodes_fts'"
        ).fetchone()
        return row is not None

    def rebuild_fts(self) -> None:
        """Rebuild the FTS index from the nodes table, including keywords from extra."""
        if not self._has_fts():
            return
        self._conn.execute("DELETE FROM nodes_fts")
        rows = self._conn.execute(
            "SELECT id, name, qualified_name, extra FROM nodes"
        ).fetchall()
        for r in rows:
            keywords = _extract_keywords(r["extra"])
            self._conn.execute(
                "INSERT INTO nodes_fts(rowid, name, qualified_name, keywords) "
                "VALUES (?, ?, ?, ?)",
                (r["id"], r["name"], r["qualified_name"], keywords),
            )
        self._conn.commit()

    def _invalidate_cache(self) -> None:
        with self._cache_lock:
            self._nxg_cache = None

    def close(self) -> None:
        self._conn.close()

    # --- Write operations ---

    def upsert_node(self, node: NodeInfo, file_hash: str = "") -> int:
        now = time.time()
        extra = json.dumps(node.extra) if node.extra else "{}"

        self._conn.execute(
            """INSERT INTO nodes
               (kind, name, qualified_name, file_path, line_start, line_end,
                language, parent_name, params, return_type, modifiers, is_test,
                file_hash, extra, updated_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
               ON CONFLICT(qualified_name) DO UPDATE SET
                 kind=excluded.kind, name=excluded.name,
                 file_path=excluded.file_path, line_start=excluded.line_start,
                 line_end=excluded.line_end, language=excluded.language,
                 parent_name=excluded.parent_name, params=excluded.params,
                 return_type=excluded.return_type, modifiers=excluded.modifiers,
                 is_test=excluded.is_test, file_hash=excluded.file_hash,
                 extra=excluded.extra, updated_at=excluded.updated_at
            """,
            (
                node.kind, node.name, node.qualified_name, str(node.file_path),
                node.line_start, node.line_end, node.language,
                node.parent_name, node.params, node.return_type,
                node.modifiers, int(node.is_test), file_hash,
                extra, now,
            ),
        )
        row = self._conn.execute(
            "SELECT id FROM nodes WHERE qualified_name = ?", (node.qualified_name,)
        ).fetchone()
        return row["id"]

    def upsert_edge(self, edge: EdgeInfo) -> int:
        now = time.time()
        extra = json.dumps(edge.extra) if edge.extra else "{}"

        existing = self._conn.execute(
            """SELECT id FROM edges
               WHERE kind=? AND source_qualified=? AND target_qualified=?
                     AND file_path=? AND line=?""",
            (edge.kind, edge.source_qualified, edge.target_qualified,
             edge.file_path, edge.line),
        ).fetchone()

        if existing:
            self._conn.execute(
                "UPDATE edges SET extra=?, updated_at=? WHERE id=?",
                (extra, now, existing["id"]),
            )
            return existing["id"]

        self._conn.execute(
            """INSERT INTO edges
               (kind, source_qualified, target_qualified, file_path, line, extra, updated_at)
               VALUES (?, ?, ?, ?, ?, ?, ?)""",
            (edge.kind, edge.source_qualified, edge.target_qualified,
             edge.file_path, edge.line, extra, now),
        )
        return self._conn.execute("SELECT last_insert_rowid()").fetchone()[0]

    def remove_file_data(self, file_path: str) -> None:
        self._conn.execute("DELETE FROM nodes WHERE file_path = ?", (file_path,))
        self._conn.execute("DELETE FROM edges WHERE file_path = ?", (file_path,))
        self._invalidate_cache()

    def store_file_nodes_edges(
        self, file_path: str, nodes: list[NodeInfo], edges: list[EdgeInfo], fhash: str = ""
    ) -> None:
        self.remove_file_data(file_path)
        for node in nodes:
            self.upsert_node(node, file_hash=fhash)
        for edge in edges:
            self.upsert_edge(edge)
        self._conn.commit()
        self._invalidate_cache()

    def set_metadata(self, key: str, value: str) -> None:
        self._conn.execute(
            "INSERT OR REPLACE INTO metadata (key, value) VALUES (?, ?)", (key, value)
        )
        self._conn.commit()

    def get_metadata(self, key: str) -> Optional[str]:
        row = self._conn.execute("SELECT value FROM metadata WHERE key=?", (key,)).fetchone()
        return row["value"] if row else None

    def commit(self) -> None:
        self._conn.commit()

    # --- Read operations ---

    def get_node(self, qualified_name: str) -> Optional[GraphNode]:
        row = self._conn.execute(
            "SELECT * FROM nodes WHERE qualified_name = ?", (qualified_name,)
        ).fetchone()
        return self._row_to_node(row) if row else None

    def get_nodes_by_file(self, file_path: str) -> list[GraphNode]:
        rows = self._conn.execute(
            "SELECT * FROM nodes WHERE file_path = ?", (file_path,)
        ).fetchall()
        return [self._row_to_node(r) for r in rows]

    def get_edges_by_source(self, qualified_name: str) -> list[GraphEdge]:
        rows = self._conn.execute(
            "SELECT * FROM edges WHERE source_qualified = ?", (qualified_name,)
        ).fetchall()
        return [self._row_to_edge(r) for r in rows]

    def get_edges_by_target(self, qualified_name: str) -> list[GraphEdge]:
        rows = self._conn.execute(
            "SELECT * FROM edges WHERE target_qualified = ?", (qualified_name,)
        ).fetchall()
        return [self._row_to_edge(r) for r in rows]

    def get_all_files(self) -> list[str]:
        rows = self._conn.execute(
            "SELECT DISTINCT file_path FROM nodes WHERE kind = 'File'"
        ).fetchall()
        return [r["file_path"] for r in rows]

    def search_nodes(self, query: str, limit: int = 20) -> list[GraphNode]:
        """Search nodes. Uses FTS5 with BM25 ranking if available, falls back to LIKE."""
        if not query.strip():
            return []

        # Try FTS5 first
        if self._has_fts():
            return self._search_fts(query, limit)
        return self._search_like(query, limit)

    def _search_fts(self, query: str, limit: int) -> list[GraphNode]:
        """FTS5 search with BM25 ranking and prefix matching."""
        # Tokenize camelCase and snake_case into words for better matching
        tokens = _tokenize_query(query)
        if not tokens:
            return []

        # Build FTS5 query: each token as prefix match, all must match
        fts_terms = " AND ".join(f'"{t}"*' for t in tokens)

        try:
            rows = self._conn.execute(
                "SELECT n.* FROM nodes_fts fts "
                "JOIN nodes n ON n.id = fts.rowid "
                "WHERE nodes_fts MATCH ? "
                "ORDER BY bm25(nodes_fts) "
                "LIMIT ?",
                (fts_terms, limit),
            ).fetchall()
            if rows:
                return [self._row_to_node(r) for r in rows]
        except Exception:
            pass  # FTS query failed -- fall back to LIKE

        return self._search_like(query, limit)

    def _search_like(self, query: str, limit: int) -> list[GraphNode]:
        """Fallback LIKE-based search."""
        words = query.lower().split()
        if not words:
            return []

        conditions: list[str] = []
        params: list[str | int] = []
        for word in words:
            conditions.append(
                "(LOWER(name) LIKE ? OR LOWER(qualified_name) LIKE ?)"
            )
            params.extend([f"%{word}%", f"%{word}%"])

        where = " AND ".join(conditions)
        sql = f"SELECT * FROM nodes WHERE {where} LIMIT ?"
        params.append(limit)
        rows = self._conn.execute(sql, params).fetchall()
        return [self._row_to_node(r) for r in rows]

    # --- Impact / Graph traversal ---

    # Edge kinds that represent real data/code dependencies — always traverse
    _STRONG_EDGES = frozenset({
        "CALLS", "IMPORTS", "IMPORTS_FROM", "INHERITS", "IMPLEMENTS",
        "REFERENCES", "PASSES_VAR", "USES_MODULE", "REMOTE_STATE",
        "READS_CONFIG", "RENDERS", "EXTENDS", "INCLUDES",
        "TESTED_BY", "DEPENDS_ON", "CALLS_MACRO", "USES_VARIABLE",
    })

    # Edge kinds that are structural — only follow inward (file→symbol) for seeding,
    # never fan out through them during traversal
    _STRUCTURAL_EDGES = frozenset({
        "CONTAINS",
    })

    def get_impact_radius(
        self, changed_files: list[str], max_depth: int = 3, max_nodes: int = 500
    ) -> dict[str, Any]:
        nxg = self._build_networkx_graph()

        seeds = set()
        for f in changed_files:
            for n in self.get_nodes_by_file(f):
                seeds.add(n.qualified_name)

        visited: set[str] = set()
        frontier = seeds.copy()
        depth = 0
        impacted: set[str] = set()

        while frontier and depth < max_depth:
            next_frontier: set[str] = set()
            for qn in frontier:
                visited.add(qn)
                if qn not in nxg:
                    continue

                # Forward edges: follow only strong dependency edges
                for neighbor in nxg.neighbors(qn):
                    if neighbor in visited:
                        continue
                    edge_data = nxg.edges[qn, neighbor]
                    if edge_data.get("kind") in self._STRONG_EDGES:
                        next_frontier.add(neighbor)
                        impacted.add(neighbor)

                # Reverse edges: things that depend on this node
                for pred in nxg.predecessors(qn):
                    if pred in visited:
                        continue
                    edge_data = nxg.edges[pred, qn]
                    if edge_data.get("kind") in self._STRONG_EDGES:
                        next_frontier.add(pred)
                        impacted.add(pred)

            if len(visited) + len(next_frontier) > max_nodes:
                break
            frontier = next_frontier
            depth += 1

        changed_nodes = [n for qn in seeds if (n := self.get_node(qn))]
        impacted_nodes = [n for qn in (impacted - seeds) if (n := self.get_node(qn))]

        total_impacted = len(impacted_nodes)
        truncated = total_impacted > max_nodes
        if truncated:
            impacted_nodes = impacted_nodes[:max_nodes]

        impacted_files = list({n.file_path for n in impacted_nodes})

        all_qns = seeds | {n.qualified_name for n in impacted_nodes}
        relevant_edges = self.get_edges_among(all_qns) if all_qns else []

        return {
            "changed_nodes": changed_nodes,
            "impacted_nodes": impacted_nodes,
            "impacted_files": impacted_files,
            "edges": relevant_edges,
            "truncated": truncated,
            "total_impacted": total_impacted,
        }

    def get_subgraph(self, qualified_names: list[str]) -> dict[str, Any]:
        nodes = [n for qn in qualified_names if (n := self.get_node(qn))]
        qn_set = set(qualified_names)
        edges = []
        for qn in qualified_names:
            for e in self.get_edges_by_source(qn):
                if e.target_qualified in qn_set:
                    edges.append(e)
        return {"nodes": nodes, "edges": edges}

    def get_stats(self) -> GraphStats:
        total_nodes = self._conn.execute("SELECT COUNT(*) FROM nodes").fetchone()[0]
        total_edges = self._conn.execute("SELECT COUNT(*) FROM edges").fetchone()[0]

        nodes_by_kind: dict[str, int] = {}
        for row in self._conn.execute("SELECT kind, COUNT(*) as cnt FROM nodes GROUP BY kind"):
            nodes_by_kind[row["kind"]] = row["cnt"]

        edges_by_kind: dict[str, int] = {}
        for row in self._conn.execute("SELECT kind, COUNT(*) as cnt FROM edges GROUP BY kind"):
            edges_by_kind[row["kind"]] = row["cnt"]

        languages = [
            r["language"] for r in self._conn.execute(
                "SELECT DISTINCT language FROM nodes WHERE language IS NOT NULL AND language != ''"
            )
        ]

        files_count = self._conn.execute(
            "SELECT COUNT(*) FROM nodes WHERE kind = 'File'"
        ).fetchone()[0]

        return GraphStats(
            total_nodes=total_nodes,
            total_edges=total_edges,
            nodes_by_kind=nodes_by_kind,
            edges_by_kind=edges_by_kind,
            languages=languages,
            files_count=files_count,
            last_updated=self.get_metadata("last_updated"),
        )

    def get_all_edges(self) -> list[GraphEdge]:
        rows = self._conn.execute("SELECT * FROM edges").fetchall()
        return [self._row_to_edge(r) for r in rows]

    def get_edges_among(self, qualified_names: set[str]) -> list[GraphEdge]:
        if not qualified_names:
            return []
        qns = list(qualified_names)
        results: list[GraphEdge] = []
        batch_size = 450
        for i in range(0, len(qns), batch_size):
            batch = qns[i:i + batch_size]
            placeholders = ",".join("?" for _ in batch)
            rows = self._conn.execute(
                f"SELECT * FROM edges WHERE source_qualified IN ({placeholders})",
                batch,
            ).fetchall()
            for r in rows:
                edge = self._row_to_edge(r)
                if edge.target_qualified in qualified_names:
                    results.append(edge)
        return results

    # --- Cross-language queries ---

    def get_cross_language_edges(self) -> list[GraphEdge]:
        """Return edges that connect nodes of different languages."""
        rows = self._conn.execute("""
            SELECT e.* FROM edges e
            JOIN nodes n1 ON e.source_qualified = n1.qualified_name
            JOIN nodes n2 ON e.target_qualified = n2.qualified_name
            WHERE n1.language != n2.language
              AND n1.language != '' AND n2.language != ''
        """).fetchall()
        return [self._row_to_edge(r) for r in rows]

    def get_nodes_by_language(self, language: str) -> list[GraphNode]:
        rows = self._conn.execute(
            "SELECT * FROM nodes WHERE language = ?", (language,)
        ).fetchall()
        return [self._row_to_node(r) for r in rows]

    # --- Internal helpers ---

    def _build_networkx_graph(self) -> nx.DiGraph:
        with self._cache_lock:
            if self._nxg_cache is not None:
                return self._nxg_cache
            g: nx.DiGraph = nx.DiGraph()
            rows = self._conn.execute("SELECT * FROM edges").fetchall()
            for r in rows:
                g.add_edge(r["source_qualified"], r["target_qualified"], kind=r["kind"])
            self._nxg_cache = g
            return g

    def _row_to_node(self, row: sqlite3.Row) -> GraphNode:
        return GraphNode(
            id=row["id"],
            kind=row["kind"],
            name=row["name"],
            qualified_name=row["qualified_name"],
            file_path=row["file_path"],
            line_start=row["line_start"],
            line_end=row["line_end"],
            language=row["language"] or "",
            parent_name=row["parent_name"],
            params=row["params"],
            return_type=row["return_type"],
            is_test=bool(row["is_test"]),
            file_hash=row["file_hash"],
            extra=json.loads(row["extra"]) if row["extra"] else {},
        )

    def _row_to_edge(self, row: sqlite3.Row) -> GraphEdge:
        return GraphEdge(
            id=row["id"],
            kind=row["kind"],
            source_qualified=row["source_qualified"],
            target_qualified=row["target_qualified"],
            file_path=row["file_path"],
            line=row["line"],
            extra=json.loads(row["extra"]) if row["extra"] else {},
        )


import re as _re


def _extract_keywords(extra_json: str | None) -> str:
    """Flatten all keys and string values from extra JSON into a searchable string.

    This lets FTS find YAML field names like 'priorityClassName',
    k8s kinds, label values, etc.
    """
    if not extra_json or extra_json == "{}":
        return ""
    try:
        data = json.loads(extra_json)
    except (json.JSONDecodeError, TypeError):
        return ""

    parts: list[str] = []

    def _walk(obj: Any, prefix: str = "") -> None:
        if isinstance(obj, dict):
            for k, v in obj.items():
                parts.append(str(k))
                _walk(v, f"{prefix}{k}.")
        elif isinstance(obj, list):
            for item in obj:
                _walk(item, prefix)
        elif isinstance(obj, str) and len(obj) <= 200:
            parts.append(obj)

    _walk(data)
    return " ".join(parts)


def _tokenize_query(query: str) -> list[str]:
    """Split a query into tokens, handling camelCase and snake_case.

    Examples:
        'getUser' -> ['get', 'user']
        'get_user_name' -> ['get', 'user', 'name']
        'HTTPClient' -> ['http', 'client']
        'deploy service' -> ['deploy', 'service']
    """
    # First split on whitespace, underscores, slashes, colons, dots
    parts = _re.split(r'[\s_/:.,]+', query)
    tokens = []
    for part in parts:
        if not part:
            continue
        # Split camelCase: 'getUser' -> ['get', 'User'], 'HTTPClient' -> ['HTTP', 'Client']
        camel_parts = _re.sub(r'([A-Z]+)([A-Z][a-z])', r'\1 \2', part)
        camel_parts = _re.sub(r'([a-z0-9])([A-Z])', r'\1 \2', camel_parts)
        for t in camel_parts.split():
            t = t.lower().strip()
            if t and len(t) >= 2:
                tokens.append(t)
    return tokens


def _sanitize_name(s: str, max_len: int = 256) -> str:
    cleaned = "".join(
        ch for ch in s
        if ch in ("\t", "\n") or ord(ch) >= 0x20
    )
    return cleaned[:max_len]


def node_to_dict(n: GraphNode) -> dict:
    return {
        "id": n.id, "kind": n.kind, "name": _sanitize_name(n.name),
        "qualified_name": _sanitize_name(n.qualified_name), "file_path": n.file_path,
        "line_start": n.line_start, "line_end": n.line_end,
        "language": n.language,
        "parent_name": _sanitize_name(n.parent_name) if n.parent_name else n.parent_name,
        "is_test": n.is_test,
        "extra": n.extra,
    }


def edge_to_dict(e: GraphEdge) -> dict:
    return {
        "id": e.id, "kind": e.kind,
        "source": _sanitize_name(e.source_qualified),
        "target": _sanitize_name(e.target_qualified),
        "file_path": e.file_path, "line": e.line,
        "extra": e.extra,
    }
