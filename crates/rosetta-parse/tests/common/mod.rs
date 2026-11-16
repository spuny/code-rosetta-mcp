//! Assertion helpers shared by every parser test file, so all parser tests look alike.
//! Edge targets are written as `"<Kind>::<name>"` for a node of the same file and as
//! `"<scheme>:<value>"` for a reference.
#![allow(dead_code)]
use rosetta_model::{EdgeKind, Language, Node, NodeKind, ParsedFile, Target};

/// Reads `tests/fixtures/<language dir>/<name>`.
pub fn fixture(dir: &str, name: &str) -> String {
    let path = format!("{}/tests/fixtures/{dir}/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {path}: {e}"))
}

/// Parses `text` as `language` under `path`.
pub fn parse(language: Language, path: &str, text: &str) -> ParsedFile {
    rosetta_parse::parse(language, path, text)
}

/// Human-readable listing of all nodes and edges, for failure messages.
pub fn dump(p: &ParsedFile) -> String {
    let mut out = String::new();
    for (i, n) in p.nodes.iter().enumerate() {
        out.push_str(&format!(
            "node {i}: {}::{} parent={:?} lines={}..{} sig={:?} test={} extra={}\n",
            n.kind,
            n.name,
            n.parent,
            n.line_start,
            n.line_end,
            n.signature,
            n.is_test,
            serde_json::Value::Object(n.extra.clone())
        ));
    }
    for e in &p.edges {
        out.push_str(&format!(
            "edge {} {} -> {} line={}\n",
            e.kind,
            label(p, e.source),
            target(p, &e.target),
            e.line
        ));
    }
    if let Some(error) = &p.parse_error {
        out.push_str(&format!("parse_error: {error}\n"));
    }
    out
}

fn label(p: &ParsedFile, index: u32) -> String {
    p.nodes.get(index as usize).map_or_else(
        || format!("<missing {index}>"),
        |n| format!("{}::{}", n.kind, n.name),
    )
}

fn target(p: &ParsedFile, target: &Target) -> String {
    match target {
        Target::Local(i) => label(p, *i),
        Target::Ref(r) => r.to_string(),
    }
}

/// Index of the node `(kind, name)`; panics with a dump when missing.
pub fn index(p: &ParsedFile, kind: NodeKind, name: &str) -> u32 {
    p.nodes
        .iter()
        .position(|n| n.kind == kind && n.name == name)
        .map(|i| i as u32)
        .unwrap_or_else(|| panic!("no node {kind}::{name} in\n{}", dump(p)))
}

/// The node `(kind, name)`; panics with a dump when missing.
pub fn node<'a>(p: &'a ParsedFile, kind: NodeKind, name: &str) -> &'a Node {
    &p.nodes[index(p, kind, name) as usize]
}

pub fn has_node(p: &ParsedFile, kind: NodeKind, name: &str) -> bool {
    p.nodes.iter().any(|n| n.kind == kind && n.name == name)
}

/// Names of all nodes of `kind`, in order.
pub fn names(p: &ParsedFile, kind: NodeKind) -> Vec<String> {
    p.nodes
        .iter()
        .filter(|n| n.kind == kind)
        .map(|n| n.name.clone())
        .collect()
}

/// The parent of `(kind, name)` as `"<Kind>::<name>"`.
pub fn parent(p: &ParsedFile, kind: NodeKind, name: &str) -> String {
    let n = node(p, kind, name);
    n.parent.map_or_else(|| "<none>".into(), |i| label(p, i))
}

/// Targets of every `edge` edge whose source is `(source_kind, source_name)`, sorted.
pub fn targets(
    p: &ParsedFile,
    edge: EdgeKind,
    source_kind: NodeKind,
    source_name: &str,
) -> Vec<String> {
    let source = index(p, source_kind, source_name);
    let mut out: Vec<String> = p
        .edges
        .iter()
        .filter(|e| e.kind == edge && e.source == source)
        .map(|e| target(p, &e.target))
        .collect();
    out.sort();
    out
}

/// Asserts an edge `edge` from `(source_kind, source_name)` to `target` exists.
pub fn assert_edge(
    p: &ParsedFile,
    edge: EdgeKind,
    source_kind: NodeKind,
    source_name: &str,
    target: &str,
) {
    let found = targets(p, edge, source_kind, source_name);
    assert!(
        found.iter().any(|t| t == target),
        "no {edge} edge {source_kind}::{source_name} -> {target}; found {found:?} in\n{}",
        dump(p)
    );
}

/// Asserts the invariants every parser output must satisfy: node 0 is the File node, parents
/// point backwards, names are unique per kind, edges point at existing nodes, no CONTAINS edges.
pub fn assert_well_formed(p: &ParsedFile) {
    assert_eq!(
        p.nodes.first().map(|n| n.kind),
        Some(NodeKind::File),
        "{}",
        dump(p)
    );
    let mut seen = std::collections::HashSet::new();
    for (i, n) in p.nodes.iter().enumerate() {
        if i == 0 {
            assert_eq!(n.parent, None, "{}", dump(p));
        } else {
            assert!(
                n.parent.is_some_and(|parent| (parent as usize) < i),
                "bad parent of node {i}\n{}",
                dump(p)
            );
        }
        assert!(
            seen.insert((n.kind, n.name.clone())),
            "duplicate {}::{}\n{}",
            n.kind,
            n.name,
            dump(p)
        );
    }
    for e in &p.edges {
        assert_ne!(e.kind, EdgeKind::Contains, "{}", dump(p));
        assert!((e.source as usize) < p.nodes.len(), "{}", dump(p));
        if let Target::Local(t) = e.target {
            assert!((t as usize) < p.nodes.len(), "{}", dump(p));
        }
    }
}
