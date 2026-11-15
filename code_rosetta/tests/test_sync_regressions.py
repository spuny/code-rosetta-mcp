import json
import subprocess

import pytest
from click.testing import CliRunner
from ruamel.yaml import YAML
from ruamel.yaml.error import YAMLError

from code_rosetta import config, gitlab_sync, incremental
from code_rosetta.cli import main
from code_rosetta.graph import GraphStore
from code_rosetta.tests.test_gitlab_sync import (
    _commit, _graph_files, _grep, _origin, _project, _run, _sync, env,
)


@pytest.mark.parametrize("stage", ["index", "fts"])
def test_interrupted_update_is_retried_with_nonempty_indexes(env, monkeypatch, stage):
    bare, work = _origin(env, "a", {"a.py": 'def handler():\n    return "oldvalue"\n'})
    projects = [_project(1, "g/a", bare)]
    _sync(env, monkeypatch, projects)
    _commit(work, {"a.py": 'def replacement():\n    return "newvalue"\n'})

    def crash(*args, **kwargs):
        raise RuntimeError("interrupted")

    with monkeypatch.context() as patch:
        if stage == "index":
            patch.setattr(incremental, "incremental_update", crash)
        else:
            patch.setattr(GraphStore, "rebuild_fts", crash)
        with pytest.raises(RuntimeError, match="interrupted"):
            _sync(env, patch, projects)

    result = _sync(env, monkeypatch, projects)
    assert result["unchanged"] == 1
    assert result["reindexed"] == 1
    assert result["ok"] is True
    assert not _grep(env, "oldvalue")["matches"]
    assert _grep(env, "newvalue")["matches"]
    with GraphStore(env / "data/q.db") as store:
        assert store.search_nodes("replacement")
        assert not store.search_nodes("handler")
    assert _sync(env, monkeypatch, projects)["reindexed"] == 0


def test_interrupted_update_then_remote_rollback_rebuilds_the_index(env, monkeypatch):
    bare, work = _origin(env, "a", {"a.py": 'def handler():\n    return "oldvalue"\n'})
    projects = [_project(1, "g/a", bare)]
    _sync(env, monkeypatch, projects)
    original = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=work, text=True).strip()
    _commit(work, {"a.py": 'def replacement():\n    return "newvalue"\n'})
    update = incremental.incremental_update

    def crash_after_update(*args, **kwargs):
        update(*args, **kwargs)
        raise RuntimeError("interrupted")

    with monkeypatch.context() as patch:
        patch.setattr(incremental, "incremental_update", crash_after_update)
        with pytest.raises(RuntimeError, match="interrupted"):
            _sync(env, patch, projects)
    _run(["git", "update-ref", "refs/heads/main", original], bare)

    result = _sync(env, monkeypatch, projects)
    assert result["ok"] is True
    assert result["reindexed"] == 1
    assert _grep(env, "oldvalue")["matches"]
    assert not _grep(env, "newvalue")["matches"]


def test_existing_indexes_without_a_checkpoint_are_rebuilt(env, monkeypatch):
    bare, _ = _origin(env, "a", {"a.py": "def handler():\n    return 1\n"})
    projects = [_project(1, "g/a", bare)]
    _sync(env, monkeypatch, projects)
    with GraphStore(env / "data/q.db") as store:
        store._conn.execute("DELETE FROM metadata")
        store.commit()
    result = _sync(env, monkeypatch, projects)
    assert result["unchanged"] == 1 and result["reindexed"] == 1
    assert result["ok"] is True
    assert _sync(env, monkeypatch, projects)["reindexed"] == 0


def test_successful_updates_stay_incremental(env, monkeypatch):
    bare, work = _origin(env, "a", {"a.py": "def handler():\n    return 1\n"})
    projects = [_project(1, "g/a", bare)]
    _sync(env, monkeypatch, projects)
    _commit(work, {"a.py": "def handler():\n    return 2\n"})
    update = incremental.incremental_update
    calls = []
    monkeypatch.setattr(incremental, "incremental_update", lambda *a, **k: calls.append(a[0]) or update(*a, **k))
    result = _sync(env, monkeypatch, projects)
    assert result["updated"] == 1 and result["reindexed"] == 0
    assert calls == [(env / "data/repos/g/a").resolve()]


def test_parse_errors_leave_the_repo_retryable(env, monkeypatch):
    bare, work = _origin(env, "a", {"a.py": "def handler():\n    return 1\n"})
    projects = [_project(1, "g/a", bare)]
    _sync(env, monkeypatch, projects)
    _commit(work, {"a.py": "def handler():\n    return 2\n"})
    update = incremental.incremental_update

    def failed_update(*args, **kwargs):
        return {**update(*args, **kwargs), "errors": [{"file": "a.py", "error": "parse failed"}]}

    with monkeypatch.context() as patch:
        patch.setattr(incremental, "incremental_update", failed_update)
        result = _sync(env, patch, projects)
    assert result["ok"] is False
    assert result["indexed_repos"] == 0
    assert _sync(env, monkeypatch, projects)["reindexed"] == 1


def test_rename_removes_the_old_graph_file(env, monkeypatch):
    bare, work = _origin(env, "a", {"a.py": "def handler():\n    return 1\n"})
    projects = [_project(1, "g/a", bare)]
    _sync(env, monkeypatch, projects)
    _run(["git", "mv", "a.py", "b.py"], work)
    _commit(work, {})
    assert _sync(env, monkeypatch, projects)["ok"] is True
    assert _graph_files(env, "g/a") == ["b.py"]


def test_write_config_preserves_other_groups_and_settings(tmp_path):
    path = tmp_path / "config.yaml"
    path.write_text("default_db: /keep.db\ndefault_group: local\ngraphs:\n  local:\n    db: /local.db\n    repos: []\n")
    gitlab_sync.write_config(path, "shared", tmp_path / "shared.db", [tmp_path / "repo"])
    data = YAML().load(path.read_text())
    assert data["default_db"] == "/keep.db"
    assert data["default_group"] == "local"
    assert data["graphs"]["local"] == {"db": "/local.db", "repos": []}
    assert data["graphs"]["shared"]["repos"] == [str(tmp_path / "repo")]
    gitlab_sync.write_config(path, "shared", tmp_path / "new.db", [])
    assert config.Config(path).get_group_db("shared") == tmp_path / "new.db"
    assert str(config.Config(path).get_group_db("local")) == "/local.db"


@pytest.mark.parametrize("contents", ["not: [valid", "- a list\n", "graphs: [not, a, mapping]\n"])
def test_invalid_config_is_not_overwritten(tmp_path, contents):
    path = tmp_path / "config.yaml"
    path.write_text(contents)
    with pytest.raises((ValueError, YAMLError)):
        gitlab_sync.write_config(path, "shared", tmp_path / "shared.db", [])
    assert path.read_text() == contents


def test_failed_fetch_returns_nonzero_and_keeps_existing_index(env, monkeypatch):
    bare, _ = _origin(env, "a", {"a.py": "def handler():\n    return 1\n"})
    _sync(env, monkeypatch, [_project(1, "g/a", bare)])
    projects = [_project(1, "g/a", env / "missing.git")]
    result = _sync(env, monkeypatch, projects)
    assert result["failed"] == 1 and result["ok"] is False
    assert result["indexed_repos"] == result["expected_repos"] == 1
    assert _grep(env, "handler")["matches"]
    monkeypatch.setenv("GITLAB_TOKEN", "t")
    result = CliRunner().invoke(main, [
        "sync-gitlab", "--url", "https://gitlab.example", "--groups", "g",
        "--repos-dir", str(env / "data/repos"), "--db", str(env / "data/q.db"),
    ])
    assert result.exit_code == 2, result.output
    assert json.loads(result.output)["ok"] is False
