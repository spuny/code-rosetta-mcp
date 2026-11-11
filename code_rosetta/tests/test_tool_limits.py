"""Server-side limits: one oversized request must not be able to take the shared server down."""

import subprocess

import pytest

from code_rosetta import tools


@pytest.fixture
def repo(tmp_path, monkeypatch):
    monkeypatch.setenv("CODE_ROSETTA_CONFIG", str(tmp_path / "none.yaml"))
    root = tmp_path / "svc"
    root.mkdir()
    for i in range(30):
        (root / f"mod{i}.py").write_text(
            "".join(f"def handler_{i}_{j}():\n    return 'needle {i} {j}'\n" for j in range(20))
        )
    subprocess.run(["git", "init", "-q"], cwd=root, check=True)
    subprocess.run(["git", "add", "-A"], cwd=root, check=True)
    tools.build_or_update_graph(full_rebuild=True, repo_root=str(root))
    return root


def test_grep_limit_is_clamped(repo, monkeypatch):
    monkeypatch.setattr(tools, "GREP_MAX_MATCHES", 50)
    res = tools.grep_content("needle", repo_root=str(repo), limit=100_000_000)
    assert res["status"] == "ok"
    assert len(res["matches"]) == 50
    assert res["truncated"] is True
    assert res["limit_clamped"] == {"requested": 100_000_000, "applied": 50}


def test_grep_files_only_limit_is_clamped(repo, monkeypatch):
    monkeypatch.setattr(tools, "GREP_MAX_FILES", 7)
    res = tools.grep_content("needle", repo_root=str(repo), limit=10**9, files_only=True)
    assert len(res["files"]) == 7
    assert res["truncated"] is True
    assert res["limit_clamped"]["applied"] == 7


def test_clamp_note_without_truncation_when_result_is_small(repo, monkeypatch):
    monkeypatch.setattr(tools, "GREP_MAX_MATCHES", 50)
    res = tools.grep_content("handler_3_7", repo_root=str(repo), limit=10**9)
    assert len(res["matches"]) == 1
    assert res["truncated"] is False
    assert res["limit_clamped"]["applied"] == 50


def test_grep_within_limit_has_no_clamp_note(repo):
    res = tools.grep_content("needle", repo_root=str(repo), limit=10)
    assert len(res["matches"]) == 10
    assert "limit_clamped" not in res


def test_grep_response_bytes_are_bounded(repo, monkeypatch):
    monkeypatch.setattr(tools, "GREP_MAX_RESPONSE_BYTES", 2_000)
    res = tools.grep_content("needle", repo_root=str(repo), limit=5000)
    assert res["truncated"] is True
    assert 0 < len(res["matches"]) < 100
    assert sum(len(m["text"]) + len(m["path"]) for m in res["matches"]) <= 2_000


def test_search_nodes_limit_is_clamped(repo, monkeypatch):
    monkeypatch.setattr(tools, "SEARCH_MAX_RESULTS", 5)
    res = tools.search_nodes("handler", repo_root=str(repo), limit=10**9)
    assert len(res["results"]) == 5
    assert res["limit_clamped"] == {"requested": 10**9, "applied": 5}
