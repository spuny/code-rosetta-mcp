//! What a parser produces for one file.
use crate::{
    kinds::{EdgeKind, Language, NodeKind, RefScheme},
    text::{cut, cut_value, line_count},
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};

pub type Extra = Map<String, Value>;

/// Longest node name kept, in characters (longer names would break the database's btree indexes).
pub const MAX_NAME_CHARS: usize = 300;

/// One node of a file. Build it with [`Node::new`] and the chainable setters, then add it with
/// [`ParsedFile::add_node`], which enforces the uniqueness rule.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub kind: NodeKind,
    /// Unique per `(kind)` within the file (see [`ParsedFile::add_node`]).
    pub name: String,
    /// Index into [`ParsedFile::nodes`]; `None` only for the File node (index 0). The store derives
    /// the CONTAINS edges from it, so parsers never emit CONTAINS themselves.
    pub parent: Option<u32>,
    /// 1-based; 0 = unknown.
    pub line_start: u32,
    pub line_end: u32,
    /// Parameters / arguments as text, "" if none.
    pub signature: String,
    pub is_test: bool,
    pub extra: Extra,
}

impl Node {
    /// A node under `parent` (use 0 for the File node) spanning `line_start..=line_end`.
    pub fn new(
        kind: NodeKind,
        name: impl Into<String>,
        parent: u32,
        line_start: u32,
        line_end: u32,
    ) -> Self {
        Self {
            kind,
            name: name.into(),
            parent: Some(parent),
            line_start,
            line_end: line_end.max(line_start),
            signature: String::new(),
            is_test: false,
            extra: Extra::new(),
        }
    }

    pub fn signature(mut self, signature: impl Into<String>) -> Self {
        self.signature = cut(&signature.into());
        self
    }

    pub fn test(mut self, is_test: bool) -> Self {
        self.is_test = is_test;
        self
    }

    /// Adds `key` to `extra`; strings are cut to 200 characters, arrays to 100 elements.
    pub fn extra(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.extra.insert(key.to_owned(), cut_value(value.into()));
        self
    }
}

/// A reference that the linker resolves after publication (docs/design.md section 4 defines the
/// value format per scheme).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Ref {
    pub scheme: RefScheme,
    pub value: String,
}

impl Ref {
    pub fn new(scheme: RefScheme, value: impl Into<String>) -> Self {
        Self {
            scheme,
            value: value.into(),
        }
    }
}

impl std::fmt::Display for Ref {
    /// `scheme:value`, the `target_ref` text of the API.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.scheme, self.value)
    }
}

/// Edge target: a node of the same file, or a reference for the linker.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Target {
    Local(u32),
    Ref(Ref),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub kind: EdgeKind,
    /// Index into [`ParsedFile::nodes`].
    pub source: u32,
    pub target: Target,
    /// 1-based; 0 = unknown.
    pub line: u32,
    pub extra: Extra,
}

impl Edge {
    /// Edge to a node of the same file.
    pub fn local(kind: EdgeKind, source: u32, target: u32, line: u32) -> Self {
        Self {
            kind,
            source,
            target: Target::Local(target),
            line,
            extra: Extra::new(),
        }
    }

    /// Edge to a reference.
    pub fn to_ref(
        kind: EdgeKind,
        source: u32,
        scheme: RefScheme,
        value: impl Into<String>,
        line: u32,
    ) -> Self {
        Self {
            kind,
            source,
            target: Target::Ref(Ref::new(scheme, value)),
            line,
            extra: Extra::new(),
        }
    }

    /// Adds `key` to `extra` (cut like [`Node::extra`]).
    pub fn extra(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.extra.insert(key.to_owned(), cut_value(value.into()));
        self
    }
}

/// The parse result of one file. `nodes[0]` is always the File node.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ParsedFile {
    pub language: Language,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub parse_error: Option<String>,
    #[serde(skip)]
    names: HashMap<(NodeKind, String), u32>,
    #[serde(skip)]
    next_suffix: HashMap<(NodeKind, String), u32>,
    #[serde(skip)]
    edge_keys: HashSet<(EdgeKind, u32, Target, u32)>,
}

impl ParsedFile {
    /// A result holding only the File node: name = base name of `path`, lines 1..=line count of
    /// `text` (0..=0 for empty text).
    pub fn new(language: Language, path: &str, text: &str) -> Self {
        let name = path.rsplit('/').next().unwrap_or(path);
        let lines = line_count(text);
        let file = Node {
            kind: NodeKind::File,
            name: name.to_owned(),
            parent: None,
            line_start: u32::from(lines > 0),
            line_end: lines,
            signature: String::new(),
            is_test: false,
            extra: Extra::new(),
        };
        let mut names = HashMap::new();
        names.insert((NodeKind::File, file.name.clone()), 0);
        Self {
            language,
            nodes: vec![file],
            edges: Vec::new(),
            parse_error: None,
            names,
            next_suffix: HashMap::new(),
            edge_keys: HashSet::new(),
        }
    }

    /// The File node alone with `parse_error` set (the worst case every parser may return).
    pub fn failed(language: Language, path: &str, text: &str, error: impl Into<String>) -> Self {
        let mut parsed = Self::new(language, path, text);
        parsed.set_error(error);
        parsed
    }

    /// Records a parse error (cut to 200 characters); the first error wins.
    pub fn set_error(&mut self, error: impl Into<String>) {
        if self.parse_error.is_none() {
            self.parse_error = Some(cut(&error.into()));
        }
    }

    /// The File node, for setting `is_test` or `extra`.
    pub fn file_mut(&mut self) -> &mut Node {
        &mut self.nodes[0]
    }

    /// Adds a node and returns its index. A parent index that does not exist yet is replaced by
    /// 0 (the File node). When `(kind, name)` is taken, the name gets the suffix `@<line_start>`,
    /// and `@<line_start>.<n>` if that is taken too, so names stay unique per kind. Names are cut
    /// to [`MAX_NAME_CHARS`] characters first. Each duplicate costs O(1) (a counter per base name).
    pub fn add_node(&mut self, mut node: Node) -> u32 {
        let index = u32::try_from(self.nodes.len()).unwrap_or(u32::MAX);
        node.parent = Some(match node.parent {
            Some(parent) if parent < index => parent,
            _ => 0,
        });
        if node.kind == NodeKind::File {
            node.kind = NodeKind::Section;
        }
        if node.name.chars().count() > MAX_NAME_CHARS {
            node.name = node.name.chars().take(MAX_NAME_CHARS).collect();
        }
        if self.names.contains_key(&(node.kind, node.name.clone())) {
            let base = format!("{}@{}", node.name, node.line_start);
            if self.names.contains_key(&(node.kind, base.clone())) {
                let counter = self
                    .next_suffix
                    .entry((node.kind, base.clone()))
                    .or_insert(2);
                let mut candidate = format!("{base}.{counter}");
                *counter += 1;
                // A literal name such as `f@5.3` can occupy a slot; skipping it keeps names unique.
                while self.names.contains_key(&(node.kind, candidate.clone())) {
                    candidate = format!("{base}.{counter}");
                    *counter += 1;
                }
                node.name = candidate;
            } else {
                node.name = base;
            }
        }
        self.names.insert((node.kind, node.name.clone()), index);
        self.nodes.push(node);
        index
    }

    /// Index of the node `(kind, name)` (exact, as stored after any `@<line>` suffix).
    pub fn find(&self, kind: NodeKind, name: &str) -> Option<u32> {
        self.names.get(&(kind, name.to_owned())).copied()
    }

    /// Adds an edge unless an identical `(kind, source, target, line)` edge exists. Returns
    /// whether it was added. Edges whose source or local target index does not exist, and
    /// CONTAINS edges (derived by the store), are dropped.
    pub fn add_edge(&mut self, edge: Edge) -> bool {
        let count = u32::try_from(self.nodes.len()).unwrap_or(u32::MAX);
        let target_ok = match &edge.target {
            Target::Local(t) => *t < count,
            Target::Ref(r) => !r.value.is_empty(),
        };
        if edge.kind == EdgeKind::Contains || edge.source >= count || !target_ok {
            return false;
        }
        let key = (edge.kind, edge.source, edge.target.clone(), edge.line);
        if !self.edge_keys.insert(key) {
            return false;
        }
        self.edges.push(edge);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_node_and_unique_names() {
        let mut p = ParsedFile::new(Language::Python, "pkg/app.py", "a\nb\n");
        assert_eq!(
            (
                p.nodes[0].name.as_str(),
                p.nodes[0].line_start,
                p.nodes[0].line_end
            ),
            ("app.py", 1, 2)
        );
        let a = p.add_node(Node::new(NodeKind::Function, "f", 0, 1, 1));
        let b = p.add_node(Node::new(NodeKind::Function, "f", 0, 5, 6));
        let c = p.add_node(Node::new(NodeKind::Function, "f", 0, 5, 6));
        let d = p.add_node(Node::new(NodeKind::Class, "f", 99, 7, 8));
        assert_eq!(p.nodes[a as usize].name, "f");
        assert_eq!(p.nodes[b as usize].name, "f@5");
        assert_eq!(p.nodes[c as usize].name, "f@5.2");
        assert_eq!(
            (
                p.nodes[d as usize].name.as_str(),
                p.nodes[d as usize].parent
            ),
            ("f", Some(0))
        );
        assert_eq!(p.find(NodeKind::Function, "f@5"), Some(b));
        assert_eq!(
            ParsedFile::new(Language::Yaml, "x.yaml", "").nodes[0].line_end,
            0
        );
    }

    #[test]
    fn many_duplicates_on_one_line_take_linear_time() {
        let mut p = ParsedFile::new(Language::Yaml, "x.yaml", "a");
        let started = std::time::Instant::now();
        for _ in 0..100_000 {
            p.add_node(Node::new(NodeKind::Section, "a", 0, 1, 1));
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        assert_eq!(p.nodes[1].name, "a");
        assert_eq!(p.nodes[2].name, "a@1");
        assert_eq!(p.nodes[3].name, "a@1.2");
        assert_eq!(p.nodes[100_000].name, "a@1.99999");
        let names: HashSet<_> = p.nodes.iter().map(|n| &n.name).collect();
        assert_eq!(names.len(), p.nodes.len());
    }

    #[test]
    fn literal_suffix_names_are_skipped_and_long_names_cut() {
        let mut p = ParsedFile::new(Language::Bash, "run.sh", "x");
        p.add_node(Node::new(NodeKind::Function, "f", 0, 5, 5));
        p.add_node(Node::new(NodeKind::Function, "f@5", 0, 1, 1));
        p.add_node(Node::new(NodeKind::Function, "f@5.2", 0, 1, 1));
        let d = p.add_node(Node::new(NodeKind::Function, "f", 0, 5, 5));
        assert_eq!(p.nodes[d as usize].name, "f@5.3");
        let long = p.add_node(Node::new(NodeKind::Function, "é".repeat(5000), 0, 1, 1));
        assert_eq!(p.nodes[long as usize].name.chars().count(), MAX_NAME_CHARS);
    }

    #[test]
    fn edges_are_deduplicated_and_validated() {
        let mut p = ParsedFile::new(Language::Bash, "run.sh", "x");
        let f = p.add_node(Node::new(NodeKind::Function, "main", 0, 1, 1));
        assert!(p.add_edge(Edge::local(EdgeKind::Calls, 0, f, 3)));
        assert!(!p.add_edge(Edge::local(EdgeKind::Calls, 0, f, 3)));
        assert!(p.add_edge(Edge::local(EdgeKind::Calls, 0, f, 4)));
        assert!(!p.add_edge(Edge::local(EdgeKind::Calls, 0, 42, 4)));
        assert!(!p.add_edge(Edge::local(EdgeKind::Contains, 0, f, 1)));
        assert!(p.add_edge(Edge::to_ref(
            EdgeKind::Imports,
            f,
            RefScheme::File,
            "./lib.sh",
            2
        )));
        assert!(!p.add_edge(Edge::to_ref(EdgeKind::Imports, f, RefScheme::File, "", 2)));
        assert_eq!(p.edges.len(), 3);
        assert_eq!(Ref::new(RefScheme::Py, "os.path").to_string(), "py:os.path");
    }

    #[test]
    fn errors_and_extra_are_bounded() {
        let mut p = ParsedFile::failed(Language::Hcl, "main.tf", "x", "e".repeat(500));
        p.set_error("second");
        assert_eq!(p.parse_error.as_deref().map(str::len), Some(200));
        let n = Node::new(NodeKind::Variable, "v", 0, 1, 1).extra("default", "x".repeat(300));
        assert_eq!(n.extra["default"].as_str().map(str::len), Some(200));
    }
}
