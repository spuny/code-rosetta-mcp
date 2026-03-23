"""Jinja2 template parser — extracts nodes and edges from .j2/.jinja2/.jinja files."""

from __future__ import annotations

from pathlib import Path
from typing import Any

import jinja2.nodes
from jinja2 import Environment, TemplateSyntaxError

from code_rosetta.models import EdgeInfo, NodeInfo, make_qualified


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _max_lineno(node: jinja2.nodes.Node) -> int:
    """Return the highest lineno found in *node* and all its descendants."""
    best = getattr(node, "lineno", 1) or 1
    for child in node.iter_child_nodes():
        best = max(best, _max_lineno(child))
    return best


def _node_lineno(node: jinja2.nodes.Node) -> int:
    return getattr(node, "lineno", 1) or 1


def _collect_names_in_subtree(
    node: jinja2.nodes.Node,
    *,
    stop_at: type | tuple[type, ...] | None = None,
) -> list[tuple[str, int]]:
    """
    Walk *node* recursively and collect (name, lineno) for every
    jinja2.nodes.Name encountered.

    *stop_at* — do not descend into nodes of these types (used to avoid
    descending into nested Block / Macro definitions while collecting names
    for the parent scope).
    """
    results: list[tuple[str, int]] = []

    def _walk(n: jinja2.nodes.Node) -> None:
        if isinstance(n, jinja2.nodes.Name):
            results.append((n.name, _node_lineno(n)))
        for child in n.iter_child_nodes():
            if stop_at and isinstance(child, stop_at):
                continue
            _walk(child)

    _walk(node)
    return results


def _collect_calls_in_subtree(
    node: jinja2.nodes.Node,
    *,
    stop_at: type | tuple[type, ...] | None = None,
) -> list[tuple[str, int]]:
    """
    Return (callee_name, lineno) for every direct-name Call node found under
    *node* (e.g.  ``{{ my_macro(...) }}``).
    """
    results: list[tuple[str, int]] = []

    def _walk(n: jinja2.nodes.Node) -> None:
        if isinstance(n, jinja2.nodes.Call):
            if isinstance(n.node, jinja2.nodes.Name):
                results.append((n.node.name, _node_lineno(n)))
        for child in n.iter_child_nodes():
            if stop_at and isinstance(child, stop_at):
                continue
            _walk(child)

    _walk(node)
    return results


# ---------------------------------------------------------------------------
# Parser
# ---------------------------------------------------------------------------

class Jinja2Parser:
    """Implements the LanguageParser protocol for Jinja2 templates."""

    language = "jinja2"
    extensions = [".j2", ".jinja2", ".jinja"]

    def parse(
        self, file_path: Path, source: bytes
    ) -> tuple[list[NodeInfo], list[EdgeInfo]]:
        nodes: list[NodeInfo] = []
        edges: list[EdgeInfo] = []

        fp = str(file_path)
        file_qn = make_qualified(fp, "File", fp)

        # --- File node ---------------------------------------------------
        try:
            source_text = source.decode("utf-8")
        except UnicodeDecodeError:
            source_text = source.decode("latin-1")

        total_lines = max(source_text.count("\n") + 1, 1)

        nodes.append(
            NodeInfo(
                kind="File",
                name=file_path.name,
                qualified_name=file_qn,
                file_path=fp,
                line_start=1,
                line_end=total_lines,
                language=self.language,
            )
        )

        # --- Parse Jinja2 AST --------------------------------------------
        env = Environment()
        try:
            ast = env.parse(source_text)
        except TemplateSyntaxError as exc:
            # Gracefully return what we have (just the File node).
            nodes[0].extra["parse_error"] = str(exc)
            return nodes, edges

        # --- Template node -----------------------------------------------
        template_name = "template"
        template_qn = make_qualified(fp, "Template", template_name)

        nodes.append(
            NodeInfo(
                kind="Template",
                name=template_name,
                qualified_name=template_qn,
                file_path=fp,
                line_start=1,
                line_end=total_lines,
                language=self.language,
            )
        )

        # file -> template
        edges.append(
            EdgeInfo(
                kind="CONTAINS",
                source_qualified=file_qn,
                target_qualified=template_qn,
                file_path=fp,
                line=1,
            )
        )

        # --- Collect top-level structural nodes --------------------------
        # We need to know all macro names up-front to distinguish CALLS_MACRO
        # from generic variable references.
        macro_names: set[str] = set()
        for node_ast in ast.find_all(jinja2.nodes.Macro):
            macro_names.add(node_ast.name)

        # Track qualified names for blocks and macros so we can build edges
        # into them from variable / call references.
        block_qnames: dict[str, str] = {}   # block_name -> qualified_name
        macro_qnames: dict[str, str] = {}   # macro_name -> qualified_name

        # --- Extends -----------------------------------------------------
        for ext_node in ast.find_all(jinja2.nodes.Extends):
            parent_template = _template_ref(ext_node.template)
            ext_name = f"extends:{parent_template}"
            ext_qn = make_qualified(fp, "Extends", ext_name)

            nodes.append(
                NodeInfo(
                    kind="Extends",
                    name=ext_name,
                    qualified_name=ext_qn,
                    file_path=fp,
                    line_start=_node_lineno(ext_node),
                    line_end=_node_lineno(ext_node),
                    language=self.language,
                    extra={"parent": parent_template},
                )
            )

            edges.append(
                EdgeInfo(
                    kind="EXTENDS",
                    source_qualified=template_qn,
                    target_qualified=ext_qn,
                    file_path=fp,
                    line=_node_lineno(ext_node),
                    extra={"parent_template": parent_template},
                )
            )

        # --- Includes ----------------------------------------------------
        for inc_node in ast.find_all(jinja2.nodes.Include):
            included = _template_ref(inc_node.template)
            inc_name = f"include:{included}"
            inc_qn = make_qualified(fp, "Include", inc_name)

            nodes.append(
                NodeInfo(
                    kind="Include",
                    name=inc_name,
                    qualified_name=inc_qn,
                    file_path=fp,
                    line_start=_node_lineno(inc_node),
                    line_end=_node_lineno(inc_node),
                    language=self.language,
                    extra={"included": included},
                )
            )

            edges.append(
                EdgeInfo(
                    kind="INCLUDES",
                    source_qualified=template_qn,
                    target_qualified=inc_qn,
                    file_path=fp,
                    line=_node_lineno(inc_node),
                    extra={"included_template": included},
                )
            )

        # --- Blocks ------------------------------------------------------
        for blk_node in ast.find_all(jinja2.nodes.Block):
            blk_name = blk_node.name
            blk_qn = make_qualified(fp, "Block", f"block:{blk_name}")
            block_qnames[blk_name] = blk_qn

            blk_start = _node_lineno(blk_node)
            blk_end = _max_lineno(blk_node)

            nodes.append(
                NodeInfo(
                    kind="Block",
                    name=blk_name,
                    qualified_name=blk_qn,
                    file_path=fp,
                    line_start=blk_start,
                    line_end=blk_end,
                    language=self.language,
                    parent_name=template_name,
                )
            )

            # template -> block
            edges.append(
                EdgeInfo(
                    kind="CONTAINS",
                    source_qualified=template_qn,
                    target_qualified=blk_qn,
                    file_path=fp,
                    line=blk_start,
                )
            )

            # Variables used inside this block (don't recurse into nested
            # Block/Macro definitions — they get their own scope entry).
            _add_variable_edges(
                blk_node,
                scope_qn=blk_qn,
                scope_name=blk_name,
                fp=fp,
                nodes=nodes,
                edges=edges,
                macro_names=macro_names,
                macro_qnames=macro_qnames,
                stop_at=(jinja2.nodes.Block, jinja2.nodes.Macro),
            )

        # --- Macros ------------------------------------------------------
        for mac_node in ast.find_all(jinja2.nodes.Macro):
            mac_name = mac_node.name
            mac_qn = make_qualified(fp, "Macro", f"macro:{mac_name}")
            macro_qnames[mac_name] = mac_qn

            mac_start = _node_lineno(mac_node)
            mac_end = _max_lineno(mac_node)

            # Build params string from argument names
            params = ", ".join(
                arg.name for arg in mac_node.args
                if isinstance(arg, jinja2.nodes.Name)
            )

            nodes.append(
                NodeInfo(
                    kind="Macro",
                    name=mac_name,
                    qualified_name=mac_qn,
                    file_path=fp,
                    line_start=mac_start,
                    line_end=mac_end,
                    language=self.language,
                    parent_name=template_name,
                    params=params,
                )
            )

            # template -> macro
            edges.append(
                EdgeInfo(
                    kind="CONTAINS",
                    source_qualified=template_qn,
                    target_qualified=mac_qn,
                    file_path=fp,
                    line=mac_start,
                )
            )

            # Variables / calls inside this macro body
            _add_variable_edges(
                mac_node,
                scope_qn=mac_qn,
                scope_name=mac_name,
                fp=fp,
                nodes=nodes,
                edges=edges,
                macro_names=macro_names,
                macro_qnames=macro_qnames,
                stop_at=(jinja2.nodes.Block, jinja2.nodes.Macro),
            )

        # --- Template-level variables (outside blocks/macros) ------------
        _add_variable_edges(
            ast,
            scope_qn=template_qn,
            scope_name=template_name,
            fp=fp,
            nodes=nodes,
            edges=edges,
            macro_names=macro_names,
            macro_qnames=macro_qnames,
            stop_at=(jinja2.nodes.Block, jinja2.nodes.Macro),
        )

        return nodes, edges


# ---------------------------------------------------------------------------
# Internal helpers
# ---------------------------------------------------------------------------

def _template_ref(node: jinja2.nodes.Node) -> str:
    """Extract a string template reference from a Const or similar node."""
    if isinstance(node, jinja2.nodes.Const):
        return str(node.value)
    # Fallback for dynamic includes / extends (expressions)
    return "<dynamic>"


def _add_variable_edges(
    scope_ast: jinja2.nodes.Node,
    *,
    scope_qn: str,
    scope_name: str,
    fp: str,
    nodes: list[NodeInfo],
    edges: list[EdgeInfo],
    macro_names: set[str],
    macro_qnames: dict[str, str],
    stop_at: type | tuple[type, ...] | None = None,
) -> None:
    """
    Collect variable references and macro calls inside *scope_ast* and emit
    Variable nodes + USES_VARIABLE / CALLS_MACRO edges.

    Variable nodes are deduplicated within the scope (one node per unique
    variable name).
    """
    # --- Variable references ---------------------------------------------
    seen_vars: dict[str, int] = {}  # name -> first lineno
    for (var_name, lineno) in _collect_names_in_subtree(scope_ast, stop_at=stop_at):
        if var_name not in seen_vars:
            seen_vars[var_name] = lineno

    for var_name, first_lineno in seen_vars.items():
        var_qn = make_qualified(fp, "Variable", f"var:{var_name}")

        # Only create the Variable node if it hasn't already been added by
        # another scope — check via qualified name uniqueness.  Since we
        # cannot easily share state here without a more complex refactor,
        # we emit a node per (scope, variable) by using a scope-qualified name
        # to guarantee uniqueness.
        scoped_var_qn = make_qualified(fp, "Variable", f"var:{scope_name}:{var_name}")

        nodes.append(
            NodeInfo(
                kind="Variable",
                name=var_name,
                qualified_name=scoped_var_qn,
                file_path=fp,
                line_start=first_lineno,
                line_end=first_lineno,
                language="jinja2",
                parent_name=scope_name,
            )
        )

        edges.append(
            EdgeInfo(
                kind="USES_VARIABLE",
                source_qualified=scope_qn,
                target_qualified=scoped_var_qn,
                file_path=fp,
                line=first_lineno,
                extra={"variable": var_name},
            )
        )

    # --- Macro calls -----------------------------------------------------
    for (callee_name, lineno) in _collect_calls_in_subtree(scope_ast, stop_at=stop_at):
        if callee_name not in macro_names:
            continue
        # The macro_qnames dict may not be populated yet when we process blocks
        # that appear before macro definitions in the template; use the same
        # qualified name formula so the edge still resolves correctly later.
        callee_qn = macro_qnames.get(
            callee_name,
            make_qualified(fp, "Macro", f"macro:{callee_name}"),
        )
        edges.append(
            EdgeInfo(
                kind="CALLS_MACRO",
                source_qualified=scope_qn,
                target_qualified=callee_qn,
                file_path=fp,
                line=lineno,
                extra={"macro": callee_name},
            )
        )


# ---------------------------------------------------------------------------
# Singleton instance (imported by the registry)
# ---------------------------------------------------------------------------

jinja2_parser = Jinja2Parser()
