"""YAML parser — extracts nodes and edges from YAML files.

Supports:
- Plain YAML (sections as top-level keys)
- Multi-document YAML (--- separator)
- Kubernetes manifests (apiVersion + kind → K8sResource nodes)
- Helm values files (values.yaml / Chart.yaml heuristics)
- YAML anchors (&name) and aliases (*name) → REFERENCES edges
"""

from __future__ import annotations

import logging
from pathlib import Path
from typing import Any

from ruamel.yaml import YAML
from ruamel.yaml.comments import CommentedMap, CommentedSeq
from ruamel.yaml.constructor import DuplicateKeyError

from code_rosetta.models import EdgeInfo, NodeInfo, make_qualified

log = logging.getLogger(__name__)

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

_HELM_FILE_NAMES = {"values.yaml", "values.yml", "chart.yaml", "chart.yml"}
_HELM_TOP_KEYS = {
    "replicaCount", "image", "service", "ingress", "resources",
    "nodeSelector", "tolerations", "affinity", "serviceAccount",
    "podAnnotations", "podSecurityContext", "securityContext",
    "livenessProbe", "readinessProbe", "autoscaling",
}
_HELM_CHART_KEYS = {"apiVersion", "name", "version", "description", "type", "appVersion"}


def _node_line(obj: Any) -> int:
    """Return the 1-based start line of a ruamel.yaml node, or 0 if unknown."""
    lc = getattr(obj, "lc", None)
    if lc is None:
        return 0
    line = getattr(lc, "line", None)
    if line is None:
        return 0
    return int(line) + 1  # ruamel uses 0-based lines


def _key_line(mapping: CommentedMap, key: str) -> int:
    """Return the 1-based line where *key* appears inside a CommentedMap."""
    lc = getattr(mapping, "lc", None)
    if lc is None:
        return _node_line(mapping)
    try:
        line, _col = lc.key(key)
        return int(line) + 1
    except (KeyError, TypeError):
        return _node_line(mapping)


def _collect_anchors(obj: Any, anchors: dict[str, tuple[str, int]]) -> None:
    """Walk *obj* recursively and populate *anchors* with {anchor_name: (tag, line)}."""
    anchor = getattr(getattr(obj, "anchor", None), "value", None)
    if anchor:
        anchors[anchor] = (_type_tag(obj), _node_line(obj))

    if isinstance(obj, CommentedMap):
        for v in obj.values():
            _collect_anchors(v, anchors)
    elif isinstance(obj, CommentedSeq):
        for item in obj:
            _collect_anchors(item, anchors)


def _collect_aliases(
    obj: Any,
    path: str,
    parent_qname: str,
    file_path: str,
    anchors: dict[str, str],  # anchor_name -> anchor_qname
    edges: list[EdgeInfo],
    line: int = 0,
) -> None:
    """Walk *obj* and emit REFERENCES edges wherever an alias is found."""
    # ruamel represents aliases as objects whose .anchor.always_dump is False
    # and whose .anchor.value matches the original anchor name.
    # The alias object itself *is* the same Python object as the anchor target
    # (ruamel shares references), so we detect aliases by checking if the
    # anchor.always_dump attribute is False (alias) vs True/None (anchor definition).
    anc = getattr(obj, "anchor", None)
    if anc is not None:
        anc_value = getattr(anc, "value", None)
        always_dump = getattr(anc, "always_dump", None)
        # always_dump=True means this is the anchor definition.
        # always_dump=False (or None with a value set) may be an alias.
        # ruamel.yaml sets always_dump=True on anchors, False on aliases.
        if anc_value and always_dump is False and anc_value in anchors:
            edges.append(EdgeInfo(
                kind="REFERENCES",
                source_qualified=parent_qname,
                target_qualified=anchors[anc_value],
                file_path=file_path,
                line=line or _node_line(obj),
                extra={"via": "alias", "anchor": anc_value},
            ))
            return  # don't recurse into the alias (it shares structure with anchor)

    if isinstance(obj, CommentedMap):
        for k, v in obj.items():
            child_path = f"{path}.{k}" if path else str(k)
            child_line = _key_line(obj, k)
            _collect_aliases(v, child_path, parent_qname, file_path, anchors, edges, child_line)
    elif isinstance(obj, CommentedSeq):
        for i, item in enumerate(obj):
            child_path = f"{path}[{i}]"
            _collect_aliases(item, child_path, parent_qname, file_path, anchors, edges, _node_line(item))


def _type_tag(obj: Any) -> str:
    if isinstance(obj, CommentedMap):
        return "mapping"
    if isinstance(obj, CommentedSeq):
        return "sequence"
    return type(obj).__name__


def _is_helm_values(file_path: Path, doc: Any) -> bool:
    """Heuristic: is this file a Helm values file?"""
    if file_path.name.lower() in _HELM_FILE_NAMES:
        return True
    if isinstance(doc, CommentedMap):
        keys = set(doc.keys())
        if len(keys & _HELM_TOP_KEYS) >= 3:
            return True
    return False


def _is_helm_chart(file_path: Path, doc: Any) -> bool:
    """Heuristic: is this file a Helm Chart.yaml?"""
    if file_path.name.lower() in {"chart.yaml", "chart.yml"}:
        return True
    if isinstance(doc, CommentedMap):
        keys = set(doc.keys())
        # Chart.yaml has apiVersion + name + version but NOT the k8s-style 'kind' key
        if "apiVersion" in keys and "name" in keys and "version" in keys and "kind" not in keys:
            return True
    return False


def _is_k8s(doc: Any) -> bool:
    """Return True if the document looks like a Kubernetes manifest."""
    if not isinstance(doc, CommentedMap):
        return False
    return "apiVersion" in doc and "kind" in doc


def _k8s_resource_name(doc: CommentedMap) -> str:
    """Build 'Kind/name' label for a Kubernetes resource."""
    kind = str(doc.get("kind", "Unknown"))
    metadata = doc.get("metadata")
    if isinstance(metadata, CommentedMap):
        name = metadata.get("name", "")
    else:
        name = ""
    if name:
        return f"{kind}/{name}"
    return kind


def _k8s_namespace(doc: CommentedMap) -> str:
    metadata = doc.get("metadata")
    if isinstance(metadata, CommentedMap):
        return str(metadata.get("namespace", ""))
    return ""


def _k8s_referenced_names(doc: CommentedMap) -> list[tuple[str, str]]:
    """Return a list of (kind_hint, name) pairs that this k8s doc references by name.

    Currently handles common patterns:
    - spec.template.spec.volumes[*].configMap.name  → ConfigMap
    - spec.template.spec.volumes[*].secret.secretName → Secret
    - spec.template.spec.containers[*].envFrom[*].configMapRef.name → ConfigMap
    - spec.template.spec.containers[*].envFrom[*].secretRef.name → Secret
    - spec.selector / spec.serviceName → Service (for StatefulSet)
    """
    refs: list[tuple[str, str]] = []

    spec = doc.get("spec")
    if not isinstance(spec, CommentedMap):
        return refs

    # Pod template spec (Deployment, DaemonSet, StatefulSet, Job, …)
    template = spec.get("template")
    pod_spec = None
    if isinstance(template, CommentedMap):
        pod_spec = template.get("spec")

    if isinstance(pod_spec, CommentedMap):
        volumes = pod_spec.get("volumes")
        if isinstance(volumes, CommentedSeq):
            for vol in volumes:
                if not isinstance(vol, CommentedMap):
                    continue
                cm = vol.get("configMap")
                if isinstance(cm, CommentedMap) and "name" in cm:
                    refs.append(("ConfigMap", str(cm["name"])))
                sec = vol.get("secret")
                if isinstance(sec, CommentedMap) and "secretName" in sec:
                    refs.append(("Secret", str(sec["secretName"])))

        containers = pod_spec.get("containers") or CommentedSeq()
        init_containers = pod_spec.get("initContainers") or CommentedSeq()
        for container in list(containers) + list(init_containers):
            if not isinstance(container, CommentedMap):
                continue
            env_from = container.get("envFrom")
            if isinstance(env_from, CommentedSeq):
                for ef in env_from:
                    if not isinstance(ef, CommentedMap):
                        continue
                    cmr = ef.get("configMapRef")
                    if isinstance(cmr, CommentedMap) and "name" in cmr:
                        refs.append(("ConfigMap", str(cmr["name"])))
                    secr = ef.get("secretRef")
                    if isinstance(secr, CommentedMap) and "name" in secr:
                        refs.append(("Secret", str(secr["name"])))

    # StatefulSet serviceName
    service_name = spec.get("serviceName")
    if service_name:
        refs.append(("Service", str(service_name)))

    return refs


# ---------------------------------------------------------------------------
# Main parser class
# ---------------------------------------------------------------------------

class YAMLParser:
    """Parses YAML files and returns NodeInfo + EdgeInfo lists."""

    language = "yaml"
    extensions = [".yaml", ".yml"]

    # ------------------------------------------------------------------
    # Public API (LanguageParser protocol)
    # ------------------------------------------------------------------

    def parse(self, file_path: Path, source: bytes) -> tuple[list[NodeInfo], list[EdgeInfo]]:
        nodes: list[NodeInfo] = []
        edges: list[EdgeInfo] = []

        fp = str(file_path)
        source_text = source.decode("utf-8", errors="replace")
        total_lines = source_text.count("\n") + 1

        # File node
        file_qname = make_qualified(fp, "File", file_path.name)
        nodes.append(NodeInfo(
            kind="File",
            name=file_path.name,
            qualified_name=file_qname,
            file_path=fp,
            line_start=1,
            line_end=total_lines,
            language="yaml",
        ))

        # Parse YAML documents
        documents = self._load_documents(source_text, fp)
        if not documents:
            return nodes, edges

        is_multi_doc = len(documents) > 1

        # Determine file-level Helm flag from first doc
        is_helm = _is_helm_values(file_path, documents[0]) or _is_helm_chart(file_path, documents[0])

        for doc_idx, doc in enumerate(documents):
            doc_nodes, doc_edges = self._parse_document(
                doc=doc,
                doc_idx=doc_idx,
                is_multi_doc=is_multi_doc,
                is_helm=is_helm,
                file_path=fp,
                file_qname=file_qname,
                file_path_obj=file_path,
                total_lines=total_lines,
            )
            nodes.extend(doc_nodes)
            edges.extend(doc_edges)

        return nodes, edges

    # ------------------------------------------------------------------
    # Document loading
    # ------------------------------------------------------------------

    def _load_documents(self, source_text: str, file_path: str) -> list[Any]:
        """Load all documents from the YAML source, tolerating errors."""
        yaml = YAML()
        yaml.preserve_quotes = True
        # Allow duplicate keys (warn, don't crash)
        yaml.allow_duplicate_keys = True

        try:
            docs = list(yaml.load_all(source_text))
        except DuplicateKeyError as exc:
            log.warning("Duplicate key in %s: %s", file_path, exc)
            try:
                docs = list(yaml.load_all(source_text))
            except Exception:
                docs = []
        except Exception as exc:
            log.warning("Failed to parse YAML %s: %s", file_path, exc)
            return []

        # Filter out None documents (empty --- separators)
        return [d for d in docs if d is not None]

    # ------------------------------------------------------------------
    # Per-document parsing
    # ------------------------------------------------------------------

    def _parse_document(
        self,
        doc: Any,
        doc_idx: int,
        is_multi_doc: bool,
        is_helm: bool,
        file_path: str,
        file_qname: str,
        file_path_obj: Path,
        total_lines: int,
    ) -> tuple[list[NodeInfo], list[EdgeInfo]]:
        nodes: list[NodeInfo] = []
        edges: list[EdgeInfo] = []

        doc_line = _node_line(doc) or 1

        # In single-document files, we work directly under the file node.
        # In multi-document files, each document gets its own Document node.
        if is_multi_doc:
            doc_name = f"doc{doc_idx}"
            doc_qname = make_qualified(file_path, "Document", doc_name)
            doc_node = NodeInfo(
                kind="Document",
                name=doc_name,
                qualified_name=doc_qname,
                file_path=file_path,
                line_start=doc_line,
                line_end=total_lines,  # refined below if possible
                language="yaml",
                parent_name=file_path_obj.name,
                extra={"doc_index": doc_idx},
            )
            nodes.append(doc_node)
            edges.append(EdgeInfo(
                kind="CONTAINS",
                source_qualified=file_qname,
                target_qualified=doc_qname,
                file_path=file_path,
                line=doc_line,
            ))
            parent_qname = doc_qname
            parent_name = doc_name
        else:
            parent_qname = file_qname
            parent_name = file_path_obj.name

        if not isinstance(doc, CommentedMap):
            # Scalar or sequence document — no sub-nodes to extract
            return nodes, edges

        # ------------------------------------------------------------------
        # Collect anchors for this document
        # ------------------------------------------------------------------
        raw_anchors: dict[str, tuple[str, int]] = {}
        _collect_anchors(doc, raw_anchors)

        # Build anchor_name -> anchor_qname mapping (needed for alias edges)
        anchor_qnames: dict[str, str] = {}
        for anchor_name, (tag, anchor_line) in raw_anchors.items():
            anc_qname = make_qualified(file_path, "Anchor", f"&{anchor_name}")
            anchor_qnames[anchor_name] = anc_qname
            nodes.append(NodeInfo(
                kind="Anchor",
                name=f"&{anchor_name}",
                qualified_name=anc_qname,
                file_path=file_path,
                line_start=anchor_line or doc_line,
                line_end=anchor_line or doc_line,
                language="yaml",
                parent_name=parent_name,
                extra={"anchor_name": anchor_name, "value_type": tag},
            ))
            edges.append(EdgeInfo(
                kind="CONTAINS",
                source_qualified=parent_qname,
                target_qualified=anc_qname,
                file_path=file_path,
                line=anchor_line or doc_line,
            ))

        # ------------------------------------------------------------------
        # Kubernetes manifest
        # ------------------------------------------------------------------
        if _is_k8s(doc):
            k8s_nodes, k8s_edges = self._parse_k8s(
                doc=doc,
                doc_line=doc_line,
                parent_qname=parent_qname,
                parent_name=parent_name,
                file_path=file_path,
            )
            nodes.extend(k8s_nodes)
            edges.extend(k8s_edges)
            # Also collect alias edges within this document
            _collect_aliases(doc, "", parent_qname, file_path, anchor_qnames, edges)
            return nodes, edges

        # ------------------------------------------------------------------
        # Standard YAML: top-level keys become Section nodes
        # ------------------------------------------------------------------
        for key in doc.keys():
            key_str = str(key)
            key_line = _key_line(doc, key_str)
            value = doc[key]

            section_name = f"{parent_name}.{key_str}" if is_multi_doc else key_str
            section_qname = make_qualified(file_path, "Section", section_name)

            extra: dict[str, Any] = {"key": key_str}
            if is_helm:
                extra["helm"] = True
            if isinstance(value, CommentedMap):
                extra["value_type"] = "mapping"
                extra["child_keys"] = list(str(k) for k in value.keys())
            elif isinstance(value, CommentedSeq):
                extra["value_type"] = "sequence"
                extra["length"] = len(value)
            else:
                extra["value_type"] = type(value).__name__
                # Store scalar values that are simple enough (not secrets)
                if value is not None and not isinstance(value, (dict, list)):
                    str_val = str(value)
                    if len(str_val) <= 200:
                        extra["value"] = str_val

            nodes.append(NodeInfo(
                kind="Section",
                name=section_name,
                qualified_name=section_qname,
                file_path=file_path,
                line_start=key_line,
                line_end=key_line,
                language="yaml",
                parent_name=parent_name,
                extra=extra,
            ))
            edges.append(EdgeInfo(
                kind="CONTAINS",
                source_qualified=parent_qname,
                target_qualified=section_qname,
                file_path=file_path,
                line=key_line,
            ))

        # Collect alias edges across the whole document
        _collect_aliases(doc, "", parent_qname, file_path, anchor_qnames, edges)

        return nodes, edges

    # ------------------------------------------------------------------
    # Kubernetes manifest parsing
    # ------------------------------------------------------------------

    def _parse_k8s(
        self,
        doc: CommentedMap,
        doc_line: int,
        parent_qname: str,
        parent_name: str,
        file_path: str,
    ) -> tuple[list[NodeInfo], list[EdgeInfo]]:
        nodes: list[NodeInfo] = []
        edges: list[EdgeInfo] = []

        resource_label = _k8s_resource_name(doc)
        api_version = str(doc.get("apiVersion", ""))
        namespace = _k8s_namespace(doc)
        k8s_kind = str(doc.get("kind", ""))

        resource_qname = make_qualified(file_path, "K8sResource", resource_label)

        extra: dict[str, Any] = {
            "apiVersion": api_version,
            "k8s_kind": k8s_kind,
        }
        if namespace:
            extra["namespace"] = namespace

        # Gather labels and annotations for extra metadata
        metadata = doc.get("metadata")
        if isinstance(metadata, CommentedMap):
            labels = metadata.get("labels")
            if isinstance(labels, CommentedMap):
                extra["labels"] = dict(labels)
            annotations = metadata.get("annotations")
            if isinstance(annotations, CommentedMap):
                # Only keep non-huge annotations
                trimmed = {k: v for k, v in annotations.items() if len(str(v)) <= 200}
                if trimmed:
                    extra["annotations"] = trimmed

        nodes.append(NodeInfo(
            kind="K8sResource",
            name=resource_label,
            qualified_name=resource_qname,
            file_path=file_path,
            line_start=doc_line,
            line_end=doc_line,
            language="yaml",
            parent_name=parent_name,
            extra=extra,
        ))
        edges.append(EdgeInfo(
            kind="CONTAINS",
            source_qualified=parent_qname,
            target_qualified=resource_qname,
            file_path=file_path,
            line=doc_line,
        ))

        # Top-level key sections under the resource (spec, metadata, status, …)
        for key in doc.keys():
            key_str = str(key)
            if key_str in ("apiVersion", "kind"):
                continue  # already captured in resource node
            key_line = _key_line(doc, key_str)
            section_name = f"{resource_label}.{key_str}"
            section_qname = make_qualified(file_path, "Section", section_name)

            nodes.append(NodeInfo(
                kind="Section",
                name=section_name,
                qualified_name=section_qname,
                file_path=file_path,
                line_start=key_line,
                line_end=key_line,
                language="yaml",
                parent_name=resource_label,
                extra={"key": key_str, "k8s_section": True},
            ))
            edges.append(EdgeInfo(
                kind="CONTAINS",
                source_qualified=resource_qname,
                target_qualified=section_qname,
                file_path=file_path,
                line=key_line,
            ))

        # Cross-resource REFERENCES (e.g. Deployment → ConfigMap)
        for ref_kind, ref_name in _k8s_referenced_names(doc):
            # We don't know the file of the target — use a placeholder qualified name
            # that cross-reference passes can later resolve.
            target_qname = f"k8s::{ref_kind}/{ref_name}"
            edges.append(EdgeInfo(
                kind="REFERENCES",
                source_qualified=resource_qname,
                target_qualified=target_qname,
                file_path=file_path,
                line=doc_line,
                extra={"ref_kind": ref_kind, "ref_name": ref_name, "unresolved": True},
            ))

        return nodes, edges


# ---------------------------------------------------------------------------
# Module-level singleton (auto-registration via parsers/__init__.py)
# ---------------------------------------------------------------------------

yaml_parser = YAMLParser()
