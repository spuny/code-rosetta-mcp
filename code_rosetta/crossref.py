"""Cross-language reference detection.

Post-parsing pass that detects relationships between files of different languages:
- Python reading YAML config files
- Python rendering Jinja2 templates
- Terraform reading YAML files via yamldecode/templatefile
"""

from __future__ import annotations

import re
from pathlib import Path

from .models import EdgeInfo, NodeInfo


class CrossReferenceDetector:
    """Detects cross-language edges after all files are parsed."""

    def detect(
        self, all_nodes: list[NodeInfo], all_edges: list[EdgeInfo]
    ) -> list[EdgeInfo]:
        new_edges: list[EdgeInfo] = []

        # Build lookup maps
        file_nodes = {n.qualified_name: n for n in all_nodes if n.kind == "File"}
        nodes_by_name = {}
        for n in all_nodes:
            nodes_by_name.setdefault(n.name, []).append(n)

        # Collect all file paths for matching
        known_files = {str(Path(n.file_path).name): n for n in all_nodes if n.kind == "File"}
        known_file_paths = {n.file_path: n for n in all_nodes if n.kind == "File"}

        new_edges.extend(self._detect_python_yaml(all_nodes, all_edges, known_files, known_file_paths))
        new_edges.extend(self._detect_python_jinja(all_nodes, all_edges, known_files, known_file_paths))
        new_edges.extend(self._detect_terraform_yaml(all_nodes, all_edges, known_files, known_file_paths))
        new_edges.extend(self._detect_terraform_remote_state(all_nodes, all_edges))

        return new_edges

    def _detect_python_yaml(
        self,
        all_nodes: list[NodeInfo],
        all_edges: list[EdgeInfo],
        known_files: dict[str, NodeInfo],
        known_file_paths: dict[str, NodeInfo],
    ) -> list[EdgeInfo]:
        """Detect Python code that reads YAML files.

        Looks for patterns like:
        - open("config.yaml"), open("config.yml")
        - yaml.load(...), yaml.safe_load(...)
        - Path("something.yaml")
        - References to .yaml/.yml files in string literals
        """
        edges: list[EdgeInfo] = []
        seen = set()

        # Look at CALLS edges from Python files that reference yaml-related functions
        yaml_call_names = {"load", "safe_load", "load_all", "safe_load_all", "dump", "safe_dump"}

        for edge in all_edges:
            if edge.kind != "CALLS":
                continue
            # Check if the call target looks like yaml loading
            target_name = edge.target_qualified.split("::")[-1] if "::" in edge.target_qualified else edge.target_qualified
            if target_name in yaml_call_names:
                # This Python function uses YAML — check if we can find the config file
                source_node = self._find_node(edge.source_qualified, all_nodes)
                if source_node and source_node.language == "python":
                    # Look for string literals with .yaml/.yml in the same file's edges
                    for other_edge in all_edges:
                        if other_edge.file_path == edge.file_path and other_edge.kind == "CALLS":
                            ref = other_edge.extra.get("raw_target", "")
                            if ref and (ref.endswith(".yaml") or ref.endswith(".yml")):
                                yaml_file = self._resolve_file_ref(
                                    ref, edge.file_path, known_files, known_file_paths
                                )
                                if yaml_file:
                                    key = (edge.source_qualified, yaml_file.qualified_name)
                                    if key not in seen:
                                        seen.add(key)
                                        edges.append(EdgeInfo(
                                            kind="READS_CONFIG",
                                            source_qualified=edge.source_qualified,
                                            target_qualified=yaml_file.qualified_name,
                                            file_path=edge.file_path,
                                            line=edge.line,
                                            extra={"detected_by": "crossref", "config_file": ref},
                                        ))

        # Also scan IMPORTS edges for yaml module imports to flag files that use YAML
        for edge in all_edges:
            if edge.kind == "IMPORTS" and "yaml" in edge.target_qualified.lower():
                source_file = edge.file_path
                # Mark this file as yaml-aware for downstream queries
                for fn, fnode in known_file_paths.items():
                    if fn == source_file:
                        break

        return edges

    def _detect_python_jinja(
        self,
        all_nodes: list[NodeInfo],
        all_edges: list[EdgeInfo],
        known_files: dict[str, NodeInfo],
        known_file_paths: dict[str, NodeInfo],
    ) -> list[EdgeInfo]:
        """Detect Python code that renders Jinja2 templates.

        Looks for patterns like:
        - get_template("name.j2")
        - env.get_template(...)
        - render_template(...)
        - Template imports from jinja2
        """
        edges: list[EdgeInfo] = []
        seen = set()

        jinja_call_names = {"get_template", "render_template", "render_template_string"}

        for edge in all_edges:
            if edge.kind != "CALLS":
                continue
            target_name = edge.target_qualified.split("::")[-1] if "::" in edge.target_qualified else edge.target_qualified
            if target_name in jinja_call_names:
                # Check extra for the template name if available
                template_name = edge.extra.get("raw_target", "")
                if template_name:
                    jinja_file = self._resolve_file_ref(
                        template_name, edge.file_path, known_files, known_file_paths
                    )
                    if jinja_file:
                        key = (edge.source_qualified, jinja_file.qualified_name)
                        if key not in seen:
                            seen.add(key)
                            edges.append(EdgeInfo(
                                kind="RENDERS",
                                source_qualified=edge.source_qualified,
                                target_qualified=jinja_file.qualified_name,
                                file_path=edge.file_path,
                                line=edge.line,
                                extra={"detected_by": "crossref", "template": template_name},
                            ))

        return edges

    def _detect_terraform_yaml(
        self,
        all_nodes: list[NodeInfo],
        all_edges: list[EdgeInfo],
        known_files: dict[str, NodeInfo],
        known_file_paths: dict[str, NodeInfo],
    ) -> list[EdgeInfo]:
        """Detect Terraform code that reads YAML/template files.

        Looks for:
        - file("*.yaml") / file("*.yml") in extra data
        - yamldecode() references
        - templatefile() references
        """
        edges: list[EdgeInfo] = []
        seen = set()

        # Check HCL nodes for file references in their extra data
        for node in all_nodes:
            if node.language != "hcl":
                continue
            extra_str = str(node.extra)

            # Look for file() / yamldecode() / templatefile() patterns
            file_refs = re.findall(
                r'(?:file|yamldecode|templatefile)\s*\(\s*["\']([^"\']+)["\']',
                extra_str,
            )
            for ref in file_refs:
                if ref.endswith((".yaml", ".yml", ".json", ".j2", ".tpl")):
                    target_file = self._resolve_file_ref(
                        ref, node.file_path, known_files, known_file_paths
                    )
                    if target_file:
                        key = (node.qualified_name, target_file.qualified_name)
                        if key not in seen:
                            seen.add(key)
                            edge_kind = "RENDERS" if ref.endswith((".j2", ".tpl")) else "READS_CONFIG"
                            edges.append(EdgeInfo(
                                kind=edge_kind,
                                source_qualified=node.qualified_name,
                                target_qualified=target_file.qualified_name,
                                file_path=node.file_path,
                                extra={"detected_by": "crossref", "reference": ref},
                            ))

        return edges

    def _detect_terraform_remote_state(
        self,
        all_nodes: list[NodeInfo],
        all_edges: list[EdgeInfo],
    ) -> list[EdgeInfo]:
        """Connect terraform_remote_state data sources to outputs in other repos.

        When repo A has:
            data "terraform_remote_state" "legacy" { ... }
            module "users" { policies = data.terraform_remote_state.legacy.outputs.X }

        And repo B has:
            output "X" { value = ... }

        This creates a REMOTE_STATE edge from the data source to the matching output,
        enabling cross-repo blast radius detection.
        """
        edges: list[EdgeInfo] = []
        seen = set()

        # Collect all terraform_remote_state DataSource nodes
        remote_states = [
            n for n in all_nodes
            if n.kind == "DataSource" and n.name.startswith("terraform_remote_state.")
        ]

        if not remote_states:
            return edges

        # Collect all Output nodes, indexed by name
        # Multiple repos may have outputs with the same name — collect all
        outputs_by_name: dict[str, list[NodeInfo]] = {}
        for n in all_nodes:
            if n.kind == "Output":
                outputs_by_name.setdefault(n.name, []).append(n)

        # For each remote state, find REFERENCES edges that use it,
        # extract the output names being accessed, and link to matching Output nodes
        for rs in remote_states:
            # Find all edges that reference this remote state
            rs_refs = [
                e for e in all_edges
                if e.kind == "REFERENCES" and e.target_qualified == rs.qualified_name
            ]

            # Also scan all string values in nodes from the same file
            # for patterns like: data.terraform_remote_state.NAME.outputs.OUTPUT_NAME
            rs_label = rs.name.split(".", 1)[1]  # e.g. "legacy" from "terraform_remote_state.legacy"
            output_ref_pattern = re.compile(
                rf"data\.terraform_remote_state\.{re.escape(rs_label)}\.outputs\.([A-Za-z0-9_]+)"
            )

            # Scan all nodes in the same file for output references
            referenced_outputs: set[str] = set()
            same_file_nodes = [n for n in all_nodes if n.file_path == rs.file_path]
            for node in same_file_nodes:
                extra_str = str(node.extra)
                for m in output_ref_pattern.finditer(extra_str):
                    referenced_outputs.add(m.group(1))

            # Also scan edges' extras for the pattern
            same_file_edges = [e for e in all_edges if e.file_path == rs.file_path]
            for edge in same_file_edges:
                extra_str = str(edge.extra)
                for m in output_ref_pattern.finditer(extra_str):
                    referenced_outputs.add(m.group(1))

            # Create edges from remote_state to matching outputs in OTHER repos
            rs_repo = str(Path(rs.file_path).parent)
            for output_name in referenced_outputs:
                for output_node in outputs_by_name.get(output_name, []):
                    # Only connect to outputs in a different repo
                    output_repo = str(Path(output_node.file_path).parent)
                    if output_repo != rs_repo:
                        key = (rs.qualified_name, output_node.qualified_name)
                        if key not in seen:
                            seen.add(key)
                            edges.append(EdgeInfo(
                                kind="REMOTE_STATE",
                                source_qualified=rs.qualified_name,
                                target_qualified=output_node.qualified_name,
                                file_path=rs.file_path,
                                extra={
                                    "detected_by": "crossref",
                                    "remote_state": rs.name,
                                    "output": output_name,
                                },
                            ))

            # If we couldn't find specific output references, create a general link
            # from the remote_state to ALL outputs in other repos (weaker signal)
            if not referenced_outputs and rs_refs:
                for ref_edge in rs_refs:
                    referrer = self._find_node(ref_edge.source_qualified, all_nodes)
                    if referrer:
                        extra_str = str(referrer.extra)
                        for m in output_ref_pattern.finditer(extra_str):
                            output_name = m.group(1)
                            for output_node in outputs_by_name.get(output_name, []):
                                output_repo = str(Path(output_node.file_path).parent)
                                if output_repo != rs_repo:
                                    key = (rs.qualified_name, output_node.qualified_name)
                                    if key not in seen:
                                        seen.add(key)
                                        edges.append(EdgeInfo(
                                            kind="REMOTE_STATE",
                                            source_qualified=rs.qualified_name,
                                            target_qualified=output_node.qualified_name,
                                            file_path=rs.file_path,
                                            extra={
                                                "detected_by": "crossref",
                                                "remote_state": rs.name,
                                                "output": output_name,
                                            },
                                        ))

        return edges

    def _find_node(self, qualified_name: str, all_nodes: list[NodeInfo]) -> NodeInfo | None:
        for n in all_nodes:
            if n.qualified_name == qualified_name:
                return n
        return None

    def _resolve_file_ref(
        self,
        ref: str,
        source_file: str,
        known_files: dict[str, NodeInfo],
        known_file_paths: dict[str, NodeInfo],
    ) -> NodeInfo | None:
        """Try to resolve a file reference to a known file node."""
        # Try exact filename match
        basename = Path(ref).name
        if basename in known_files:
            return known_files[basename]

        # Try relative path from source file
        source_dir = Path(source_file).parent
        resolved = source_dir / ref
        resolved_str = str(resolved)
        if resolved_str in known_file_paths:
            return known_file_paths[resolved_str]

        # Try normalized path
        try:
            resolved_norm = str(resolved.resolve())
            if resolved_norm in known_file_paths:
                return known_file_paths[resolved_norm]
        except (OSError, ValueError):
            pass

        return None


# Module-level singleton
cross_reference_detector = CrossReferenceDetector()
