"""Parser plugin system — register parsers by file extension."""

from __future__ import annotations

from pathlib import Path
from typing import Protocol

from code_rosetta.models import EdgeInfo, NodeInfo


class LanguageParser(Protocol):
    """Interface that every parser must implement."""

    extensions: list[str]

    def parse(self, file_path: Path, source: bytes) -> tuple[list[NodeInfo], list[EdgeInfo]]:
        """Parse a file and return nodes + edges."""
        ...


class CrossReferencePass(Protocol):
    """Detects relationships across files/languages after all files are parsed."""

    def detect(self, all_nodes: list[NodeInfo], all_edges: list[EdgeInfo]) -> list[EdgeInfo]:
        """Return new edges discovered by cross-referencing parsed data."""
        ...


# Registry: extension -> parser instance
_PARSERS: dict[str, LanguageParser] = {}


def register_parser(parser: LanguageParser) -> None:
    """Register a parser for its declared file extensions."""
    for ext in parser.extensions:
        _PARSERS[ext] = parser


def get_parser(file_path: Path) -> LanguageParser | None:
    """Get the parser for a file, or None if unsupported."""
    return _PARSERS.get(file_path.suffix.lower())


def detect_language(file_path: Path) -> str | None:
    """Return the language name for a file, or None."""
    parser = get_parser(file_path)
    if parser is None:
        return None
    # Language is derived from the parser class name by convention
    return getattr(parser, "language", None)


def supported_extensions() -> set[str]:
    """Return all registered file extensions."""
    return set(_PARSERS.keys())


def parse_file(file_path: Path, source: bytes) -> tuple[list[NodeInfo], list[EdgeInfo]]:
    """Parse a file using the appropriate parser. Returns empty lists if unsupported."""
    parser = get_parser(file_path)
    if parser is None:
        return [], []
    return parser.parse(file_path, source)
