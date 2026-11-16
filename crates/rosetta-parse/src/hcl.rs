//! HCL / Terraform parser (owner P2). Contract: docs/design.md section 4, "HCL".
//!
//! Two passes over the hcl-edit tree: the first creates the nodes of the top-level blocks, the
//! second walks every expression (resource bodies, nested and dynamic blocks included) and emits
//! the edges, so a reference may point at a block that appears later in the file.
use hcl_edit::{
    Span,
    expr::{Expression, ForExpr, FuncCall, Object, ObjectKey, Traversal, TraversalOperator},
    structure::{Attribute, Block, BlockLabel, Body, Structure},
    template::{Element, Template},
    visit::{self, Visit},
};
use rosetta_model::{
    Edge, EdgeKind, Language, Node, NodeKind, ParsedFile, RefScheme, is_secret_key,
};
use std::ops::Range;

/// Deepest bracket nesting we hand to the parser; the parser and the walker are recursive.
const MAX_NESTING: usize = 120;
/// Module arguments that are not input variables.
const MODULE_META: [&str; 6] = [
    "source",
    "version",
    "providers",
    "depends_on",
    "count",
    "for_each",
];
/// Root names that look like `T.N` but are not resource references.
const RESERVED_ROOTS: [&str; 5] = ["path", "terraform", "each", "count", "self"];

/// Parses one file. Total: never panics, returns the File node alone on a syntax error.
pub(crate) fn parse(path: &str, text: &str) -> ParsedFile {
    if too_deep(text) {
        return ParsedFile::failed(Language::Hcl, path, text, "nesting too deep");
    }
    let body = match hcl_edit::parser::parse_body(text) {
        Ok(body) => body,
        Err(e) => {
            let loc = e.location();
            let message = format!(
                "HCL parse error at line {}, column {}: {}",
                loc.line(),
                loc.column(),
                e.message().lines().next().unwrap_or("")
            );
            return ParsedFile::failed(Language::Hcl, path, text, message);
        }
    };
    let src = Src::new(text);
    let mut out = ParsedFile::new(Language::Hcl, path, text);
    if path.ends_with(".tfvars") {
        tfvars(&mut out, &src, &body);
        return out;
    }
    let mut work = Vec::new();
    let mut modules = Vec::new();
    for structure in &body {
        if let Structure::Block(block) = structure {
            declare(&mut out, &src, block, &mut work, &mut modules);
        }
    }
    for item in &work {
        let (Work::Body(source, _) | Work::Attr(source, _)) = item;
        let mut walker = Walker {
            out: &mut out,
            src: &src,
            source: *source,
            bound: Vec::new(),
            cur_line: 0,
        };
        match item {
            Work::Body(_, body) => walker.visit_body(body),
            Work::Attr(_, attr) => walker.visit_attr(attr),
        }
    }
    for module in &modules {
        module_edges(&mut out, &src, module);
    }
    out
}

enum Work<'b> {
    Body(u32, &'b Body),
    Attr(u32, &'b Attribute),
}

struct ModuleInfo<'b> {
    node: u32,
    source: String,
    body: &'b Body,
}

/// Byte offsets to line numbers, plus source slices for `extra`.
struct Src<'a> {
    text: &'a str,
    starts: Vec<usize>,
}

impl<'a> Src<'a> {
    fn new(text: &'a str) -> Self {
        let mut starts = vec![0];
        starts.extend(
            text.bytes()
                .enumerate()
                .filter(|(_, b)| *b == b'\n')
                .map(|(i, _)| i + 1),
        );
        Self { text, starts }
    }

    fn line_at(&self, offset: usize) -> u32 {
        u32::try_from(self.starts.partition_point(|s| *s <= offset)).unwrap_or(u32::MAX)
    }

    /// First line of a span, 0 when unknown.
    fn start_line(&self, span: Option<Range<usize>>) -> u32 {
        span.map_or(0, |r| self.line_at(r.start))
    }

    /// First and last line of a span; trailing whitespace of the span is not counted.
    fn lines(&self, span: Option<Range<usize>>) -> (u32, u32) {
        let Some(r) = span else { return (0, 0) };
        let bytes = self.text.as_bytes();
        let mut end = r.end.min(bytes.len());
        while end > r.start && bytes[end - 1].is_ascii_whitespace() {
            end -= 1;
        }
        (self.line_at(r.start), self.line_at(end.saturating_sub(1)))
    }

    /// Source text of a span with runs of whitespace collapsed (`extra` cuts it to 200 chars).
    fn slice(&self, span: Option<Range<usize>>) -> Option<String> {
        let r = span?;
        let s = self.text.get(r)?;
        Some(s.split_whitespace().collect::<Vec<_>>().join(" "))
    }
}

/// Rejects input that would recurse deeply in the parser or the walker: bracket nesting beyond
/// [`MAX_NESTING`]. Operator chains are parsed iteratively and need no guard (tested to 100k).
fn too_deep(text: &str) -> bool {
    let bytes = text.as_bytes();
    let (mut depth, mut in_string, mut i) = (0usize, false, 0usize);
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            match b {
                b'\\' => i += 1,
                b'"' | b'\n' => in_string = false,
                _ => {}
            }
        } else {
            match b {
                b'"' => in_string = true,
                b'#' => i = skip_line(bytes, i),
                b'/' if bytes.get(i + 1) == Some(&b'/') => i = skip_line(bytes, i),
                b'/' if bytes.get(i + 1) == Some(&b'*') => {
                    i = text[i + 2..]
                        .find("*/")
                        .map_or(bytes.len(), |p| i + 2 + p + 1);
                }
                b'(' | b'[' | b'{' => {
                    depth += 1;
                    if depth > MAX_NESTING {
                        return true;
                    }
                }
                b')' | b']' | b'}' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        i += 1;
    }
    false
}

fn skip_line(bytes: &[u8], from: usize) -> usize {
    bytes[from..]
        .iter()
        .position(|b| *b == b'\n')
        .map_or(bytes.len(), |p| from + p)
}

fn label_text(label: &BlockLabel) -> String {
    match label {
        BlockLabel::Ident(i) => i.to_string(),
        BlockLabel::String(s) => s.to_string(),
    }
}

/// A plain string literal.
fn literal(expr: &Expression) -> Option<String> {
    match expr {
        Expression::String(s) => Some(s.to_string()),
        _ => None,
    }
}

fn attr_expr<'b>(body: &'b Body, key: &str) -> Option<&'b Expression> {
    body.get_attribute(key).map(|a| &a.value)
}

fn attr_literal(body: &Body, key: &str) -> Option<String> {
    attr_expr(body, key).and_then(literal)
}

fn object_get<'b>(obj: &'b Object, name: &str) -> Option<&'b Expression> {
    obj.iter().find_map(|(key, value)| {
        let matches = match key {
            ObjectKey::Ident(i) => i.to_string() == name,
            ObjectKey::Expression(Expression::String(s)) => s.to_string() == name,
            ObjectKey::Expression(_) => false,
        };
        matches.then(|| value.expr())
    })
}

/// The state key of a backend configuration: `key`, `prefix` or `path`, when a literal.
fn state_key(get: impl Fn(&str) -> Option<String>) -> Option<String> {
    ["key", "prefix", "path"].iter().find_map(|k| get(k))
}

fn provider_of(resource_type: &str) -> &str {
    resource_type.split('_').next().unwrap_or(resource_type)
}

fn tfvars(out: &mut ParsedFile, src: &Src, body: &Body) {
    for structure in body {
        if let Structure::Attribute(attr) = structure {
            let (start, end) = src.lines(attr.span());
            let node = Node::new(NodeKind::Variable, attr.key.to_string(), 0, start, end)
                .extra("assigned", true);
            out.add_node(node);
        }
    }
}

fn declare<'b>(
    out: &mut ParsedFile,
    src: &Src,
    block: &'b Block,
    work: &mut Vec<Work<'b>>,
    modules: &mut Vec<ModuleInfo<'b>>,
) {
    let (start, end) = src.lines(block.span());
    let labels: Vec<String> = block.labels.iter().map(label_text).collect();
    let body = &block.body;
    let new = |kind: NodeKind, name: String| Node::new(kind, name, 0, start, end);
    match (block.ident.as_str(), labels.as_slice()) {
        ("resource", [ty, name, ..]) => {
            let node = new(NodeKind::Resource, format!("{ty}.{name}"))
                .extra("provider", provider_of(ty))
                .extra("resource_type", ty.as_str());
            work.push(Work::Body(out.add_node(node), body));
        }
        ("data", [ty, name, ..]) => {
            let mut node = new(NodeKind::DataSource, format!("{ty}.{name}"))
                .extra("provider", provider_of(ty))
                .extra("data_type", ty.as_str());
            if ty == "terraform_remote_state" {
                if let Some(backend) = attr_literal(body, "backend") {
                    node = node.extra("backend", backend);
                }
                let key = match attr_expr(body, "config") {
                    Some(Expression::Object(config)) => {
                        state_key(|k| object_get(config, k).and_then(literal))
                    }
                    _ => None,
                };
                if let Some(key) = key {
                    node = node.extra("backend_key", key);
                }
            }
            work.push(Work::Body(out.add_node(node), body));
        }
        ("module", [name, ..]) => {
            let mut node = new(NodeKind::Module, name.clone());
            let source = attr_expr(body, "source");
            if let Some(s) = source.and_then(|e| literal(e).or_else(|| src.slice(e.span()))) {
                node = node.extra("source", s);
            }
            if let Some(v) = attr_expr(body, "version").and_then(literal) {
                node = node.extra("version", v);
            }
            let index = out.add_node(node);
            work.push(Work::Body(index, body));
            if let Some(path) = source.and_then(literal)
                && (path.starts_with("./") || path.starts_with("../"))
            {
                modules.push(ModuleInfo {
                    node: index,
                    source: path,
                    body,
                });
            }
        }
        ("variable", [name, ..]) => {
            let mut node = new(NodeKind::Variable, name.clone());
            if let Some(t) = attr_expr(body, "type").and_then(|e| src.slice(e.span())) {
                node = node.extra("type", t);
            }
            let sensitive =
                matches!(attr_expr(body, "sensitive"), Some(Expression::Bool(b)) if **b);
            if !sensitive
                && !is_secret_key(name)
                && let Some(d) = attr_expr(body, "default")
                    .and_then(|e| literal(e).or_else(|| src.slice(e.span())))
            {
                node = node.extra("default", d);
            }
            if let Some(d) = description(src, body) {
                node = node.extra("description", d);
            }
            work.push(Work::Body(out.add_node(node), body));
        }
        ("output", [name, ..]) => {
            let mut node = new(NodeKind::Output, name.clone());
            if let Some(d) = description(src, body) {
                node = node.extra("description", d);
            }
            work.push(Work::Body(out.add_node(node), body));
        }
        ("provider", [name, ..]) => {
            let alias = attr_literal(body, "alias");
            let full = alias
                .as_ref()
                .map_or_else(|| name.clone(), |a| format!("{name}.{a}"));
            let mut node = new(NodeKind::Provider, full);
            if let Some(a) = alias {
                node = node.extra("alias", a);
            }
            if let Some(r) = attr_literal(body, "region") {
                node = node.extra("region", r);
            }
            work.push(Work::Body(out.add_node(node), body));
        }
        ("locals", _) => {
            for structure in body {
                if let Structure::Attribute(attr) = structure {
                    let (s, e) = src.lines(attr.span());
                    let mut node = Node::new(NodeKind::Local, attr.key.to_string(), 0, s, e);
                    if !is_secret_key(&attr.key)
                        && let Some(v) = src.slice(attr.value.span())
                    {
                        node = node.extra("value", v);
                    }
                    work.push(Work::Attr(out.add_node(node), attr));
                }
            }
        }
        ("terraform", _) => {
            let node = terraform_node(new(NodeKind::TerraformConfig, "terraform".into()), body);
            work.push(Work::Body(out.add_node(node), body));
        }
        _ => {}
    }
}

fn description(src: &Src, body: &Body) -> Option<String> {
    let e = attr_expr(body, "description")?;
    literal(e).or_else(|| src.slice(e.span()))
}

/// Backend type and state key, and the required providers with their sources.
fn terraform_node(mut node: Node, body: &Body) -> Node {
    let mut providers = serde_json::Map::new();
    for block in body.blocks() {
        match block.ident.as_str() {
            "backend" => {
                if let Some(kind) = block.labels.first().map(label_text) {
                    node = node.extra("backend", kind);
                }
                if let Some(key) = state_key(|k| attr_literal(&block.body, k)) {
                    node = node.extra("backend_key", key);
                }
            }
            "required_providers" => {
                for attr in block.body.attributes() {
                    let source = match &attr.value {
                        Expression::Object(o) => object_get(o, "source").and_then(literal),
                        _ => None,
                    };
                    providers.insert(attr.key.to_string(), source.unwrap_or_default().into());
                }
            }
            _ => {}
        }
    }
    if !providers.is_empty() {
        node = node.extra("required_providers", serde_json::Value::Object(providers));
    }
    node
}

fn module_edges(out: &mut ParsedFile, src: &Src, module: &ModuleInfo) {
    let line = module
        .body
        .get_attribute("source")
        .map_or(0, |a| src.start_line(a.span()));
    out.add_edge(Edge::to_ref(
        EdgeKind::UsesModule,
        module.node,
        RefScheme::TfModule,
        module.source.clone(),
        line,
    ));
    for attr in module.body.attributes() {
        let key = attr.key.to_string();
        if MODULE_META.contains(&key.as_str()) {
            continue;
        }
        out.add_edge(Edge::to_ref(
            EdgeKind::PassesVar,
            module.node,
            RefScheme::TfVar,
            format!("{}|{key}", module.source),
            src.start_line(attr.span()),
        ));
    }
}

/// Expression walker: emits REFERENCES, READS_CONFIG / RENDERS and REMOTE_STATE edges for
/// everything below one source node.
struct Walker<'a> {
    out: &'a mut ParsedFile,
    src: &'a Src<'a>,
    source: u32,
    /// Names bound by `for` expressions and `dynamic` blocks around the current expression.
    bound: Vec<String>,
    cur_line: u32,
}

impl Walker<'_> {
    fn line(&self, span: Option<Range<usize>>) -> u32 {
        span.map_or(self.cur_line, |r| self.src.line_at(r.start))
    }

    fn reference(&mut self, kind: NodeKind, name: &str, value: String, line: u32) {
        let edge = match self.out.find(kind, name) {
            Some(target) if target != self.source => {
                Edge::local(EdgeKind::References, self.source, target, line)
            }
            Some(_) => return,
            None => Edge::to_ref(
                EdgeKind::References,
                self.source,
                RefScheme::Tf,
                value,
                line,
            ),
        };
        self.out.add_edge(edge);
    }

    fn on_traversal(&mut self, t: &Traversal) {
        let Expression::Variable(root) = &t.expr else {
            return;
        };
        let root = root.to_string();
        if self.bound.contains(&root) || RESERVED_ROOTS.contains(&root.as_str()) {
            return;
        }
        let attrs: Vec<String> = t
            .operators
            .iter()
            .map_while(|op| match &**op {
                TraversalOperator::GetAttr(name) => Some(name.to_string()),
                _ => None,
            })
            .take(3)
            .collect();
        let line = self.line(t.span());
        match (root.as_str(), attrs.as_slice()) {
            ("var", [a, ..]) => self.reference(NodeKind::Variable, a, format!("var.{a}"), line),
            ("local", [a, ..]) => self.reference(NodeKind::Local, a, format!("local.{a}"), line),
            ("module", [a, ..]) => self.reference(NodeKind::Module, a, format!("module.{a}"), line),
            ("data", [ty, name, ..]) => {
                let key = format!("{ty}.{name}");
                self.reference(NodeKind::DataSource, &key, format!("data.{key}"), line);
                if ty == "terraform_remote_state" {
                    self.remote_state(t, name, line);
                }
            }
            ("data", _) => {}
            (ty, [name, ..]) if ty.contains('_') => {
                let key = format!("{ty}.{name}");
                self.reference(NodeKind::Resource, &key, key.clone(), line);
            }
            _ => {}
        }
    }

    /// `data.terraform_remote_state.<label>.outputs.<name>` with a literal backend key.
    fn remote_state(&mut self, t: &Traversal, label: &str, line: u32) {
        let outputs = matches!(
            t.operators.get(2).map(|op| &**op),
            Some(TraversalOperator::GetAttr(n)) if n.as_str() == "outputs"
        );
        if !outputs {
            return;
        }
        let name = match t.operators.get(3).map(|op| &**op) {
            Some(TraversalOperator::GetAttr(n)) => n.to_string(),
            Some(TraversalOperator::Index(e)) => match literal(e) {
                Some(s) => s,
                None => return,
            },
            _ => return,
        };
        let data = self.out.find(
            NodeKind::DataSource,
            &format!("terraform_remote_state.{label}"),
        );
        let key = data
            .and_then(|i| self.out.nodes.get(i as usize))
            .and_then(|n| n.extra.get("backend_key"))
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        if let Some(key) = key {
            let edge = Edge::to_ref(
                EdgeKind::RemoteState,
                self.source,
                RefScheme::TfState,
                format!("{key}|{name}"),
                line,
            )
            .extra("output", name);
            self.out.add_edge(edge);
        }
    }

    fn on_call(&mut self, call: &FuncCall) {
        let function = call.name.name.to_string();
        if call.name.is_namespaced()
            || !matches!(function.as_str(), "file" | "filebase64" | "templatefile")
        {
            return;
        }
        let Some(path) = call.args.get(0).and_then(path_literal) else {
            return;
        };
        let kind = if [".j2", ".tpl", ".tftpl"].iter().any(|e| path.ends_with(e)) {
            EdgeKind::Renders
        } else {
            EdgeKind::ReadsConfig
        };
        let line = self.line(call.span());
        let edge = Edge::to_ref(kind, self.source, RefScheme::File, path, line)
            .extra("function", function);
        self.out.add_edge(edge);
    }
}

/// A path written as a literal, optionally behind `${path.module}`, `${path.root}` or
/// `${path.cwd}`; any other interpolation makes it unknowable.
fn path_literal(expr: &Expression) -> Option<String> {
    let path = match expr {
        Expression::String(s) => s.to_string(),
        Expression::StringTemplate(t) => template_path(t)?,
        _ => return None,
    };
    let path = path.trim_start_matches('/').to_owned();
    (!path.is_empty()).then_some(path)
}

fn template_path(template: &Template) -> Option<String> {
    let mut out = String::new();
    for (i, element) in template.iter().enumerate() {
        match element {
            Element::Literal(s) => out.push_str(s),
            Element::Interpolation(interp) if i == 0 => match &interp.expr {
                Expression::Traversal(t) if is_path_prefix(t) => {}
                _ => return None,
            },
            _ => return None,
        }
    }
    Some(out)
}

fn is_path_prefix(t: &Traversal) -> bool {
    matches!(&t.expr, Expression::Variable(v) if v.as_str() == "path")
        && matches!(
            t.operators.first().map(|op| &**op),
            Some(TraversalOperator::GetAttr(n)) if matches!(n.as_str(), "module" | "root" | "cwd")
        )
        && t.operators.len() == 1
}

impl Visit for Walker<'_> {
    fn visit_attr(&mut self, node: &Attribute) {
        self.cur_line = self.src.start_line(node.span());
        visit::visit_attr(self, node);
    }

    fn visit_block(&mut self, node: &Block) {
        let dynamic = node.ident.as_str() == "dynamic";
        let before = self.bound.len();
        if dynamic {
            if let Some(label) = node.labels.first() {
                self.bound.push(label_text(label));
            }
            if let Some(Expression::Variable(iterator)) = attr_expr(&node.body, "iterator") {
                self.bound.push(iterator.to_string());
            }
        }
        visit::visit_block(self, node);
        self.bound.truncate(before);
    }

    fn visit_for_expr(&mut self, node: &ForExpr) {
        let before = self.bound.len();
        if let Some(key) = &node.intro.key_var {
            self.bound.push(key.to_string());
        }
        self.bound.push(node.intro.value_var.to_string());
        visit::visit_for_expr(self, node);
        self.bound.truncate(before);
    }

    fn visit_traversal(&mut self, node: &Traversal) {
        self.on_traversal(node);
        visit::visit_traversal(self, node);
    }

    fn visit_func_call(&mut self, node: &FuncCall) {
        self.on_call(node);
        visit::visit_func_call(self, node);
    }
}
