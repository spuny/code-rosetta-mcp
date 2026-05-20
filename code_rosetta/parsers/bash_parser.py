"""Bash/Shell parser using tree-sitter to extract functions, variables, sources, and calls."""

from __future__ import annotations

from pathlib import Path

from tree_sitter_language_pack import get_parser as ts_get_parser

from code_rosetta.models import EdgeInfo, NodeInfo, make_qualified

_TS_PARSER = ts_get_parser("bash")


def _node_text(node) -> str:
    """Return the UTF-8 text of a tree-sitter node."""
    return node.text.decode("utf-8", errors="replace") if node.text else ""


def _find_first(node, node_type: str):
    """Return first direct child of given type, or None."""
    for c in node.children:
        if c.type == node_type:
            return c
    return None


def _get_function_name(node) -> str:
    """Extract function name from a function_definition node."""
    # function_definition has either:
    #   function <name> () { ... }
    #   <name> () { ... }
    for c in node.children:
        if c.type == "word":
            return _node_text(c)
    return ""


def _collect_commands(node) -> list:
    """Recursively collect all command nodes from a subtree."""
    results = []
    if node.type == "command":
        results.append(node)
    for c in node.children:
        results.extend(_collect_commands(c))
    return results


class BashParser:
    language = "bash"
    extensions = [".sh", ".bash"]

    def parse(self, file_path: Path, source: bytes) -> tuple[list[NodeInfo], list[EdgeInfo]]:
        nodes: list[NodeInfo] = []
        edges: list[EdgeInfo] = []
        fp = str(file_path)

        tree = _TS_PARSER.parse(source)
        root = tree.root_node

        # File node
        file_qname = make_qualified(fp, "File", file_path.name)
        nodes.append(NodeInfo(
            kind="File",
            name=file_path.name,
            qualified_name=file_qname,
            file_path=fp,
            line_start=1,
            line_end=root.end_point[0] + 1,
            language=self.language,
        ))

        # Track defined function names for call resolution
        defined_functions: set[str] = set()

        # First pass: collect function definitions
        for child in root.children:
            if child.type == "function_definition":
                name = _get_function_name(child)
                if name:
                    defined_functions.add(name)

        # Second pass: extract everything
        for child in root.children:
            line = child.start_point[0] + 1

            if child.type == "function_definition":
                name = _get_function_name(child)
                if not name:
                    continue

                func_qname = make_qualified(fp, "Function", name)
                body = _find_first(child, "compound_statement")

                nodes.append(NodeInfo(
                    kind="Function",
                    name=name,
                    qualified_name=func_qname,
                    file_path=fp,
                    line_start=line,
                    line_end=child.end_point[0] + 1,
                    language=self.language,
                ))
                edges.append(EdgeInfo(
                    kind="CONTAINS",
                    source_qualified=file_qname,
                    target_qualified=func_qname,
                    file_path=fp,
                    line=line,
                ))

                # Extract calls within function body
                if body:
                    for cmd in _collect_commands(body):
                        cmd_name_node = _find_first(cmd, "command_name")
                        if not cmd_name_node:
                            continue
                        cmd_name = _node_text(cmd_name_node)

                        # source/. = file include
                        if cmd_name in ("source", "."):
                            args = [c for c in cmd.children if c.type == "word" and c != cmd_name_node]
                            if args:
                                sourced = _node_text(args[0])
                                edges.append(EdgeInfo(
                                    kind="IMPORTS",
                                    source_qualified=func_qname,
                                    target_qualified=f"file::{sourced}",
                                    file_path=fp,
                                    line=cmd.start_point[0] + 1,
                                ))

                        # Calls to other defined functions
                        elif cmd_name in defined_functions and cmd_name != name:
                            edges.append(EdgeInfo(
                                kind="CALLS",
                                source_qualified=func_qname,
                                target_qualified=make_qualified(fp, "Function", cmd_name),
                                file_path=fp,
                                line=cmd.start_point[0] + 1,
                            ))

            elif child.type == "command":
                # Top-level commands (outside functions)
                cmd_name_node = _find_first(child, "command_name")
                if not cmd_name_node:
                    continue
                cmd_name = _node_text(cmd_name_node)

                # source/. at top level
                if cmd_name in ("source", "."):
                    word_nodes = [c for c in child.children if c.type == "word"]
                    # First word after source/. is the file path
                    source_args = [w for w in word_nodes if _node_text(w) not in ("source", ".")]
                    if source_args:
                        sourced = _node_text(source_args[0])
                        edges.append(EdgeInfo(
                            kind="IMPORTS",
                            source_qualified=file_qname,
                            target_qualified=f"file::{sourced}",
                            file_path=fp,
                            line=line,
                        ))

                # Top-level calls to defined functions
                elif cmd_name in defined_functions:
                    edges.append(EdgeInfo(
                        kind="CALLS",
                        source_qualified=file_qname,
                        target_qualified=make_qualified(fp, "Function", cmd_name),
                        file_path=fp,
                        line=line,
                    ))

            elif child.type in ("variable_assignment", "declaration_command"):
                # Top-level variable/export
                var_node = None
                if child.type == "variable_assignment":
                    var_node = _find_first(child, "variable_name")
                elif child.type == "declaration_command":
                    assign = _find_first(child, "variable_assignment")
                    if assign:
                        var_node = _find_first(assign, "variable_name")

                if var_node:
                    var_name = _node_text(var_node)
                    var_qname = make_qualified(fp, "Variable", var_name)
                    is_export = child.type == "declaration_command"

                    nodes.append(NodeInfo(
                        kind="Variable",
                        name=var_name,
                        qualified_name=var_qname,
                        file_path=fp,
                        line_start=line,
                        line_end=line,
                        language=self.language,
                        extra={"exported": is_export},
                    ))
                    edges.append(EdgeInfo(
                        kind="CONTAINS",
                        source_qualified=file_qname,
                        target_qualified=var_qname,
                        file_path=fp,
                        line=line,
                    ))

        return nodes, edges


bash_parser = BashParser()
