"""Python parser using tree-sitter to extract nodes and edges from .py files."""

from __future__ import annotations

import re
from pathlib import Path

from tree_sitter_language_pack import get_parser as ts_get_parser

from code_rosetta.models import EdgeInfo, NodeInfo, make_qualified

# Grab the tree-sitter parser once at import time.
_TS_PARSER = ts_get_parser("python")


def _node_text(node, source: bytes) -> str:
    """Return the UTF-8 text of a tree-sitter node."""
    return source[node.start_byte:node.end_byte].decode("utf-8", errors="replace")


def _is_test_file(file_path: Path) -> bool:
    name = file_path.name
    return name.startswith("test_") or name.endswith("_test.py")


def _is_test_function(name: str) -> bool:
    return name.startswith("test_")


# ---------------------------------------------------------------------------
# Visitor helpers
# ---------------------------------------------------------------------------

def _get_children_by_type(node, *types: str):
    return [c for c in node.children if c.type in types]


def _get_child_by_field(node, field: str):
    return node.child_by_field_name(field)


def _collect_decorators(decorated_node, source: bytes) -> list[str]:
    """Collect decorator names from a decorated_definition node."""
    decorators: list[str] = []
    for child in decorated_node.children:
        if child.type == "decorator":
            # decorator children: '@', expression
            # Text after '@' is the decorator expression
            text = _node_text(child, source).lstrip("@").strip().split("(")[0].strip()
            decorators.append(f"@{text}")
    return decorators


def _params_text(parameters_node, source: bytes) -> str:
    """Return a compact string for a function's parameter list."""
    if parameters_node is None:
        return ""
    text = _node_text(parameters_node, source)
    # Strip outer parens and normalise whitespace
    text = text.strip("()")
    # Collapse multiline
    text = re.sub(r"\s+", " ", text).strip()
    return text


def _return_type_text(return_type_node, source: bytes) -> str:
    if return_type_node is None:
        return ""
    text = _node_text(return_type_node, source)
    # tree-sitter includes the '->' prefix in the type field content;
    # strip it just in case.
    return text.lstrip("->").strip()


# ---------------------------------------------------------------------------
# Main parser class
# ---------------------------------------------------------------------------

class PythonParser:
    """Parses Python source files and returns NodeInfo + EdgeInfo lists."""

    language: str = "python"
    extensions: list[str] = [".py"]

    # ------------------------------------------------------------------
    # Public API (LanguageParser protocol)
    # ------------------------------------------------------------------

    def parse(self, file_path: Path, source: bytes) -> tuple[list[NodeInfo], list[EdgeInfo]]:
        tree = _TS_PARSER.parse(source)
        root = tree.root_node

        fp = str(file_path)
        is_test_file = _is_test_file(file_path)

        nodes: list[NodeInfo] = []
        edges: list[EdgeInfo] = []

        # File node
        file_node = NodeInfo(
            kind="File",
            name=file_path.name,
            qualified_name=make_qualified(fp, "File", file_path.name),
            file_path=fp,
            line_start=1,
            line_end=root.end_point[0] + 1,
            language="python",
            is_test=is_test_file,
        )
        nodes.append(file_node)

        # Walk the module body
        self._visit_body(
            body_nodes=root.children,
            source=source,
            fp=fp,
            file_qualified=file_node.qualified_name,
            parent_qualified=file_node.qualified_name,
            parent_name="",
            context="file",
            is_test_file=is_test_file,
            nodes=nodes,
            edges=edges,
        )

        return nodes, edges

    # ------------------------------------------------------------------
    # Recursive body visitor
    # ------------------------------------------------------------------

    def _visit_body(
        self,
        body_nodes,
        source: bytes,
        fp: str,
        file_qualified: str,
        parent_qualified: str,
        parent_name: str,
        context: str,  # "file" | "class" | "function"
        is_test_file: bool,
        nodes: list[NodeInfo],
        edges: list[EdgeInfo],
    ) -> None:
        for node in body_nodes:
            if node.type in ("function_definition", "async_function_def"):
                self._handle_function(
                    node, [], source, fp, file_qualified, parent_qualified,
                    parent_name, context, is_test_file, nodes, edges,
                )
            elif node.type == "decorated_definition":
                self._handle_decorated(
                    node, source, fp, file_qualified, parent_qualified,
                    parent_name, context, is_test_file, nodes, edges,
                )
            elif node.type == "class_definition":
                self._handle_class(
                    node, [], source, fp, file_qualified, parent_qualified,
                    parent_name, is_test_file, nodes, edges,
                )
            elif node.type in ("import_statement", "import_from_statement"):
                self._handle_import(
                    node, source, fp, parent_qualified, edges,
                )
            elif node.type == "expression_statement":
                # Could contain calls at module/class level
                for child in node.children:
                    if child.type == "call":
                        self._collect_calls(
                            child, source, fp, parent_qualified, edges,
                        )
            elif node.type == "assignment":
                # Handle calls on the right-hand side
                value = _get_child_by_field(node, "right")
                if value and value.type == "call":
                    self._collect_calls(
                        value, source, fp, parent_qualified, edges,
                    )

    # ------------------------------------------------------------------
    # Handler: decorated_definition
    # ------------------------------------------------------------------

    def _handle_decorated(
        self,
        node,
        source: bytes,
        fp: str,
        file_qualified: str,
        parent_qualified: str,
        parent_name: str,
        context: str,
        is_test_file: bool,
        nodes: list[NodeInfo],
        edges: list[EdgeInfo],
    ) -> None:
        decorators = _collect_decorators(node, source)
        # Find the inner definition
        inner = None
        for child in node.children:
            if child.type in ("function_definition", "async_function_def", "class_definition"):
                inner = child
                break
        if inner is None:
            return
        if inner.type == "class_definition":
            self._handle_class(
                inner, decorators, source, fp, file_qualified, parent_qualified,
                parent_name, is_test_file, nodes, edges,
            )
        else:
            self._handle_function(
                inner, decorators, source, fp, file_qualified, parent_qualified,
                parent_name, context, is_test_file, nodes, edges,
            )

    # ------------------------------------------------------------------
    # Handler: class_definition
    # ------------------------------------------------------------------

    def _handle_class(
        self,
        node,
        decorators: list[str],
        source: bytes,
        fp: str,
        file_qualified: str,
        parent_qualified: str,
        parent_name: str,
        is_test_file: bool,
        nodes: list[NodeInfo],
        edges: list[EdgeInfo],
    ) -> None:
        name_node = _get_child_by_field(node, "name")
        if name_node is None:
            return
        class_name = _node_text(name_node, source)
        qualified = make_qualified(fp, "Class", class_name)

        modifiers_parts = list(decorators)
        modifiers = " ".join(modifiers_parts)

        class_node = NodeInfo(
            kind="Class",
            name=class_name,
            qualified_name=qualified,
            file_path=fp,
            line_start=node.start_point[0] + 1,
            line_end=node.end_point[0] + 1,
            language="python",
            parent_name=parent_name,
            modifiers=modifiers,
            is_test=is_test_file,
        )
        nodes.append(class_node)

        # CONTAINS edge: file -> class
        edges.append(EdgeInfo(
            kind="CONTAINS",
            source_qualified=file_qualified,
            target_qualified=qualified,
            file_path=fp,
            line=node.start_point[0] + 1,
        ))

        # INHERITS edges from superclasses
        bases_node = _get_child_by_field(node, "superclasses")
        if bases_node is not None:
            for base in bases_node.children:
                if base.type in ("identifier", "attribute"):
                    base_name = _node_text(base, source)
                    # Target qualified is just the bare name; cross-ref pass can resolve
                    edges.append(EdgeInfo(
                        kind="INHERITS",
                        source_qualified=qualified,
                        target_qualified=base_name,
                        file_path=fp,
                        line=base.start_point[0] + 1,
                    ))

        # Recurse into class body
        body = _get_child_by_field(node, "body")
        if body is not None:
            self._visit_body(
                body_nodes=body.children,
                source=source,
                fp=fp,
                file_qualified=file_qualified,
                parent_qualified=qualified,
                parent_name=class_name,
                context="class",
                is_test_file=is_test_file,
                nodes=nodes,
                edges=edges,
            )

    # ------------------------------------------------------------------
    # Handler: function_definition / async_function_def
    # ------------------------------------------------------------------

    def _handle_function(
        self,
        node,
        decorators: list[str],
        source: bytes,
        fp: str,
        file_qualified: str,
        parent_qualified: str,
        parent_name: str,
        context: str,
        is_test_file: bool,
        nodes: list[NodeInfo],
        edges: list[EdgeInfo],
    ) -> None:
        name_node = _get_child_by_field(node, "name")
        if name_node is None:
            return
        func_name = _node_text(name_node, source)

        is_method = context == "class"
        kind = "Method" if is_method else "Function"

        # Qualified name includes parent for methods to avoid collisions
        symbol_name = f"{parent_name}.{func_name}" if is_method and parent_name else func_name
        qualified = make_qualified(fp, kind, symbol_name)

        # Modifiers
        modifiers_parts = list(decorators)
        if node.type == "async_function_def":
            modifiers_parts.append("async")
        modifiers = " ".join(modifiers_parts)

        params_node = _get_child_by_field(node, "parameters")
        params = _params_text(params_node, source)

        ret_node = _get_child_by_field(node, "return_type")
        return_type = _return_type_text(ret_node, source)

        is_test_func = _is_test_function(func_name)

        func_node = NodeInfo(
            kind=kind,
            name=func_name,
            qualified_name=qualified,
            file_path=fp,
            line_start=node.start_point[0] + 1,
            line_end=node.end_point[0] + 1,
            language="python",
            parent_name=parent_name,
            params=params,
            return_type=return_type,
            modifiers=modifiers,
            is_test=is_test_file or is_test_func,
        )
        nodes.append(func_node)

        # CONTAINS edge: parent -> function/method
        edges.append(EdgeInfo(
            kind="CONTAINS",
            source_qualified=parent_qualified,
            target_qualified=qualified,
            file_path=fp,
            line=node.start_point[0] + 1,
        ))

        # Also file -> function (top-level only)
        if context == "file":
            # Already covered by parent_qualified == file_qualified above.
            pass
        elif context == "class":
            # The file->class CONTAINS edge was already added; class->method is the
            # CONTAINS edge we just added above.
            pass

        # Collect CALLS within the function body
        body = _get_child_by_field(node, "body")
        if body is not None:
            self._collect_calls_in_subtree(body, source, fp, qualified, edges)

            # Recurse for nested functions/classes
            self._visit_body(
                body_nodes=body.children,
                source=source,
                fp=fp,
                file_qualified=file_qualified,
                parent_qualified=qualified,
                parent_name=symbol_name,
                context="function",
                is_test_file=is_test_file,
                nodes=nodes,
                edges=edges,
            )

    # ------------------------------------------------------------------
    # Import handling
    # ------------------------------------------------------------------

    def _handle_import(
        self,
        node,
        source: bytes,
        fp: str,
        source_qualified: str,
        edges: list[EdgeInfo],
    ) -> None:
        if node.type == "import_statement":
            # import foo, import foo as bar, import foo.bar
            for child in node.children:
                if child.type in ("dotted_name", "aliased_import"):
                    if child.type == "aliased_import":
                        name_node = child.children[0]  # the module part
                        module = _node_text(name_node, source)
                    else:
                        module = _node_text(child, source)
                    edges.append(EdgeInfo(
                        kind="IMPORTS",
                        source_qualified=source_qualified,
                        target_qualified=module,
                        file_path=fp,
                        line=node.start_point[0] + 1,
                        extra={"import_type": "module"},
                    ))

        elif node.type == "import_from_statement":
            # from foo import bar, baz
            module_node = _get_child_by_field(node, "module_name")
            module = _node_text(module_node, source) if module_node else ""

            # Collect imported names
            imported_names: list[str] = []
            for child in node.children:
                if child.type == "dotted_name" and child != module_node:
                    imported_names.append(_node_text(child, source))
                elif child.type == "aliased_import":
                    name_node = child.children[0]
                    imported_names.append(_node_text(name_node, source))
                elif child.type == "wildcard_import":
                    imported_names.append("*")

            if not imported_names:
                # from foo import (...)
                for child in node.children:
                    if child.type == "import_list":
                        for item in child.children:
                            if item.type in ("dotted_name", "identifier"):
                                imported_names.append(_node_text(item, source))
                            elif item.type == "aliased_import":
                                imported_names.append(_node_text(item.children[0], source))

            for name in imported_names:
                target = f"{module}.{name}" if module and name != "*" else (module or name)
                edges.append(EdgeInfo(
                    kind="IMPORTS",
                    source_qualified=source_qualified,
                    target_qualified=target,
                    file_path=fp,
                    line=node.start_point[0] + 1,
                    extra={"import_type": "from", "module": module},
                ))

    # ------------------------------------------------------------------
    # Call collection
    # ------------------------------------------------------------------

    def _collect_calls(
        self,
        call_node,
        source: bytes,
        fp: str,
        source_qualified: str,
        edges: list[EdgeInfo],
    ) -> None:
        """Record a single call node as a CALLS edge."""
        func_node = _get_child_by_field(call_node, "function")
        if func_node is None:
            return

        if func_node.type == "identifier":
            callee = _node_text(func_node, source)
        elif func_node.type == "attribute":
            # e.g. self.foo() or obj.method()
            callee = _node_text(func_node, source)
        else:
            callee = _node_text(func_node, source)

        if not callee:
            return

        edges.append(EdgeInfo(
            kind="CALLS",
            source_qualified=source_qualified,
            target_qualified=callee,
            file_path=fp,
            line=call_node.start_point[0] + 1,
        ))

    def _collect_calls_in_subtree(
        self,
        node,
        source: bytes,
        fp: str,
        source_qualified: str,
        edges: list[EdgeInfo],
        _depth: int = 0,
    ) -> None:
        """Walk a subtree and collect all call expressions, skipping nested defs."""
        # Avoid diving into nested function/class bodies at depth > 0
        # (they get their own node + qualified name from _visit_body)
        if _depth > 0 and node.type in (
            "function_definition", "async_function_def", "class_definition"
        ):
            return

        if node.type == "call":
            self._collect_calls(node, source, fp, source_qualified, edges)
            # Still recurse — arguments may contain further calls
            for child in node.children:
                self._collect_calls_in_subtree(
                    child, source, fp, source_qualified, edges, _depth + 1
                )
            return

        for child in node.children:
            self._collect_calls_in_subtree(
                child, source, fp, source_qualified, edges, _depth + 1
            )


# Module-level singleton used by the registry
python_parser = PythonParser()
