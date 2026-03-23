"""HCL/Terraform parser — extracts nodes and edges from .tf and .tfvars files.

Uses python-hcl2 to parse Terraform configuration into nested dicts, then
walks the structure to produce NodeInfo/EdgeInfo entries for the codebase graph.

Supported constructs:
    resource, data, module, variable, output, provider, locals, terraform
"""

from __future__ import annotations

import io
import re
from pathlib import Path
from typing import Any

import hcl2

from code_rosetta.models import EdgeInfo, NodeInfo, make_qualified

# ---------------------------------------------------------------------------
# Reference patterns found inside HCL string values / expressions
# ---------------------------------------------------------------------------

# Matches: var.NAME
_RE_VAR = re.compile(r"\bvar\.([A-Za-z0-9_\-]+)")
# Matches: local.NAME
_RE_LOCAL = re.compile(r"\blocal\.([A-Za-z0-9_\-]+)")
# Matches: module.NAME  (optionally .OUTPUT after)
_RE_MODULE = re.compile(r"\bmodule\.([A-Za-z0-9_\-]+)")
# Matches: data.TYPE.NAME  (optionally .ATTR after)
_RE_DATA = re.compile(r"\bdata\.([A-Za-z0-9_\-]+)\.([A-Za-z0-9_\-]+)")
# Matches: TYPE.NAME.ATTR — resource references like aws_instance.web.id
# Must have at least two dots and not start with var/local/module/data
_RE_RESOURCE_REF = re.compile(
    r"\b(?!var\.|local\.|module\.|data\.)([a-z][A-Za-z0-9_]*)\.([A-Za-z0-9_\-]+)\.[A-Za-z0-9_\-]+"
)

# Keys inside a module block that are NOT variable pass-throughs
_MODULE_META_KEYS = frozenset(
    {"source", "version", "providers", "depends_on", "count", "for_each"}
)


class HCLParser:
    """Parser for Terraform HCL files (.tf, .tfvars)."""

    language = "hcl"
    extensions = [".tf", ".tfvars"]

    # ------------------------------------------------------------------
    # Public interface
    # ------------------------------------------------------------------

    def parse(
        self, file_path: Path, source: bytes
    ) -> tuple[list[NodeInfo], list[EdgeInfo]]:
        """Parse an HCL file and return (nodes, edges)."""
        fp = str(file_path)
        nodes: list[NodeInfo] = []
        edges: list[EdgeInfo] = []

        # File node — always present
        file_qn = make_qualified(fp, "File", fp)
        nodes.append(
            NodeInfo(
                kind="File",
                name=file_path.name,
                qualified_name=file_qn,
                file_path=fp,
                line_start=0,
                line_end=0,
                language=self.language,
            )
        )

        # Parse HCL; return just the file node on syntax errors
        try:
            parsed: dict[str, Any] = hcl2.load(io.StringIO(source.decode("utf-8")))
        except Exception:
            return nodes, edges

        # Dispatch each top-level block type
        handlers = {
            "resource": self._handle_resource,
            "data": self._handle_data,
            "module": self._handle_module,
            "variable": self._handle_variable,
            "output": self._handle_output,
            "provider": self._handle_provider,
            "locals": self._handle_locals,
            "terraform": self._handle_terraform,
        }

        for block_type, items in parsed.items():
            handler = handlers.get(block_type)
            if handler is None:
                continue
            # python-hcl2 wraps every block type in a list of dicts
            for item in items:
                block_nodes, block_edges = handler(fp, item)
                nodes.extend(block_nodes)
                edges.extend(block_edges)

        # CONTAINS edges: file -> each non-file node
        for node in nodes:
            if node.kind == "File":
                continue
            edges.append(
                EdgeInfo(
                    kind="CONTAINS",
                    source_qualified=file_qn,
                    target_qualified=node.qualified_name,
                    file_path=fp,
                )
            )

        # REFERENCES edges from string interpolations inside all block bodies
        ref_edges = self._extract_references(fp, file_qn, parsed, nodes)
        edges.extend(ref_edges)

        return nodes, edges

    # ------------------------------------------------------------------
    # Block handlers
    # ------------------------------------------------------------------

    def _handle_resource(
        self, fp: str, item: dict
    ) -> tuple[list[NodeInfo], list[EdgeInfo]]:
        """resource "aws_iam_role" "my_role" { ... }"""
        nodes: list[NodeInfo] = []
        for resource_type, instances in item.items():
            provider = resource_type.split("_")[0]
            for resource_name, body in instances.items():
                name = f"{resource_type}.{resource_name}"
                qn = make_qualified(fp, "Resource", name)
                nodes.append(
                    NodeInfo(
                        kind="Resource",
                        name=name,
                        qualified_name=qn,
                        file_path=fp,
                        line_start=0,
                        line_end=0,
                        language="hcl",
                        extra={
                            "provider": provider,
                            "resource_type": resource_type,
                        },
                    )
                )
        return nodes, []

    def _handle_data(
        self, fp: str, item: dict
    ) -> tuple[list[NodeInfo], list[EdgeInfo]]:
        """data "aws_ami" "ubuntu" { ... }"""
        nodes: list[NodeInfo] = []
        for data_type, instances in item.items():
            provider = data_type.split("_")[0]
            for data_name, body in instances.items():
                name = f"{data_type}.{data_name}"
                qn = make_qualified(fp, "DataSource", name)
                nodes.append(
                    NodeInfo(
                        kind="DataSource",
                        name=name,
                        qualified_name=qn,
                        file_path=fp,
                        line_start=0,
                        line_end=0,
                        language="hcl",
                        extra={
                            "provider": provider,
                            "data_type": data_type,
                        },
                    )
                )
        return nodes, []

    def _handle_module(
        self, fp: str, item: dict
    ) -> tuple[list[NodeInfo], list[EdgeInfo]]:
        """module "vpc" { source = "./modules/vpc" ... }"""
        nodes: list[NodeInfo] = []
        edges: list[EdgeInfo] = []
        for module_name, body in item.items():
            qn = make_qualified(fp, "Module", module_name)
            source = body.get("source", "") if isinstance(body, dict) else ""
            nodes.append(
                NodeInfo(
                    kind="Module",
                    name=module_name,
                    qualified_name=qn,
                    file_path=fp,
                    line_start=0,
                    line_end=0,
                    language="hcl",
                    extra={"source": source},
                )
            )

            if isinstance(body, dict):
                # USES_MODULE edge for local path sources
                if source and source.startswith(("./", "../")):
                    # Resolve to actual file path (module dir + main.tf)
                    source_dir = Path(fp).parent / source
                    resolved_main = source_dir / "main.tf"
                    target = str(resolved_main) if resolved_main.exists() else source
                    edges.append(
                        EdgeInfo(
                            kind="USES_MODULE",
                            source_qualified=qn,
                            target_qualified=target,
                            file_path=fp,
                            extra={"source_path": source},
                        )
                    )
                # PASSES_VAR edges for each non-meta key
                for key, value in body.items():
                    if key not in _MODULE_META_KEYS:
                        edges.append(
                            EdgeInfo(
                                kind="PASSES_VAR",
                                source_qualified=qn,
                                target_qualified=make_qualified(fp, "Variable", key),
                                file_path=fp,
                                extra={"var_name": key, "value": str(value)},
                            )
                        )
        return nodes, edges

    def _handle_variable(
        self, fp: str, item: dict
    ) -> tuple[list[NodeInfo], list[EdgeInfo]]:
        """variable "vpc_id" { type = string ... }"""
        nodes: list[NodeInfo] = []
        for var_name, body in item.items():
            body = body if isinstance(body, dict) else {}
            qn = make_qualified(fp, "Variable", var_name)
            nodes.append(
                NodeInfo(
                    kind="Variable",
                    name=var_name,
                    qualified_name=qn,
                    file_path=fp,
                    line_start=0,
                    line_end=0,
                    language="hcl",
                    extra={
                        "type": str(body.get("type", "")),
                        "default": str(body.get("default", "")),
                        "description": str(body.get("description", "")),
                    },
                )
            )
        return nodes, []

    def _handle_output(
        self, fp: str, item: dict
    ) -> tuple[list[NodeInfo], list[EdgeInfo]]:
        """output "instance_ip" { value = ... }"""
        nodes: list[NodeInfo] = []
        for output_name, body in item.items():
            body = body if isinstance(body, dict) else {}
            qn = make_qualified(fp, "Output", output_name)
            nodes.append(
                NodeInfo(
                    kind="Output",
                    name=output_name,
                    qualified_name=qn,
                    file_path=fp,
                    line_start=0,
                    line_end=0,
                    language="hcl",
                    extra={
                        "description": str(body.get("description", "")),
                        "value": str(body.get("value", "")),
                    },
                )
            )
        return nodes, []

    def _handle_provider(
        self, fp: str, item: dict
    ) -> tuple[list[NodeInfo], list[EdgeInfo]]:
        """provider "aws" { region = "us-east-1" }"""
        nodes: list[NodeInfo] = []
        for provider_name, body in item.items():
            body = body if isinstance(body, dict) else {}
            qn = make_qualified(fp, "Provider", provider_name)
            nodes.append(
                NodeInfo(
                    kind="Provider",
                    name=provider_name,
                    qualified_name=qn,
                    file_path=fp,
                    line_start=0,
                    line_end=0,
                    language="hcl",
                    extra={
                        "region": str(body.get("region", "")),
                        "alias": str(body.get("alias", "")),
                    },
                )
            )
        return nodes, []

    def _handle_locals(
        self, fp: str, item: dict
    ) -> tuple[list[NodeInfo], list[EdgeInfo]]:
        """locals { name = value ... }  — one node per local value."""
        nodes: list[NodeInfo] = []
        for local_name, value in item.items():
            qn = make_qualified(fp, "Local", local_name)
            nodes.append(
                NodeInfo(
                    kind="Local",
                    name=local_name,
                    qualified_name=qn,
                    file_path=fp,
                    line_start=0,
                    line_end=0,
                    language="hcl",
                    extra={"value": str(value)},
                )
            )
        return nodes, []

    def _handle_terraform(
        self, fp: str, item: dict
    ) -> tuple[list[NodeInfo], list[EdgeInfo]]:
        """terraform { required_version = "..." required_providers { ... } }"""
        nodes: list[NodeInfo] = []
        qn = make_qualified(fp, "TerraformConfig", "terraform")
        nodes.append(
            NodeInfo(
                kind="TerraformConfig",
                name="terraform",
                qualified_name=qn,
                file_path=fp,
                line_start=0,
                line_end=0,
                language="hcl",
                extra=item if isinstance(item, dict) else {},
            )
        )
        return nodes, []

    # ------------------------------------------------------------------
    # Reference extraction
    # ------------------------------------------------------------------

    def _extract_references(
        self,
        fp: str,
        source_qn: str,
        parsed: dict,
        nodes: list[NodeInfo],
    ) -> list[EdgeInfo]:
        """Walk all string values in the parsed HCL dict, extract REFERENCES edges.

        Builds a lookup from (kind, name) -> qualified_name from the already-
        produced nodes so we can resolve references to concrete targets.
        """
        # Build lookup: (kind, name) -> qn
        node_by_kind_name: dict[tuple[str, str], str] = {}
        for node in nodes:
            if node.kind != "File":
                node_by_kind_name[(node.kind, node.name)] = node.qualified_name

        edges: list[EdgeInfo] = []
        seen: set[tuple[str, str]] = set()  # (source_qn, target_qn)

        def add_ref(src: str, tgt: str) -> None:
            key = (src, tgt)
            if key not in seen:
                seen.add(key)
                edges.append(
                    EdgeInfo(
                        kind="REFERENCES",
                        source_qualified=src,
                        target_qualified=tgt,
                        file_path=fp,
                    )
                )

        def current_block_qn(block_type: str, block_path: list[str]) -> str:
            """Resolve the qualified name for the enclosing block."""
            # block_path is built as we recurse; for resources it's [type, name]
            if block_type == "resource" and len(block_path) >= 2:
                return node_by_kind_name.get(
                    ("Resource", f"{block_path[0]}.{block_path[1]}"), source_qn
                )
            if block_type == "data" and len(block_path) >= 2:
                return node_by_kind_name.get(
                    ("DataSource", f"{block_path[0]}.{block_path[1]}"), source_qn
                )
            if block_type == "module" and block_path:
                return node_by_kind_name.get(("Module", block_path[0]), source_qn)
            if block_type == "output" and block_path:
                return node_by_kind_name.get(("Output", block_path[0]), source_qn)
            if block_type == "locals":
                return source_qn  # file level
            return source_qn

        def scan_value(value: Any, src: str) -> None:
            """Recursively scan a value for reference patterns."""
            if isinstance(value, str):
                _scan_string(value, src)
            elif isinstance(value, dict):
                for v in value.values():
                    scan_value(v, src)
            elif isinstance(value, list):
                for v in value:
                    scan_value(v, src)

        def _scan_string(text: str, src: str) -> None:
            # var.NAME
            for m in _RE_VAR.finditer(text):
                var_name = m.group(1)
                tgt = node_by_kind_name.get(("Variable", var_name))
                if tgt:
                    add_ref(src, tgt)

            # local.NAME
            for m in _RE_LOCAL.finditer(text):
                local_name = m.group(1)
                tgt = node_by_kind_name.get(("Local", local_name))
                if tgt:
                    add_ref(src, tgt)

            # module.NAME (ignore sub-attribute)
            for m in _RE_MODULE.finditer(text):
                mod_name = m.group(1)
                tgt = node_by_kind_name.get(("Module", mod_name))
                if tgt:
                    add_ref(src, tgt)

            # data.TYPE.NAME
            for m in _RE_DATA.finditer(text):
                data_name = f"{m.group(1)}.{m.group(2)}"
                tgt = node_by_kind_name.get(("DataSource", data_name))
                if tgt:
                    add_ref(src, tgt)

            # TYPE.NAME.ATTR — resource references
            for m in _RE_RESOURCE_REF.finditer(text):
                res_name = f"{m.group(1)}.{m.group(2)}"
                tgt = node_by_kind_name.get(("Resource", res_name))
                if tgt:
                    add_ref(src, tgt)

        # Walk block types, carrying the right source qn per block
        for block_type, items in parsed.items():
            for item in items:
                if not isinstance(item, dict):
                    continue

                if block_type == "resource":
                    for res_type, instances in item.items():
                        if not isinstance(instances, dict):
                            continue
                        for res_name, body in instances.items():
                            src = node_by_kind_name.get(
                                ("Resource", f"{res_type}.{res_name}"), source_qn
                            )
                            scan_value(body, src)

                elif block_type == "data":
                    for data_type, instances in item.items():
                        if not isinstance(instances, dict):
                            continue
                        for data_name, body in instances.items():
                            src = node_by_kind_name.get(
                                ("DataSource", f"{data_type}.{data_name}"), source_qn
                            )
                            scan_value(body, src)

                elif block_type == "module":
                    for mod_name, body in item.items():
                        src = node_by_kind_name.get(("Module", mod_name), source_qn)
                        scan_value(body, src)

                elif block_type == "output":
                    for out_name, body in item.items():
                        src = node_by_kind_name.get(("Output", out_name), source_qn)
                        scan_value(body, src)

                elif block_type == "locals":
                    scan_value(item, source_qn)

                elif block_type in ("variable", "provider", "terraform"):
                    scan_value(item, source_qn)

        return edges


# Module-level singleton for registry
hcl_parser = HCLParser()
