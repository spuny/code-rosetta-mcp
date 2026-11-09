import subprocess

import pytest

from code_rosetta.content import _required_literal, grep, sync_repo
from code_rosetta.graph import GraphStore


def _git_repo(root, files):
    root.mkdir()
    for rel, data in files.items():
        p = root / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(data if isinstance(data, bytes) else data.encode())
    subprocess.run(["git", "init", "-q"], cwd=root, check=True)
    subprocess.run(["git", "add", "-A"], cwd=root, check=True)
    return root


@pytest.fixture
def repo(tmp_path):
    return _git_repo(tmp_path / "objects", {
        "deploy.yaml": "spec:\n  image: ghcr.io/sourcebot-dev/sourcebot:v4.17.4\n  env:\n    - name: FORCE_ENABLE_ANONYMOUS_ACCESS\n",
        "README.md": "Sourcebot runs here.\n",
        "blob.bin": b"\x00\x01binary",
    })


def test_sync_indexes_text_skips_binary_and_is_incremental(tmp_path, repo):
    with GraphStore(tmp_path / "g.db") as store:
        assert sync_repo(store, repo) == {"indexed": 2, "unchanged": 0, "removed": 0}
        assert sync_repo(store, repo) == {"indexed": 0, "unchanged": 2, "removed": 0}

        (repo / "README.md").write_text("changed\n")
        (repo / "deploy.yaml").unlink()
        subprocess.run(["git", "add", "-A"], cwd=repo, check=True)
        assert sync_repo(store, repo) == {"indexed": 1, "unchanged": 0, "removed": 1}
        assert grep(store, "FORCE_ENABLE")["files"] == []
        assert grep(store, "changed")["files"] == [str(repo / "README.md")]


def test_literal_grep_returns_lines_case_sensitive_by_default(tmp_path, repo):
    with GraphStore(tmp_path / "g.db") as store:
        sync_repo(store, repo)
        res = grep(store, "FORCE_ENABLE_ANONYMOUS_ACCESS")
        assert res["matches"] == [{
            "path": str(repo / "deploy.yaml"), "line": 4,
            "text": "    - name: FORCE_ENABLE_ANONYMOUS_ACCESS",
        }]
        assert grep(store, "sourcebot runs")["matches"] == []
        assert len(grep(store, "sourcebot runs", ignore_case=True)["matches"]) == 1


def test_regex_short_pattern_path_glob_and_files_only(tmp_path, repo):
    with GraphStore(tmp_path / "g.db") as store:
        sync_repo(store, repo)
        res = grep(store, r"image:\s*ghcr\.io/[a-z-]+/sourcebot", regex=True)
        assert [m["line"] for m in res["matches"]] == [2]
        assert grep(store, "v4", files_only=True)["files"] == [str(repo / "deploy.yaml")]
        assert grep(store, "Sourcebot", path_glob="*.yaml")["files"] == []
        assert grep(store, "(", regex=True)["status"] == "error"


def test_limit_truncates(tmp_path):
    repo = _git_repo(tmp_path / "r", {"a.txt": "hit\n" * 10})
    with GraphStore(tmp_path / "g.db") as store:
        sync_repo(store, repo)
        res = grep(store, "hit", limit=3)
        assert len(res["matches"]) == 3 and res["truncated"]


def test_full_build_populates_content(tmp_path, repo):
    from code_rosetta.incremental import full_build

    with GraphStore(tmp_path / "g.db") as store:
        result = full_build(repo, store)
        assert result["content"]["indexed"] == 2
        assert grep(store, "ghcr.io/sourcebot-dev")["files"] == [str(repo / "deploy.yaml")]


@pytest.mark.parametrize("pattern,expected", [
    (r"image:\s*ghcr\.io/[a-z-]+/sourcebot", "/sourcebot"),
    ("foo|barbaz", None),
    ("abcd?", "abc"),
    (r"(optional)?required_part", "required_part"),
    (r"x{0,3}yz", None),
    (r"\d+", None),
])
def test_required_literal(pattern, expected):
    assert _required_literal(pattern) == expected


def test_incremental_update_refreshes_content(tmp_path, repo):
    from code_rosetta.incremental import full_build, incremental_update

    with GraphStore(tmp_path / "g.db") as store:
        full_build(repo, store)
        (repo / "deploy.yaml").write_text("spec:\n  image: ghcr.io/other/app:v1\n")
        subprocess.run(["git", "add", "-A"], cwd=repo, check=True)
        result = incremental_update(repo, store, changed_files=["deploy.yaml"])
        assert result["content"]["indexed"] == 1
        assert grep(store, "sourcebot-dev")["files"] == []
        assert grep(store, "ghcr.io/other")["files"] == [str(repo / "deploy.yaml")]
