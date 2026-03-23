"""Core data model — the contract between parsers and the graph store."""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path


@dataclass
class NodeInfo:
    """A node in the codebase graph."""

    kind: str  # Function, Class, Resource, Variable, Template, Document, etc.
    name: str
    qualified_name: str  # Unique: file_path::kind::name
    file_path: str
    line_start: int
    line_end: int
    language: str  # python, hcl, yaml, jinja2, etc.
    parent_name: str = ""
    params: str = ""
    return_type: str = ""
    modifiers: str = ""  # e.g. "async", "static"
    is_test: bool = False
    extra: dict = field(default_factory=dict)  # Language-specific metadata


@dataclass
class EdgeInfo:
    """An edge (relationship) in the codebase graph."""

    kind: str  # CALLS, IMPORTS, REFERENCES, PASSES_VAR, READS_CONFIG, etc.
    source_qualified: str
    target_qualified: str
    file_path: str = ""
    line: int = 0
    extra: dict = field(default_factory=dict)


def make_qualified(file_path: str | Path, kind: str, name: str) -> str:
    """Build a qualified name: file_path::name for files, file_path::kind::name for symbols."""
    fp = str(file_path)
    if kind == "File":
        return fp
    return f"{fp}::{name}"
