"""Dockerfile parser using tree-sitter to extract build stages, instructions, and dependencies."""

from __future__ import annotations

from pathlib import Path

from tree_sitter_language_pack import get_parser as ts_get_parser

from code_rosetta.models import EdgeInfo, NodeInfo, make_qualified

_TS_PARSER = ts_get_parser("dockerfile")


def _node_text(node) -> str:
    """Return the UTF-8 text of a tree-sitter node."""
    return node.text.decode("utf-8", errors="replace") if node.text else ""


def _find_children(node, *types: str):
    """Return direct children matching any of the given types."""
    return [c for c in node.children if c.type in types]


def _find_first(node, node_type: str):
    """Return first direct child of given type, or None."""
    for c in node.children:
        if c.type == node_type:
            return c
    return None


class DockerfileParser:
    language = "dockerfile"
    extensions = [".dockerfile"]
    filenames = ["Dockerfile"]

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

        stages: dict[str, str] = {}  # alias -> base image
        current_stage = None

        for child in root.children:
            line = child.start_point[0] + 1

            if child.type == "from_instruction":
                image_spec = _find_first(child, "image_spec")
                image_name_node = _find_first(image_spec, "image_name") if image_spec else None
                image_name = _node_text(image_name_node) if image_name_node else "unknown"

                image_tag_node = _find_first(image_spec, "image_tag") if image_spec else None
                tag = _node_text(image_tag_node).lstrip(":") if image_tag_node else ""

                alias_node = _find_first(child, "image_alias")
                alias = _node_text(alias_node) if alias_node else None

                full_image = f"{image_name}:{tag}" if tag else image_name
                stage_name = alias or full_image

                stage_qname = make_qualified(fp, "Stage", stage_name)
                nodes.append(NodeInfo(
                    kind="Stage",
                    name=stage_name,
                    qualified_name=stage_qname,
                    file_path=fp,
                    line_start=line,
                    line_end=line,
                    language=self.language,
                    extra={"image": full_image, "alias": alias or ""},
                ))
                edges.append(EdgeInfo(
                    kind="CONTAINS",
                    source_qualified=file_qname,
                    target_qualified=stage_qname,
                    file_path=fp,
                    line=line,
                ))

                if alias:
                    stages[alias] = full_image
                current_stage = stage_qname

                # If FROM references another stage, add edge
                if image_name in stages:
                    edges.append(EdgeInfo(
                        kind="DEPENDS_ON",
                        source_qualified=stage_qname,
                        target_qualified=make_qualified(fp, "Stage", image_name),
                        file_path=fp,
                        line=line,
                    ))

            elif child.type == "copy_instruction" and current_stage:
                # Check for --from=stage references
                for param in _find_children(child, "param"):
                    param_text = _node_text(param)
                    if param_text.startswith("--from="):
                        ref_stage = param_text.split("=", 1)[1]
                        if ref_stage in stages:
                            edges.append(EdgeInfo(
                                kind="DEPENDS_ON",
                                source_qualified=current_stage,
                                target_qualified=make_qualified(fp, "Stage", ref_stage),
                                file_path=fp,
                                line=line,
                            ))

                # Track COPY sources as references to repo files
                paths = _find_children(child, "path")
                if len(paths) >= 2:
                    # Last path is destination, rest are sources
                    for src_path in paths[:-1]:
                        src_text = _node_text(src_path)
                        if src_text and src_text not in (".", "./", "/"):
                            edges.append(EdgeInfo(
                                kind="REFERENCES",
                                source_qualified=current_stage,
                                target_qualified=f"file::{src_text}",
                                file_path=fp,
                                line=line,
                                extra={"ref_type": "COPY"},
                            ))

            elif child.type == "expose_instruction" and current_stage:
                port_node = _find_first(child, "expose_port")
                if port_node:
                    port = _node_text(port_node)
                    nodes.append(NodeInfo(
                        kind="Port",
                        name=f"EXPOSE {port}",
                        qualified_name=make_qualified(fp, "Port", port),
                        file_path=fp,
                        line_start=line,
                        line_end=line,
                        language=self.language,
                        extra={"port": port},
                    ))
                    edges.append(EdgeInfo(
                        kind="CONTAINS",
                        source_qualified=current_stage,
                        target_qualified=make_qualified(fp, "Port", port),
                        file_path=fp,
                        line=line,
                    ))

            elif child.type == "entrypoint_instruction" and current_stage:
                cmd = _node_text(child).replace("ENTRYPOINT", "").strip()
                nodes.append(NodeInfo(
                    kind="Entrypoint",
                    name=f"ENTRYPOINT {cmd}",
                    qualified_name=make_qualified(fp, "Entrypoint", "entrypoint"),
                    file_path=fp,
                    line_start=line,
                    line_end=line,
                    language=self.language,
                    extra={"command": cmd},
                ))
                edges.append(EdgeInfo(
                    kind="CONTAINS",
                    source_qualified=current_stage,
                    target_qualified=make_qualified(fp, "Entrypoint", "entrypoint"),
                    file_path=fp,
                    line=line,
                ))

        return nodes, edges


dockerfile_parser = DockerfileParser()
