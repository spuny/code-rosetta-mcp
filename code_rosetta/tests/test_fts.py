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
