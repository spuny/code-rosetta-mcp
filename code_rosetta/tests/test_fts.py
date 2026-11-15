import json
import sqlite3

from code_rosetta.graph import GraphStore


def _add(store, nid, name, extra):
    store._conn.execute(
        "INSERT INTO nodes (id, kind, name, qualified_name, file_path, line_start, line_end, language, extra, updated_at) "
        "VALUES (?, 'K8sResource', ?, ?, 'f.yaml', 1, 2, 'yaml', ?, 0)",
        (nid, name, f"f.yaml::{name}", json.dumps(extra)),
    )


def test_rebuild_indexes_keywords_and_survives_node_rewrite(tmp_path):
    db = tmp_path / "g.db"
    with GraphStore(db) as store:
        _add(store, 1, "Deployment/api", {"spec": {"priorityClassName": "high"}})
        store.commit()
        store.rebuild_fts()
        assert [n.name for n in store.search_nodes("priorityClassName")] == ["Deployment/api"]

        store._conn.execute("DELETE FROM nodes")
        _add(store, 2, "Deployment/worker", {"spec": {"hostNetwork": True}})
        store.commit()
        store.rebuild_fts()
        assert [n.name for n in store.search_nodes("hostNetwork")] == ["Deployment/worker"]


def _fn(store, nid, name, qualified):
    store._conn.execute(
        "INSERT INTO nodes (id, kind, name, qualified_name, file_path, line_start, line_end, language, extra, updated_at) "
        "VALUES (?, 'Function', ?, ?, ?, 1, 2, 'python', '{}', 0)",
        (nid, name, qualified, f"/r/{nid}.py"),
    )


def test_exact_name_matches_rank_first_even_when_bm25_prefers_others(tmp_path):
    with GraphStore(tmp_path / "g.db") as store:
        # 40 short near-names that BM25 likes, then the exact names with long qualified names it dislikes.
        for i in range(40):
            _fn(store, i + 1, f"get_conn_{i}", f"get_conn_{i}")
        _fn(store, 100, "get_conn", "pkg.a.b.c.d.e.f.g.h.i.j.k.l.m.n.get_conn")
        _fn(store, 101, "GET_CONN", "pkg.a.b.c.d.e.f.g.h.i.j.k.l.m.n.GET_CONN")
        store.commit()
        store.rebuild_fts()

        names = [n.name for n in store.search_nodes("get_conn", limit=5)]
        assert names[:2] == ["get_conn", "GET_CONN"]  # case-sensitive exact, then case-insensitive exact
        assert len(names) == 5
        assert len(set(names)) == 5  # no duplicates when the FTS pass finds the exact nodes again

        # Several tokens: no exact-name pass, plain ranking.
        assert store.search_nodes("get conn", limit=3)


def test_old_external_content_schema_is_migrated(tmp_path):
    db = tmp_path / "old.db"
    with GraphStore(db):
        pass
    conn = sqlite3.connect(db)
    conn.execute("DROP TABLE nodes_fts")
    conn.execute(
        'CREATE VIRTUAL TABLE nodes_fts USING fts5(name, qualified_name, tokenize="unicode61", '
        "content=nodes, content_rowid=id)"
    )
    conn.commit()
    conn.close()
    with GraphStore(db) as store:
        sql = store._conn.execute("SELECT sql FROM sqlite_master WHERE name='nodes_fts'").fetchone()[0]
        assert "keywords" in sql and "content=nodes" not in sql
