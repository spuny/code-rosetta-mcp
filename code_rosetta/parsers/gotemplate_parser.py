"""Go template parser for Helm charts.

Extracts template definitions, include/template calls, and .Values references
from Helm chart templates and _helpers.tpl files. Uses regex since Go templates
are embedded in YAML and can't be parsed by tree-sitter.
"""

from __future__ import annotations

import re
from pathlib import Path

from code_rosetta.models import EdgeInfo, NodeInfo, make_qualified

# Patterns for Go template directives
_DEFINE_RE = re.compile(r'\{\{-?\s*define\s+"([^"]+)"')
_INCLUDE_RE = re.compile(r'\{\{-?\s*include\s+"([^"]+)"')
_TEMPLATE_RE = re.compile(r'\{\{-?\s*template\s+"([^"]+)"')
_VALUES_RE = re.compile(r'\.Values\.([\w.]+)')
_DOLLAR_VALUES_RE = re.compile(r'\$\.Values\.([\w.]+)')
_RANGE_VALUES_RE = re.compile(r'\{\{-?\s*range\s+[^}]*\.Values\.([\w.]+)')
_IF_VALUES_RE = re.compile(r'\{\{-?\s*(?:if|with)\s+[^}]*\.Values\.([\w.]+)')


def _is_helm_template(file_path: Path) -> bool:
    """Check if a file is likely a Helm Go template."""
    # Files in templates/ directory, or _helpers.tpl
    parts = file_path.parts
    if file_path.name == "_helpers.tpl":
        return True
    if "templates" in parts:
        return True
    return False


def _has_go_template_syntax(source: str) -> bool:
    """Quick check if file contains Go template syntax."""
    return "{{" in source and "}}" in source


class GoTemplateParser:
    language = "gotemplate"
    extensions = [".tpl"]
    # .yaml/.yml files in templates/ dirs are handled by detection in parse()

    def parse(self, file_path: Path, source: bytes) -> tuple[list[NodeInfo], list[EdgeInfo]]:
        nodes: list[NodeInfo] = []
        edges: list[EdgeInfo] = []
        fp = str(file_path)
        text = source.decode("utf-8", errors="replace")

        if not _has_go_template_syntax(text):
            return nodes, edges

        lines = text.splitlines()

        # File node
        file_qname = make_qualified(fp, "File", file_path.name)
        nodes.append(NodeInfo(
            kind="File",
            name=file_path.name,
            qualified_name=file_qname,
            file_path=fp,
            line_start=1,
            line_end=len(lines),
            language=self.language,
        ))

        # Collect all values references for the file's extra/keywords
        all_values_refs: set[str] = set()
        # Collect template definitions and calls
        defined_templates: dict[str, int] = {}  # name -> line
        included_templates: list[tuple[str, int]] = []  # (name, line)

        for line_num, line in enumerate(lines, 1):
            # Template definitions: {{ define "name" }}
            for m in _DEFINE_RE.finditer(line):
                tpl_name = m.group(1)
                defined_templates[tpl_name] = line_num

                tpl_qname = make_qualified(fp, "Template", tpl_name)
                nodes.append(NodeInfo(
                    kind="Template",
                    name=tpl_name,
                    qualified_name=tpl_qname,
                    file_path=fp,
                    line_start=line_num,
                    line_end=line_num,
                    language=self.language,
                ))
                edges.append(EdgeInfo(
                    kind="CONTAINS",
                    source_qualified=file_qname,
                    target_qualified=tpl_qname,
                    file_path=fp,
                    line=line_num,
                ))

            # Template includes: {{ include "name" }} or {{ template "name" }}
            for m in _INCLUDE_RE.finditer(line):
                included_templates.append((m.group(1), line_num))
            for m in _TEMPLATE_RE.finditer(line):
                included_templates.append((m.group(1), line_num))

            # .Values references
            for m in _VALUES_RE.finditer(line):
                all_values_refs.add(m.group(1))
            for m in _DOLLAR_VALUES_RE.finditer(line):
                all_values_refs.add(m.group(1))

        # Create edges for template includes
        for tpl_name, line_num in included_templates:
            # If the template is defined in this file, use local qname
            if tpl_name in defined_templates:
                target = make_qualified(fp, "Template", tpl_name)
            else:
                # Cross-file template reference -- use a placeholder
                target = f"gotemplate::{tpl_name}"

            edges.append(EdgeInfo(
                kind="CALLS",
                source_qualified=file_qname,
                target_qualified=target,
                file_path=fp,
                line=line_num,
            ))

        # Create edges for .Values references (link to values.yaml concept)
        seen_values: set[str] = set()
        for val_ref in sorted(all_values_refs):
            # Deduplicate and create references to the top-level values key
            top_key = val_ref.split(".")[0]
            if top_key not in seen_values:
                seen_values.add(top_key)
                edges.append(EdgeInfo(
                    kind="READS_CONFIG",
                    source_qualified=file_qname,
                    target_qualified=f"values::{top_key}",
                    file_path=fp,
                    line=0,
                    extra={"values_paths": sorted(
                        v for v in all_values_refs if v.startswith(top_key)
                    )},
                ))

        # Store values refs in file node extra for keyword search
        if all_values_refs:
            # Update the file node with values references
            nodes[0] = NodeInfo(
                kind=nodes[0].kind,
                name=nodes[0].name,
                qualified_name=nodes[0].qualified_name,
                file_path=nodes[0].file_path,
                line_start=nodes[0].line_start,
                line_end=nodes[0].line_end,
                language=nodes[0].language,
                extra={
                    "values_refs": sorted(all_values_refs),
                    "templates_defined": list(defined_templates.keys()),
                    "templates_included": list({t[0] for t in included_templates}),
                },
            )

        return nodes, edges


gotemplate_parser = GoTemplateParser()
