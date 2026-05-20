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
# Scope context for call resolution
# ---------------------------------------------------------------------------

class _FileScope:
    """Tracks imports and local definitions for resolving call targets."""

    def __init__(self, file_path: str) -> None:
        self.file_path = file_path
        # import alias -> module path (e.g. 'pd' -> 'pandas', 'Path' -> 'pathlib.Path')
        self.import_map: dict[str, str] = {}
        # local names defined in this file -> qualified name
        self.local_defs: dict[str, str] = {}
        # class names defined in this file
        self.classes: set[str] = set()

    def register_import(self, alias: str, module_path: str) -> None:
        self.import_map[alias] = module_path

    def register_def(self, name: str, qualified_name: str) -> None:
        self.local_defs[name] = qualified_name

    def register_class(self, name: str) -> None:
        self.classes.add(name)

    def resolve_call(self, raw_target: str, enclosing_class: str) -> str:
        """Resolve a raw call target to a qualified name.

        Resolution order:
        1. self.method() -> EnclosingClass.method (same-file method)
        2. bare_name() -> same-file definition if exists
        3. bare_name() -> imported name if exists
        4. obj.method() -> if obj is an imported module, resolve to module.method
        5. ClassName.method() -> if ClassName is local, resolve to file-qualified
        6. Fall through -> return raw target unchanged
        """
        # 1. self.method() -> resolve to enclosing class method
        if raw_target.startswith("self.") and enclosing_class:
            method = raw_target[5:]  # strip 'self.'
            # Could be chained: self.foo.bar -- only resolve self.method
            if "." not in method:
                local_key = f"{enclosing_class}.{method}"
                if local_key in self.local_defs:
                    return self.local_defs[local_key]
                # Method might not be defined yet (forward ref) -- construct qualified name
                return make_qualified(self.file_path, "Method", local_key)
            # self.foo.bar -- can't resolve further
            return raw_target

        # 2. cls.method() for classmethods
        if raw_target.startswith("cls.") and enclosing_class:
            method = raw_target[4:]
            if "." not in method:
                local_key = f"{enclosing_class}.{method}"
                if local_key in self.local_defs:
                    return self.local_defs[local_key]
                return make_qualified(self.file_path, "Method", local_key)
            return raw_target

        # 3. bare name -> same-file definition
        if "." not in raw_target and raw_target in self.local_defs:
            return self.local_defs[raw_target]

        # 4. bare name -> imported name
        if "." not in raw_target and raw_target in self.import_map:
            return self.import_map[raw_target]

        # 5. dotted: obj.method()
        if "." in raw_target:
            parts = raw_target.split(".", 1)
            prefix, rest = parts[0], parts[1]

            # prefix is a local class -> resolve to class.method in this file
            if prefix in self.classes:
                local_key = f"{prefix}.{rest}"
                if local_key in self.local_defs:
                    return self.local_defs[local_key]
                # Assume it's a method
                return make_qualified(self.file_path, "Method", local_key)

            # prefix is an imported module/name -> resolve to module.rest
            if prefix in self.import_map:
                return f"{self.import_map[prefix]}.{rest}"

        # 6. Can't resolve -- return as-is
        return raw_target


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
        scope = _FileScope(fp)

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
            scope=scope,
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
        scope: _FileScope | None = None,
    ) -> None:
        for node in body_nodes:
            if node.type in ("function_definition", "async_function_def"):
                self._handle_function(
                    node, [], source, fp, file_qualified, parent_qualified,
                    parent_name, context, is_test_file, nodes, edges, scope,
                )
            elif node.type == "decorated_definition":
                self._handle_decorated(
                    node, source, fp, file_qualified, parent_qualified,
                    parent_name, context, is_test_file, nodes, edges, scope,
                )
            elif node.type == "class_definition":
                self._handle_class(
                    node, [], source, fp, file_qualified, parent_qualified,
                    parent_name, is_test_file, nodes, edges, scope,
                )
            elif node.type in ("import_statement", "import_from_statement"):
                self._handle_import(
                    node, source, fp, parent_qualified, edges, scope,
                )
            elif node.type == "expression_statement":
                # Could contain calls at module/class level
                for child in node.children:
                    if child.type == "call":
                        self._collect_calls(
                            child, source, fp, parent_qualified, edges,
                            scope, parent_name if context == "class" else "",
                        )
            elif node.type == "assignment":
                # Handle calls on the right-hand side
                value = _get_child_by_field(node, "right")
                if value and value.type == "call":
                    self._collect_calls(
                        value, source, fp, parent_qualified, edges,
                        scope, parent_name if context == "class" else "",
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
        scope: _FileScope | None = None,
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
                parent_name, is_test_file, nodes, edges, scope,
            )
        else:
            self._handle_function(
                inner, decorators, source, fp, file_qualified, parent_qualified,
                parent_name, context, is_test_file, nodes, edges, scope,
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
        scope: _FileScope | None = None,
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

        # Register in scope
        if scope:
            scope.register_def(class_name, qualified)
            scope.register_class(class_name)

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
                    # Resolve through scope (same as calls)
                    if scope:
                        resolved_base = scope.resolve_call(base_name, "")
                    else:
                        resolved_base = base_name
                    edges.append(EdgeInfo(
                        kind="INHERITS",
                        source_qualified=qualified,
                        target_qualified=resolved_base,
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
                scope=scope,
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
        scope: _FileScope | None = None,
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

        # Register in scope
        if scope:
            scope.register_def(symbol_name, qualified)
            # Also register by bare function name for top-level functions
            if context == "file":
                scope.register_def(func_name, qualified)

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

        # Determine enclosing class for self.x resolution
        enclosing_class = parent_name if context == "class" else ""

        # Collect CALLS within the function body
        body = _get_child_by_field(node, "body")
        if body is not None:
            self._collect_calls_in_subtree(
                body, source, fp, qualified, edges, scope, enclosing_class,
            )

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
                scope=scope,
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
        scope: _FileScope | None = None,
    ) -> None:
        if node.type == "import_statement":
            # import foo, import foo as bar, import foo.bar
            for child in node.children:
                if child.type in ("dotted_name", "aliased_import"):
                    if child.type == "aliased_import":
                        name_node = child.children[0]  # the module part
                        module = _node_text(name_node, source)
                        # Get alias: import foo as bar -> alias='bar'
                        alias_node = child.children[-1] if len(child.children) >= 3 else None
                        alias = _node_text(alias_node, source) if alias_node else module.split(".")[-1]
                    else:
                        module = _node_text(child, source)
                        alias = module.split(".")[0]  # import foo.bar -> alias='foo'
                    edges.append(EdgeInfo(
                        kind="IMPORTS",
                        source_qualified=source_qualified,
                        target_qualified=module,
                        file_path=fp,
                        line=node.start_point[0] + 1,
                        extra={"import_type": "module"},
                    ))
                    if scope:
                        scope.register_import(alias, module)

        elif node.type == "import_from_statement":
            # from foo import bar, baz
            module_node = _get_child_by_field(node, "module_name")
            module = _node_text(module_node, source) if module_node else ""

            # Collect imported names (and their aliases)
            imported: list[tuple[str, str]] = []  # (name, alias)
            for child in node.children:
                if child.type == "dotted_name" and child != module_node:
                    name = _node_text(child, source)
                    imported.append((name, name))
                elif child.type == "aliased_import":
                    name_node = child.children[0]
                    name = _node_text(name_node, source)
                    alias_node = child.children[-1] if len(child.children) >= 3 else None
                    alias = _node_text(alias_node, source) if alias_node else name
                    imported.append((name, alias))
                elif child.type == "wildcard_import":
                    imported.append(("*", "*"))

            if not imported:
                # from foo import (...)
                for child in node.children:
                    if child.type == "import_list":
                        for item in child.children:
                            if item.type in ("dotted_name", "identifier"):
                                name = _node_text(item, source)
                                imported.append((name, name))
                            elif item.type == "aliased_import":
                                name = _node_text(item.children[0], source)
                                alias_node = item.children[-1] if len(item.children) >= 3 else None
                                alias = _node_text(alias_node, source) if alias_node else name
                                imported.append((name, alias))

            for name, alias in imported:
                target = f"{module}.{name}" if module and name != "*" else (module or name)
                edges.append(EdgeInfo(
                    kind="IMPORTS",
                    source_qualified=source_qualified,
                    target_qualified=target,
                    file_path=fp,
                    line=node.start_point[0] + 1,
                    extra={"import_type": "from", "module": module},
                ))
                if scope and name != "*":
                    scope.register_import(alias, target)

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
        scope: _FileScope | None = None,
        enclosing_class: str = "",
    ) -> None:
        """Record a single call node as a CALLS edge, resolving through scope."""
        func_node = _get_child_by_field(call_node, "function")
        if func_node is None:
            return

        raw_callee = _node_text(func_node, source)
        if not raw_callee:
            return

        # Resolve through scope
        if scope:
            resolved = scope.resolve_call(raw_callee, enclosing_class)
        else:
            resolved = raw_callee

        edges.append(EdgeInfo(
            kind="CALLS",
            source_qualified=source_qualified,
            target_qualified=resolved,
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
        scope: _FileScope | None = None,
        enclosing_class: str = "",
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
            self._collect_calls(
                node, source, fp, source_qualified, edges, scope, enclosing_class,
            )
            # Still recurse -- arguments may contain further calls
            for child in node.children:
                self._collect_calls_in_subtree(
                    child, source, fp, source_qualified, edges,
                    scope, enclosing_class, _depth + 1,
                )
            return

        for child in node.children:
            self._collect_calls_in_subtree(
                child, source, fp, source_qualified, edges,
                scope, enclosing_class, _depth + 1,
            )


# Module-level singleton used by the registry
python_parser = PythonParser()
