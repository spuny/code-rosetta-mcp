"""Configuration management for Code Rosetta.

Reads ~/.code-rosetta/config.yaml for graph groups and default settings.
"""

from __future__ import annotations

import os
from pathlib import Path
from typing import Any, Optional

_CONFIG_DIR = Path.home() / ".code-rosetta"
_CONFIG_FILE = _CONFIG_DIR / "config.yaml"


def _load_yaml(path: Path) -> dict:
    """Load YAML file, return empty dict on failure."""
    if not path.exists():
        return {}
    try:
        from ruamel.yaml import YAML
        yaml = YAML()
        with open(path) as f:
            data = yaml.load(f)
        return data if isinstance(data, dict) else {}
    except Exception:
        return {}


def _expand(p: str) -> str:
    """Expand ~ and env vars in a path string."""
    return str(Path(os.path.expandvars(os.path.expanduser(p))).resolve())


class Config:
    """Code Rosetta configuration."""

    def __init__(self, config_path: Path | None = None):
        self._path = config_path or _CONFIG_FILE
        self._data = _load_yaml(self._path)

    @property
    def default_db(self) -> Path:
        raw = self._data.get("default_db", str(_CONFIG_DIR / "graph.db"))
        return Path(_expand(raw))

    @property
    def groups(self) -> dict[str, dict[str, Any]]:
        return self._data.get("graphs", {})

    def get_group(self, name: str) -> Optional[dict[str, Any]]:
        return self.groups.get(name)

    def get_group_db(self, name: str) -> Optional[Path]:
        group = self.get_group(name)
        if not group:
            return None
        raw = group.get("db", str(_CONFIG_DIR / f"{name}.db"))
        return Path(_expand(raw))

    def get_group_repos(self, name: str) -> list[Path]:
        group = self.get_group(name)
        if not group:
            return []
        return [Path(_expand(r)) for r in group.get("repos", [])]

    def find_group_for_repo(self, repo_path: Path) -> Optional[str]:
        """Find which group a repo belongs to, if any."""
        resolved = str(repo_path.resolve())
        for name, group in self.groups.items():
            for repo in group.get("repos", []):
                if str(Path(_expand(repo)).resolve()) == resolved:
                    return name
        return None

    def resolve_db_for_repo(self, repo_path: Path) -> Path:
        """Determine the database path for a repo.

        Priority: group db > default_db > repo-local .code-rosetta/graph.db
        """
        group_name = self.find_group_for_repo(repo_path)
        if group_name:
            return self.get_group_db(group_name)
        return self.default_db

    def list_groups(self) -> list[str]:
        return list(self.groups.keys())

    @property
    def exists(self) -> bool:
        return self._path.exists()


def init_config(groups: dict[str, dict] | None = None) -> Path:
    """Create a default config file if it doesn't exist. Returns config path."""
    _CONFIG_DIR.mkdir(parents=True, exist_ok=True)

    if _CONFIG_FILE.exists():
        return _CONFIG_FILE

    from ruamel.yaml import YAML
    yaml = YAML()
    yaml.default_flow_style = False

    default = {
        "default_db": str(_CONFIG_DIR / "graph.db"),
        "graphs": groups or {
            "example": {
                "db": str(_CONFIG_DIR / "example.db"),
                "repos": [
                    "~/projects/repo1",
                    "~/projects/repo2",
                ],
            }
        },
    }

    with open(_CONFIG_FILE, "w") as f:
        yaml.dump(default, f)

    return _CONFIG_FILE


# Module-level singleton
cfg = Config()
