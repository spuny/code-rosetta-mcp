//! YAML parser (owner P3). Contract: docs/design.md section 4, "Per-language output".
//! Event parser: `saphyr-parser` (see docs/parser-notes.md).
//!
//! The event stream is folded into a small positioned tree (depth-capped), then nodes and edges
//! are emitted from it: Documents (multi-document files only), Sections (one per top-level key),
//! Kubernetes resources with their references, anchors and aliases, kustomization file refs.
use rosetta_model::{
    Edge, EdgeKind, Language, Node, NodeKind, ParsedFile, RefScheme, cut, cut_value, is_secret_key,
};
use saphyr_parser::{Event, Parser, ScalarStyle};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};

/// Containers nested deeper than this are replaced by an empty placeholder when the tree is built.
const MAX_DEPTH: usize = 64;
/// Keys listed in `extra.child_keys`.
const MAX_CHILD_KEYS: usize = 50;
/// Depth of the dotted paths in `extra.spec_keys`.
const SPEC_KEY_DEPTH: usize = 6;
/// Entries kept in `extra.spec_keys`.
const MAX_SPEC_KEYS: usize = 100;
/// Entries kept in `extra.labels` and `extra.images`.
const MAX_LABELS: usize = 30;
const MAX_IMAGES: usize = 20;
/// Longest scalar stored as `extra.value`.
const MAX_VALUE_CHARS: usize = 200;

const HELM_KEYS: &[&str] = &[
    "replicaCount",
    "image",
    "service",
    "ingress",
    "resources",
    "nodeSelector",
    "tolerations",
    "affinity",
    "serviceAccount",
    "podAnnotations",
    "podSecurityContext",
    "securityContext",
    "livenessProbe",
    "readinessProbe",
    "autoscaling",
];

/// Parses one file. Must be total: never panic, bound recursion, stay fast on 1 MiB input.
pub(crate) fn parse(path: &str, text: &str) -> ParsedFile {
    let mut out = ParsedFile::new(Language::Yaml, path, text);
    extract(text, path, false, &mut out);
    out
}

/// Extracts the YAML nodes and edges of `text` into `out`, appending them under node 0 (the File
/// node, which the caller has already created). `templated` is true when `text` is a masked Helm
/// template (Go template actions replaced by same-length filler, so byte offsets and line numbers
/// match the original file); every node added then gets `extra.templated = true`. A YAML syntax
/// error sets `out.parse_error` (only if none is set) and keeps whatever was extracted before it.
/// Frozen interface: the Go template parser (owner P4) calls it in wave 2.
pub(crate) fn extract(text: &str, path: &str, templated: bool, out: &mut ParsedFile) {
    let (roots, error) = build(text);
    let live: Vec<&YNode> = roots.iter().filter(|r| !r.is_null()).collect();
    let file_name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    let helm = is_helm(&file_name, live.first().copied());
    let kustomization = matches!(
        file_name.as_str(),
        "kustomization.yaml" | "kustomization.yml"
    );
    let mut em = Emit { out, templated };
    if helm {
        em.out
            .file_mut()
            .extra
            .insert("helm".into(), Value::Bool(true));
    }
    let multi = live.len() > 1;
    for (index, root) in live.iter().enumerate() {
        emit_document(&mut em, root, index, multi, helm, kustomization);
    }
    if let Some(error) = error {
        em.out.set_error(error);
    }
}

// ---------------------------------------------------------------------------------------------
// Positioned tree

struct YNode {
    line: u32,
    end_line: u32,
    anchor: Option<(usize, String)>,
    body: Body,
}

enum Body {
    Scalar { text: String, plain: bool },
    Map(Vec<(YNode, YNode)>),
    Seq(Vec<YNode>),
    Alias { id: usize, name: String },
}

impl YNode {
    fn scalar(&self) -> Option<&str> {
        match &self.body {
            Body::Scalar { text, .. } => Some(text),
            _ => None,
        }
    }

    fn get(&self, key: &str) -> Option<&YNode> {
        match &self.body {
            Body::Map(items) => items
                .iter()
                .find(|(k, _)| k.scalar() == Some(key))
                .map(|(_, v)| v),
            _ => None,
        }
    }

    fn items(&self) -> &[YNode] {
        match &self.body {
            Body::Seq(items) => items,
            _ => &[],
        }
    }

    fn entries(&self) -> &[(YNode, YNode)] {
        match &self.body {
            Body::Map(items) => items,
            _ => &[],
        }
    }

    fn is_null(&self) -> bool {
        match &self.body {
            Body::Scalar { text, plain } => {
                self.anchor.is_none()
                    && *plain
                    && matches!(text.as_str(), "" | "~" | "null" | "Null" | "NULL")
            }
            _ => false,
        }
    }

    fn type_name(&self) -> &'static str {
        match &self.body {
            Body::Map(_) => "mapping",
            Body::Seq(_) => "sequence",
            Body::Alias { .. } => "alias",
            Body::Scalar { text, plain } => {
                if !*plain {
                    "string"
                } else if matches!(text.as_str(), "" | "~" | "null" | "Null" | "NULL") {
                    "null"
                } else if matches!(
                    text.as_str(),
                    "true" | "True" | "TRUE" | "false" | "False" | "FALSE"
                ) {
                    "bool"
                } else if text.parse::<i64>().is_ok() {
                    "int"
                } else if text.parse::<f64>().is_ok() {
                    "float"
                } else {
                    "string"
                }
            }
        }
    }
}

enum Frame {
    Map {
        items: Vec<(YNode, YNode)>,
        key: Option<YNode>,
        line: u32,
        anchor: Option<(usize, String)>,
    },
    Seq {
        items: Vec<YNode>,
        line: u32,
        anchor: Option<(usize, String)>,
    },
}

struct Builder<'a> {
    text: &'a str,
    ascii: bool,
    char_to_byte: Option<Vec<usize>>,
    frames: Vec<Frame>,
    skip: usize,
    root: Option<YNode>,
    anchors: HashMap<usize, String>,
}

fn line_of(index: usize) -> u32 {
    u32::try_from(index).unwrap_or(u32::MAX)
}

impl<'a> Builder<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            ascii: text.is_ascii(),
            char_to_byte: None,
            frames: Vec::new(),
            skip: 0,
            root: None,
            anchors: HashMap::new(),
        }
    }

    fn reset_document(&mut self) {
        self.frames.clear();
        self.skip = 0;
        self.root = None;
        self.anchors.clear();
    }

    /// Anchor names are not in the events: read them from the source at the node's start.
    fn anchor(&mut self, id: usize, start_char: usize) -> Option<(usize, String)> {
        if id == 0 {
            return None;
        }
        let byte = if self.ascii {
            Some(start_char)
        } else {
            let text = self.text;
            self.char_to_byte
                .get_or_insert_with(|| text.char_indices().map(|(b, _)| b).collect())
                .get(start_char)
                .copied()
        };
        let name = byte
            .and_then(|b| read_anchor_name(self.text, b))
            .unwrap_or_else(|| format!("anchor{id}"));
        self.anchors.insert(id, name.clone());
        Some((id, name))
    }

    fn complete(&mut self, node: YNode) {
        match self.frames.last_mut() {
            Some(Frame::Map { items, key, .. }) => match key.take() {
                None => *key = Some(node),
                Some(k) => items.push((k, node)),
            },
            Some(Frame::Seq { items, .. }) => items.push(node),
            None => self.root = Some(node),
        }
    }

    fn open(&mut self, is_map: bool, id: usize, start_char: usize, line: u32) {
        if self.skip > 0 {
            self.skip += 1;
            return;
        }
        if self.frames.len() >= MAX_DEPTH {
            let body = if is_map {
                Body::Map(Vec::new())
            } else {
                Body::Seq(Vec::new())
            };
            self.complete(YNode {
                line,
                end_line: line,
                anchor: None,
                body,
            });
            self.skip = 1;
            return;
        }
        let anchor = self.anchor(id, start_char);
        self.frames.push(if is_map {
            Frame::Map {
                items: Vec::new(),
                key: None,
                line,
                anchor,
            }
        } else {
            Frame::Seq {
                items: Vec::new(),
                line,
                anchor,
            }
        });
    }

    fn close(&mut self) {
        if self.skip > 0 {
            self.skip -= 1;
            return;
        }
        let node = match self.frames.pop() {
            Some(Frame::Map {
                items,
                key,
                line,
                anchor,
            }) => {
                let mut end = line;
                for (k, v) in &items {
                    end = end.max(k.end_line).max(v.end_line);
                }
                if let Some(k) = &key {
                    end = end.max(k.end_line);
                }
                YNode {
                    line,
                    end_line: end,
                    anchor,
                    body: Body::Map(items),
                }
            }
            Some(Frame::Seq {
                items,
                line,
                anchor,
            }) => {
                let end = items.iter().map(|i| i.end_line).fold(line, u32::max);
                YNode {
                    line,
                    end_line: end,
                    anchor,
                    body: Body::Seq(items),
                }
            }
            None => return,
        };
        self.complete(node);
    }
}

fn read_anchor_name(text: &str, byte: usize) -> Option<String> {
    let mut rest = text.get(byte..)?;
    if rest.starts_with('!') {
        rest = rest
            .trim_start_matches(|c: char| !c.is_whitespace())
            .trim_start_matches([' ', '\t']);
    }
    let rest = rest.strip_prefix('&')?;
    let name: String = rest
        .chars()
        .take_while(|c| !c.is_whitespace() && !matches!(c, '[' | ']' | '{' | '}' | ','))
        .take(200)
        .collect();
    (!name.is_empty()).then_some(name)
}

/// Folds the event stream into one root per document. On a scan error the documents completed
/// before it and the part of the broken document that precedes the error are returned together
/// with the error text.
fn build(text: &str) -> (Vec<YNode>, Option<String>) {
    let mut roots = Vec::new();
    let mut error = None;
    let mut b = Builder::new(text);
    let mut parser = Parser::new_from_str(text);
    while let Some(next) = parser.next_event() {
        let (event, span) = match next {
            Ok(pair) => pair,
            Err(e) => {
                error = Some(e.to_string());
                // Keep what the broken document had before the error: close its open containers.
                b.skip = 0;
                while !b.frames.is_empty() {
                    b.close();
                }
                if let Some(root) = b.root.take() {
                    roots.push(root);
                }
                break;
            }
        };
        let line = line_of(span.start.line());
        match event {
            Event::DocumentStart(_) => b.reset_document(),
            Event::DocumentEnd => {
                if let Some(root) = b.root.take() {
                    roots.push(root);
                }
                b.reset_document();
            }
            Event::StreamEnd => break,
            Event::Scalar(value, style, id, _) => {
                if b.skip > 0 {
                    continue;
                }
                let end_line = line_of(span.end.line());
                let end_line = if matches!(style, ScalarStyle::Literal | ScalarStyle::Folded)
                    && end_line > line
                {
                    end_line - 1
                } else {
                    end_line.max(line)
                };
                let anchor = b.anchor(id, span.start.index());
                b.complete(YNode {
                    line,
                    end_line,
                    anchor,
                    body: Body::Scalar {
                        text: value.into_owned(),
                        plain: matches!(style, ScalarStyle::Plain),
                    },
                });
            }
            Event::Alias(id) => {
                if b.skip > 0 {
                    continue;
                }
                let name = b.anchors.get(&id).cloned().unwrap_or_default();
                b.complete(YNode {
                    line,
                    end_line: line,
                    anchor: None,
                    body: Body::Alias { id, name },
                });
            }
            Event::MappingStart(id, _) => b.open(true, id, span.start.index(), line),
            Event::SequenceStart(id, _) => b.open(false, id, span.start.index(), line),
            Event::MappingEnd | Event::SequenceEnd => b.close(),
            Event::StreamStart | Event::Nothing => {}
        }
    }
    (roots, error)
}

// ---------------------------------------------------------------------------------------------
// Emission

struct Emit<'a> {
    out: &'a mut ParsedFile,
    templated: bool,
}

impl Emit<'_> {
    fn node(&mut self, node: Node) -> u32 {
        let node = if self.templated {
            node.extra("templated", true)
        } else {
            node
        };
        self.out.add_node(node)
    }

    fn edge(&mut self, edge: Edge) {
        self.out.add_edge(edge);
    }
}

/// Per-document emission state.
struct Scope {
    prefix: String,
    anchors: HashMap<usize, u32>,
}

fn key_text(key: &YNode) -> String {
    let raw = key.scalar().unwrap_or("?");
    let cleaned: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let cleaned = cut(cleaned.trim());
    if cleaned.is_empty() {
        "(empty)".to_owned()
    } else {
        cleaned
    }
}

fn is_helm(file_name: &str, first: Option<&YNode>) -> bool {
    if matches!(
        file_name,
        "values.yaml" | "values.yml" | "chart.yaml" | "chart.yml"
    ) {
        return true;
    }
    let Some(root) = first else {
        return false;
    };
    let keys: Vec<&str> = root
        .entries()
        .iter()
        .filter_map(|(k, _)| k.scalar())
        .collect();
    if keys.iter().filter(|k| HELM_KEYS.contains(k)).count() >= 3 {
        return true;
    }
    ["apiVersion", "name", "version"]
        .iter()
        .all(|k| keys.contains(k))
        && !keys.contains(&"kind")
}

fn emit_document(
    em: &mut Emit<'_>,
    root: &YNode,
    index: usize,
    multi: bool,
    helm: bool,
    kustomization: bool,
) {
    let doc = if multi {
        em.node(
            Node::new(
                NodeKind::Document,
                format!("doc{index}"),
                0,
                root.line,
                root.end_line,
            )
            .extra("doc_index", index),
        )
    } else {
        0
    };
    let mut scope = Scope {
        prefix: if multi {
            format!("doc{index}.")
        } else {
            String::new()
        },
        anchors: HashMap::new(),
    };
    let Body::Map(items) = &root.body else {
        walk(em, &mut scope, root, doc);
        return;
    };
    register_anchor(em, &mut scope, root, doc);

    let resource = k8s_resource(em, root, doc);
    let section_parent = resource.as_ref().map_or(doc, |r| r.index);
    let mut sections: HashMap<String, u32> = HashMap::new();
    for (key, value) in items {
        let name = key_text(key);
        if resource.is_some() && matches!(name.as_str(), "apiVersion" | "kind") {
            continue;
        }
        let section_name = match &resource {
            Some(r) => format!("{}.{name}", r.name),
            None => format!("{}{name}", scope.prefix),
        };
        let mut node = Node::new(
            NodeKind::Section,
            section_name,
            section_parent,
            key.line,
            value.end_line.max(key.line),
        )
        .extra("key", name.as_str())
        .extra("value_type", value.type_name());
        if resource.is_some() {
            node = node.extra("k8s_section", true);
        } else if helm {
            node = node.extra("helm", true);
        }
        match &value.body {
            Body::Map(children) => {
                let keys: Vec<Value> = children
                    .iter()
                    .take(MAX_CHILD_KEYS)
                    .map(|(k, _)| Value::String(key_text(k)))
                    .collect();
                node = node.extra("child_keys", Value::Array(keys));
            }
            Body::Seq(children) => node = node.extra("length", children.len()),
            Body::Scalar { text, .. } => {
                if storable_value(&name, text) {
                    node = node.extra("value", text.as_str());
                }
            }
            Body::Alias { .. } => {}
        }
        let section = em.node(node);
        sections.entry(name).or_insert(section);
        walk(em, &mut scope, key, section);
        walk(em, &mut scope, value, section);
    }
    if let Some(r) = &resource {
        k8s_refs(em, root, r);
    }
    if kustomization || resource.as_ref().is_some_and(|r| r.kind == "Kustomization") {
        kustomize_refs(em, items, &sections);
    }
}

/// Whether the scalar value of top-level key `name` may be stored.
fn storable_value(name: &str, text: &str) -> bool {
    if is_secret_key(name) || text.chars().count() > MAX_VALUE_CHARS || text.contains("-----BEGIN")
    {
        return false;
    }
    !has_url_credentials(text)
}

/// `scheme://user:password@host` anywhere in the text.
fn has_url_credentials(text: &str) -> bool {
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        let after = &rest[at + 3..];
        let authority: &str = after
            .split(['/', ' ', '\n', '"', '\''])
            .next()
            .unwrap_or("");
        if let Some((userinfo, _)) = authority.split_once('@')
            && userinfo.contains(':')
        {
            return true;
        }
        rest = after;
    }
    false
}

// --- anchors and aliases ---------------------------------------------------------------------

fn register_anchor(em: &mut Emit<'_>, scope: &mut Scope, n: &YNode, owner: u32) {
    if let Some((id, name)) = &n.anchor {
        let node = em.node(
            Node::new(
                NodeKind::Anchor,
                format!("{}&{name}", scope.prefix),
                owner,
                n.line,
                n.end_line,
            )
            .extra("anchor_name", name.as_str())
            .extra("value_type", n.type_name()),
        );
        scope.anchors.insert(*id, node);
    }
}

/// Registers every anchor below `n` as an Anchor node under `owner` and turns every alias into a
/// REFERENCES edge from `owner` to its anchor. Tree depth is bounded by [`MAX_DEPTH`].
fn walk(em: &mut Emit<'_>, scope: &mut Scope, n: &YNode, owner: u32) {
    register_anchor(em, scope, n, owner);
    match &n.body {
        Body::Alias { id, name } => {
            if let Some(&anchor) = scope.anchors.get(id) {
                em.edge(
                    Edge::local(EdgeKind::References, owner, anchor, n.line)
                        .extra("via", "alias")
                        .extra("anchor", name.as_str()),
                );
            }
        }
        Body::Map(items) => {
            for (k, v) in items {
                walk(em, scope, k, owner);
                walk(em, scope, v, owner);
            }
        }
        Body::Seq(items) => {
            for item in items {
                walk(em, scope, item, owner);
            }
        }
        Body::Scalar { .. } => {}
    }
}

// --- Kubernetes ------------------------------------------------------------------------------

struct Resource {
    index: u32,
    name: String,
    kind: String,
    namespace: Option<String>,
}

fn k8s_resource(em: &mut Emit<'_>, root: &YNode, parent: u32) -> Option<Resource> {
    let api_version = root.get("apiVersion")?.scalar()?.trim();
    let kind = root.get("kind")?.scalar()?.trim();
    if api_version.is_empty() || kind.is_empty() || !kind.chars().all(|c| c.is_ascii_alphanumeric())
    {
        return None;
    }
    let metadata = root.get("metadata");
    let meta_str = |key: &str| {
        metadata
            .and_then(|m| m.get(key))
            .and_then(YNode::scalar)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let literal = |s: &str| {
        if em.templated {
            unmask(s)
        } else {
            s.to_owned()
        }
    };
    let name = match meta_str("name").or_else(|| meta_str("generateName")) {
        Some(n) => format!("{kind}/{}", cut(&literal(n))),
        None => kind.to_owned(),
    };
    let namespace = meta_str("namespace")
        .filter(|ns| !(em.templated && looks_like_filler(ns)))
        .map(cut);
    let mut node = Node::new(
        NodeKind::K8sResource,
        name.as_str(),
        parent,
        root.line,
        root.end_line,
    )
    .extra("apiVersion", api_version)
    .extra("k8s_kind", kind);
    if let Some(ns) = &namespace {
        node = node.extra("namespace", ns.as_str());
    }
    if let Some(labels) = metadata.and_then(|m| m.get("labels")) {
        let mut map = Map::new();
        for (k, v) in labels.entries() {
            if map.len() >= MAX_LABELS {
                break;
            }
            if let (Some(k), Some(v)) = (k.scalar(), v.scalar())
                && !is_secret_key(k)
            {
                map.insert(cut(k), Value::String(cut(v)));
            }
        }
        if !map.is_empty() {
            node = node.extra("labels", Value::Object(map));
        }
    }
    if let Some(spec) = root.get("spec") {
        let mut keys = Vec::new();
        let mut seen = HashSet::new();
        spec_keys(spec, "", 1, &mut keys, &mut seen);
        if !keys.is_empty() {
            node = node.extra("spec_keys", cut_value(Value::from(keys)));
        }
    }
    let images = collect_images(root);
    if !images.is_empty() {
        node = node.extra("images", Value::from(images));
    }
    let index = em.node(node);
    let stored = em.out.nodes[index as usize].name.clone();
    Some(Resource {
        index,
        name: stored,
        kind: kind.to_owned(),
        namespace,
    })
}

/// Dotted key paths of a mapping to `SPEC_KEY_DEPTH` levels, sequences transparent, pre-order.
fn spec_keys(
    n: &YNode,
    prefix: &str,
    depth: usize,
    out: &mut Vec<String>,
    seen: &mut HashSet<String>,
) {
    if depth > SPEC_KEY_DEPTH || out.len() >= MAX_SPEC_KEYS {
        return;
    }
    match &n.body {
        Body::Map(items) => {
            for (k, v) in items {
                let Some(key) = k.scalar() else { continue };
                let path = if prefix.is_empty() {
                    key.to_owned()
                } else {
                    format!("{prefix}.{key}")
                };
                if out.len() >= MAX_SPEC_KEYS {
                    return;
                }
                if seen.insert(path.clone()) {
                    out.push(path.clone());
                }
                spec_keys(v, &path, depth + 1, out, seen);
            }
        }
        Body::Seq(items) => {
            for item in items {
                spec_keys(item, prefix, depth, out, seen);
            }
        }
        _ => {}
    }
}

const CONTAINER_KEYS: [&str; 3] = ["containers", "initContainers", "ephemeralContainers"];

fn collect_images(root: &YNode) -> Vec<String> {
    fn visit(n: &YNode, depth: usize, out: &mut Vec<String>) {
        if depth > 16 || out.len() >= MAX_IMAGES {
            return;
        }
        match &n.body {
            Body::Map(items) => {
                for key in CONTAINER_KEYS {
                    if let Some(containers) = n.get(key) {
                        for c in containers.items() {
                            if let Some(image) = c.get("image").and_then(YNode::scalar)
                                && !image.is_empty()
                                && out.len() < MAX_IMAGES
                                && !out.iter().any(|i| i == image)
                            {
                                out.push(cut(image));
                            }
                        }
                    }
                }
                for (_, v) in items {
                    visit(v, depth + 1, out);
                }
            }
            Body::Seq(items) => {
                for item in items {
                    visit(item, depth + 1, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    visit(root, 0, &mut out);
    out
}

struct Refs<'a, 'b> {
    em: &'a mut Emit<'b>,
    source: u32,
    namespace: Option<&'a str>,
}

impl Refs<'_, '_> {
    fn add(&mut self, kind: &str, name: Option<&YNode>, via: &str) {
        let Some(name) = name else { return };
        let Some(text) = name.scalar() else { return };
        let text = text.trim();
        if text.is_empty() || (self.em.templated && looks_like_filler(text)) {
            return;
        }
        let mut edge = Edge::to_ref(
            EdgeKind::References,
            self.source,
            RefScheme::K8s,
            format!("{kind}/{}", cut(text)),
            name.line,
        )
        .extra("ref_kind", kind)
        .extra("ref_name", text)
        .extra("via", via);
        if let Some(ns) = self.namespace {
            edge = edge.extra("namespace", ns);
        }
        self.em.edge(edge);
    }

    fn scan(&mut self, n: &YNode, depth: usize) {
        if depth > 16 {
            return;
        }
        match &n.body {
            Body::Map(items) => {
                self.pod_parts(n);
                for (_, v) in items {
                    self.scan(v, depth + 1);
                }
            }
            Body::Seq(items) => {
                for item in items {
                    self.scan(item, depth + 1);
                }
            }
            _ => {}
        }
    }

    /// A mapping that may be a pod spec: volumes, containers, service account.
    fn pod_parts(&mut self, m: &YNode) {
        if let Some(volumes) = m.get("volumes") {
            for v in volumes.items() {
                self.volume(v);
            }
        }
        for key in CONTAINER_KEYS {
            if let Some(containers) = m.get(key) {
                for c in containers.items() {
                    self.container(c);
                }
            }
        }
        self.add(
            "ServiceAccount",
            m.get("serviceAccountName"),
            "serviceAccountName",
        );
    }

    fn volume(&mut self, v: &YNode) {
        self.add(
            "ConfigMap",
            v.get("configMap").and_then(|c| c.get("name")),
            "volume.configMap",
        );
        self.add(
            "Secret",
            v.get("secret").and_then(|s| s.get("secretName")),
            "volume.secret",
        );
        self.add(
            "PersistentVolumeClaim",
            v.get("persistentVolumeClaim")
                .and_then(|p| p.get("claimName")),
            "volume.persistentVolumeClaim",
        );
        if let Some(projected) = v.get("projected").and_then(|p| p.get("sources")) {
            for source in projected.items() {
                self.add(
                    "ConfigMap",
                    source.get("configMap").and_then(|c| c.get("name")),
                    "volume.projected.configMap",
                );
                self.add(
                    "Secret",
                    source.get("secret").and_then(|s| s.get("name")),
                    "volume.projected.secret",
                );
            }
        }
    }

    fn container(&mut self, c: &YNode) {
        if let Some(env_from) = c.get("envFrom") {
            for entry in env_from.items() {
                self.add(
                    "ConfigMap",
                    entry.get("configMapRef").and_then(|r| r.get("name")),
                    "envFrom.configMapRef",
                );
                self.add(
                    "Secret",
                    entry.get("secretRef").and_then(|r| r.get("name")),
                    "envFrom.secretRef",
                );
            }
        }
        if let Some(env) = c.get("env") {
            for entry in env.items() {
                let Some(from) = entry.get("valueFrom") else {
                    continue;
                };
                self.add(
                    "ConfigMap",
                    from.get("configMapKeyRef").and_then(|r| r.get("name")),
                    "env.configMapKeyRef",
                );
                self.add(
                    "Secret",
                    from.get("secretKeyRef").and_then(|r| r.get("name")),
                    "env.secretKeyRef",
                );
            }
        }
    }

    fn ingress(&mut self, spec: &YNode) {
        let backend = |this: &mut Self, b: &YNode| {
            this.add(
                "Service",
                b.get("service").and_then(|s| s.get("name")),
                "ingress.backend",
            );
            this.add("Service", b.get("serviceName"), "ingress.backend");
        };
        if let Some(default) = spec.get("defaultBackend").or_else(|| spec.get("backend")) {
            backend(self, default);
        }
        if let Some(rules) = spec.get("rules") {
            for rule in rules.items() {
                let Some(paths) = rule.get("http").and_then(|h| h.get("paths")) else {
                    continue;
                };
                for path in paths.items() {
                    if let Some(b) = path.get("backend") {
                        backend(self, b);
                    }
                }
            }
        }
    }

    fn route(&mut self, spec: &YNode) {
        let Some(rules) = spec.get("rules") else {
            return;
        };
        for rule in rules.items() {
            let Some(refs) = rule.get("backendRefs") else {
                continue;
            };
            for r in refs.items() {
                let kind = r
                    .get("kind")
                    .and_then(YNode::scalar)
                    .map(str::trim)
                    .filter(|k| !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric()))
                    .unwrap_or("Service");
                self.add(kind, r.get("name"), "route.backendRef");
            }
        }
    }

    fn scale_target(&mut self, spec: &YNode) {
        let Some(target) = spec.get("scaleTargetRef") else {
            return;
        };
        let Some(kind) = target.get("kind").and_then(YNode::scalar).map(str::trim) else {
            return;
        };
        if kind.is_empty() || !kind.chars().all(|c| c.is_ascii_alphanumeric()) {
            return;
        }
        self.add(kind, target.get("name"), "scaleTargetRef");
    }
}

/// Shortest run of `scan::MASK_FILL` that [`crate::scan::mask`] produces (`{{.}}`).
const FILL_RUN: usize = 5;

/// True when a masked template expression produced part of `text`: such a value is not a
/// literal name and must not become a reference.
fn looks_like_filler(text: &str) -> bool {
    fill_runs(text).next().is_some()
}

fn fill_runs(text: &str) -> impl Iterator<Item = (usize, usize)> + '_ {
    let bytes = text.as_bytes();
    let mut i = 0;
    std::iter::from_fn(move || {
        while i < bytes.len() {
            if bytes[i] == crate::scan::MASK_FILL {
                let start = i;
                while i < bytes.len() && bytes[i] == crate::scan::MASK_FILL {
                    i += 1;
                }
                if i - start >= FILL_RUN {
                    return Some((start, i));
                }
            } else {
                i += 1;
            }
        }
        None
    })
}

/// `text` with every filler run shown as `*` (a templated name keeps its literal parts).
fn unmask(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for (start, end) in fill_runs(text) {
        out.push_str(&text[last..start]);
        out.push('*');
        last = end;
    }
    out.push_str(&text[last..]);
    out
}

fn k8s_refs(em: &mut Emit<'_>, root: &YNode, resource: &Resource) {
    let mut refs = Refs {
        em,
        source: resource.index,
        namespace: resource.namespace.as_deref(),
    };
    refs.scan(root, 0);
    let Some(spec) = root.get("spec") else { return };
    refs.add("Service", spec.get("serviceName"), "serviceName");
    match resource.kind.as_str() {
        "Ingress" => refs.ingress(spec),
        "HTTPRoute" | "GRPCRoute" => refs.route(spec),
        "HorizontalPodAutoscaler" => refs.scale_target(spec),
        _ => {}
    }
}

// --- kustomization ---------------------------------------------------------------------------

/// A path-like entry of a kustomization (not a URL, not a remote base, not inline YAML).
fn kustomize_path(entry: &str) -> Option<&str> {
    let entry = entry.trim();
    if entry.is_empty()
        || entry.contains('\n')
        || entry.contains("://")
        || entry.starts_with("git@")
        || entry.starts_with(['{', '-', '$'])
    {
        return None;
    }
    if let Some((first, _)) = entry.split_once('/')
        && !first.starts_with('.')
        && first.contains('.')
    {
        return None;
    }
    Some(entry)
}

fn kustomize_refs(em: &mut Emit<'_>, items: &[(YNode, YNode)], sections: &HashMap<String, u32>) {
    for (key, value) in items {
        let Some(name) = key.scalar() else { continue };
        let source = sections.get(name).copied().unwrap_or(0);
        let add = |em: &mut Emit<'_>, node: &YNode| {
            if let Some(path) = node.scalar().and_then(kustomize_path) {
                em.edge(
                    Edge::to_ref(
                        EdgeKind::References,
                        source,
                        RefScheme::File,
                        path,
                        node.line,
                    )
                    .extra("via", name),
                );
            }
        };
        match name {
            "resources"
            | "bases"
            | "components"
            | "crds"
            | "generators"
            | "transformers"
            | "patchesStrategicMerge" => {
                for entry in value.items() {
                    add(em, entry);
                }
            }
            "patches" | "patchesJson6902" => {
                for entry in value.items() {
                    match &entry.body {
                        Body::Scalar { .. } => add(em, entry),
                        _ => {
                            if let Some(path) = entry.get("path") {
                                add(em, path);
                            }
                        }
                    }
                }
            }
            "configMapGenerator" | "secretGenerator" => {
                for generator in value.items() {
                    for list in ["files", "envs"] {
                        let Some(entries) = generator.get(list) else {
                            continue;
                        };
                        for entry in entries.items() {
                            let Some(text) = entry.scalar() else { continue };
                            let path = text.split_once('=').map_or(text, |(_, p)| p);
                            if let Some(path) = kustomize_path(path) {
                                em.edge(
                                    Edge::to_ref(
                                        EdgeKind::References,
                                        source,
                                        RefScheme::File,
                                        path,
                                        entry.line,
                                    )
                                    .extra("via", name),
                                );
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templated_marks_every_node_and_keeps_earlier_nodes() {
        let mut out = ParsedFile::new(Language::GoTemplate, "chart/templates/x.yaml", "");
        out.add_node(Node::new(NodeKind::Template, "t", 0, 1, 1));
        extract(
            "replicas: 2\nname: xxxxxxxx\n",
            "chart/templates/x.yaml",
            true,
            &mut out,
        );
        assert!(out.nodes[1].extra.get("templated").is_none());
        for n in &out.nodes[2..] {
            assert_eq!(
                n.extra.get("templated"),
                Some(&Value::Bool(true)),
                "{}",
                n.name
            );
        }
        assert_eq!(out.nodes.len(), 4);
        assert_eq!(out.nodes[2].parent, Some(0));
    }

    #[test]
    fn templated_filler_names_do_not_become_references() {
        let text = "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: app\nspec:\n  template:\n    spec:\n      serviceAccountName: xxxxxxxxxx\n      volumes:\n        - configMap:\n            name: real\n";
        let mut out = ParsedFile::new(Language::GoTemplate, "t.yaml", text);
        extract(text, "t.yaml", true, &mut out);
        let refs: Vec<String> = out
            .edges
            .iter()
            .filter_map(|e| match &e.target {
                rosetta_model::Target::Ref(r) => Some(r.to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(refs, ["k8s:ConfigMap/real"]);
    }

    #[test]
    fn anchor_name_reading() {
        assert_eq!(read_anchor_name("&a value", 0).as_deref(), Some("a"));
        assert_eq!(read_anchor_name("!!str &t2 bar", 0).as_deref(), Some("t2"));
        assert_eq!(read_anchor_name("&sq [a, b]", 0).as_deref(), Some("sq"));
        assert_eq!(read_anchor_name("plain", 0), None);
        assert_eq!(read_anchor_name("&", 0), None);
    }
}
