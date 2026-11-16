//! Python parser (owner P1). Contract: docs/design.md section 4, "Per-language output".
//!
//! Two passes over the tree-sitter tree: the first creates the definition nodes, so a call can
//! resolve to a definition that appears later in the file; the second emits imports, inheritance
//! and calls. Nothing is resolved outside the file: such targets are `py` references.
use rosetta_model::{Edge, EdgeKind, Language, Node, NodeKind, ParsedFile, RefScheme, cut};
use std::{
    cell::RefCell,
    collections::HashMap,
    time::{Duration, Instant},
};
use tree_sitter::{Node as TsNode, ParseOptions, Parser, Point};

/// Statement nesting (blocks, control flow, definitions) below which nothing is looked at.
const MAX_STATEMENT_DEPTH: usize = 400;
const PARSE_TIME_LIMIT: Duration = Duration::from_secs(10);

thread_local! {
    static PARSER: RefCell<Option<Parser>> = const { RefCell::new(None) };
}

/// Parses one file. Must be total: never panic, bound recursion, stay fast on 1 MiB input.
pub(crate) fn parse(path: &str, text: &str) -> ParsedFile {
    let mut parsed = ParsedFile::new(Language::Python, path, text);
    let Some(tree) = parse_tree(text) else {
        parsed.set_error("syntax tree could not be built");
        return parsed;
    };
    let root = tree.root_node();
    if root.has_error() {
        match first_error(root) {
            Some(line) => parsed.set_error(format!("syntax error near line {line}")),
            None => parsed.set_error("syntax error"),
        }
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    let file_is_test = name.starts_with("test_") || name.ends_with("_test.py");
    parsed.file_mut().is_test = file_is_test;

    let mut walker = Walker {
        src: text.as_bytes(),
        out: &mut parsed,
        frames: vec![Frame::module()],
        defs: HashMap::new(),
        ids: HashMap::new(),
        qnames: HashMap::new(),
        bases: HashMap::new(),
        file_is_test,
        phase: Phase::Definitions,
        too_deep: false,
    };
    walker.statement(root, 0);
    walker.phase = Phase::References;
    walker.frames = vec![Frame::module()];
    walker.frames[0].imports = scope_imports(root, walker.src);
    walker.statement(root, 0);
    if walker.too_deep {
        parsed.set_error("statements are nested too deeply");
    }
    parsed
}

fn parse_tree(text: &str) -> Option<tree_sitter::Tree> {
    PARSER.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            let mut parser = Parser::new();
            parser
                .set_language(&tree_sitter_python::LANGUAGE.into())
                .ok()?;
            *slot = Some(parser);
        }
        let parser = slot.as_mut()?;
        let deadline = Instant::now() + PARSE_TIME_LIMIT;
        let mut halt = |_: &tree_sitter::ParseState| Instant::now() > deadline;
        let bytes = text.as_bytes();
        let mut input = |offset: usize, _: Point| &bytes[offset.min(bytes.len())..];
        parser.parse_with_options(
            &mut input,
            None,
            Some(ParseOptions::new().progress_callback(&mut halt)),
        )
    })
}

/// 1-based line of the first error or missing node, found without recursion.
fn first_error(root: TsNode) -> Option<usize> {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.is_error() || node.is_missing() {
            return Some(node.start_position().row + 1);
        }
        if node.has_error() {
            let mut cursor = node.walk();
            let children: Vec<TsNode> = node.children(&mut cursor).collect();
            stack.extend(children.into_iter().rev());
        }
    }
    None
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Definitions,
    References,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FrameKind {
    Module,
    Class,
    Function,
}

/// One lexical scope: the module, a class body or a function body.
struct Frame {
    kind: FrameKind,
    /// Graph node of the scope (0 for the module).
    node: u32,
    /// Logical dotted name of the scope (`""` for the module); definitions are `prefix.name`.
    prefix: String,
    /// Import aliases visible in this scope: local name -> dotted target.
    imports: HashMap<String, String>,
}

impl Frame {
    fn module() -> Self {
        Self {
            kind: FrameKind::Module,
            node: 0,
            prefix: String::new(),
            imports: HashMap::new(),
        }
    }
}

struct Walker<'a> {
    src: &'a [u8],
    out: &'a mut ParsedFile,
    frames: Vec<Frame>,
    /// Logical qualified name -> graph node (the first definition wins).
    defs: HashMap<String, u32>,
    /// Tree-sitter node id of a definition -> graph node, filled by the first pass.
    ids: HashMap<usize, u32>,
    /// Graph node -> logical qualified name.
    qnames: HashMap<u32, String>,
    /// Class qualified name -> qualified names of its base classes defined in this file.
    bases: HashMap<String, Vec<String>>,
    file_is_test: bool,
    phase: Phase,
    too_deep: bool,
}

enum Resolved {
    Local(u32),
    /// A `py` reference; `true` when it was derived from an import of this file (the linker
    /// resolves only those, see docs/design.md).
    Ref(String, bool),
}

impl Walker<'_> {
    fn text(&self, node: TsNode) -> &str {
        node.utf8_text(self.src).unwrap_or("")
    }

    fn statement(&mut self, node: TsNode, depth: usize) {
        if depth > MAX_STATEMENT_DEPTH {
            self.too_deep = true;
            return;
        }
        match node.kind() {
            "function_definition" | "class_definition" => self.definition(node, node, depth),
            "decorated_definition" => self.decorated(node, depth),
            "import_statement" | "import_from_statement" => {
                if self.phase == Phase::References {
                    self.import_edges(node);
                }
            }
            "module"
            | "block"
            | "ERROR"
            | "if_statement"
            | "elif_clause"
            | "else_clause"
            | "for_statement"
            | "while_statement"
            | "try_statement"
            | "except_clause"
            | "except_group_clause"
            | "finally_clause"
            | "with_statement"
            | "match_statement"
            | "case_clause" => {
                let mut cursor = node.walk();
                let children: Vec<TsNode> = node.named_children(&mut cursor).collect();
                for child in children {
                    self.statement(child, depth + 1);
                }
            }
            "comment" => {}
            _ => {
                if self.phase == Phase::References {
                    self.expression(node);
                }
            }
        }
    }

    fn decorated(&mut self, node: TsNode, depth: usize) {
        let mut cursor = node.walk();
        let children: Vec<TsNode> = node.named_children(&mut cursor).collect();
        for child in children {
            match child.kind() {
                "function_definition" | "class_definition" => {
                    self.definition(child, node, depth + 1);
                }
                _ => {}
            }
        }
    }

    /// `def` is the function or class node; `outer` is its decorated wrapper, or `def` itself.
    fn definition(&mut self, def: TsNode, outer: TsNode, depth: usize) {
        let Some(name_node) = def.child_by_field_name("name") else {
            self.walk_body(def, depth);
            return;
        };
        let name = self.text(name_node).to_owned();
        if name.is_empty() {
            self.walk_body(def, depth);
            return;
        }
        let is_class = def.kind() == "class_definition";
        let top = self.frames.len() - 1;
        let qname = if self.frames[top].prefix.is_empty() {
            name.clone()
        } else {
            format!("{}.{name}", self.frames[top].prefix)
        };
        let index = match self.phase {
            Phase::Definitions => {
                let index = self.create(def, outer, &name, &qname, is_class);
                self.ids.insert(def.id(), index);
                self.qnames.insert(index, qname.clone());
                self.defs.entry(qname.clone()).or_insert(index);
                index
            }
            Phase::References => {
                let Some(index) = self.ids.get(&def.id()).copied() else {
                    return;
                };
                self.references_of_header(def, outer, index);
                index
            }
        };
        let mut frame = Frame {
            kind: if is_class {
                FrameKind::Class
            } else {
                FrameKind::Function
            },
            node: index,
            prefix: qname,
            imports: HashMap::new(),
        };
        if self.phase == Phase::References
            && let Some(body) = def.child_by_field_name("body")
        {
            frame.imports = scope_imports(body, self.src);
        }
        self.frames.push(frame);
        self.walk_body(def, depth);
        self.frames.pop();
    }

    fn walk_body(&mut self, def: TsNode, depth: usize) {
        if let Some(body) = def.child_by_field_name("body") {
            self.statement(body, depth + 1);
        }
    }

    fn create(
        &mut self,
        def: TsNode,
        outer: TsNode,
        name: &str,
        qname: &str,
        is_class: bool,
    ) -> u32 {
        let top = self.frames.len() - 1;
        let parent = self.frames[top].node;
        let kind = if is_class {
            NodeKind::Class
        } else if self.frames[top].kind == FrameKind::Class {
            NodeKind::Method
        } else {
            NodeKind::Function
        };
        let line_start = line(outer.start_position());
        let line_end = line(outer.end_position());
        let is_test = self.file_is_test || (!is_class && name.starts_with("test_"));
        let mut node = Node::new(kind, qname, parent, line_start, line_end).test(is_test);
        let signature_field = if is_class {
            "superclasses"
        } else {
            "parameters"
        };
        if let Some(sig) = def.child_by_field_name(signature_field) {
            node = node.signature(strip_parens(self.text(sig)));
        }
        let decorators = self.decorators(outer);
        if !decorators.is_empty() {
            node = node.extra("decorators", decorators);
        }
        if !is_class {
            if is_async(def) {
                node = node.extra("async", true);
            }
            if let Some(returns) = def.child_by_field_name("return_type") {
                node = node.extra("return_type", collapse(self.text(returns)));
            }
        }
        self.out.add_node(node)
    }

    fn decorators(&self, outer: TsNode) -> Vec<String> {
        if outer.kind() != "decorated_definition" {
            return Vec::new();
        }
        let mut cursor = outer.walk();
        outer
            .named_children(&mut cursor)
            .filter(|c| c.kind() == "decorator")
            .map(|d| {
                let text = self.text(d).trim_start_matches('@');
                let name = text.split('(').next().unwrap_or(text);
                collapse(name)
            })
            .filter(|d| !d.is_empty())
            .collect()
    }

    /// Edges that belong to a definition's header: decorators, base classes, default values and
    /// annotations are evaluated in the enclosing scope.
    fn references_of_header(&mut self, def: TsNode, outer: TsNode, index: u32) {
        if outer.kind() == "decorated_definition" {
            let mut cursor = outer.walk();
            let decorators: Vec<TsNode> = outer
                .named_children(&mut cursor)
                .filter(|c| c.kind() == "decorator")
                .collect();
            for decorator in decorators {
                self.expression(decorator);
            }
        }
        if def.kind() == "class_definition" {
            if let Some(bases) = def.child_by_field_name("superclasses") {
                self.inherits(bases, index);
                self.expression(bases);
            }
        } else {
            for field in ["parameters", "return_type"] {
                if let Some(part) = def.child_by_field_name(field) {
                    self.expression(part);
                }
            }
        }
    }

    fn inherits(&mut self, bases: TsNode, class: u32) {
        let mut cursor = bases.walk();
        let children: Vec<TsNode> = bases.named_children(&mut cursor).collect();
        for base in children {
            let mut base = base;
            if base.kind() == "subscript"
                && let Some(value) = base.child_by_field_name("value")
            {
                base = value;
            }
            if !matches!(base.kind(), "identifier" | "attribute") {
                continue;
            }
            let Some(raw) = dotted_name(base, self.src) else {
                continue;
            };
            let line = line(base.start_position());
            match self.resolve(&raw) {
                Some(Resolved::Local(target)) if target != class => {
                    if let (Some(class_name), Some(base_name)) =
                        (self.qnames.get(&class), self.qnames.get(&target))
                    {
                        self.bases
                            .entry(class_name.clone())
                            .or_default()
                            .push(base_name.clone());
                    }
                    self.out
                        .add_edge(Edge::local(EdgeKind::Inherits, class, target, line));
                }
                Some(Resolved::Ref(value, via_import)) => {
                    self.out.add_edge(with_via(
                        Edge::to_ref(EdgeKind::Inherits, class, RefScheme::Py, value, line),
                        via_import,
                    ));
                }
                _ => {}
            }
        }
    }

    /// The graph node that owns code running in the current scope: the innermost function, or the
    /// File node for module and class level code.
    fn call_source(&self) -> u32 {
        self.frames
            .iter()
            .rev()
            .find(|f| f.kind == FrameKind::Function)
            .map_or(0, |f| f.node)
    }

    /// Emits a CALLS edge for every call in the expression tree of `node` (iteratively).
    fn expression(&mut self, node: TsNode) {
        let source = self.call_source();
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            if current.kind() == "call"
                && let Some(callee) = current.child_by_field_name("function")
                && let Some(raw) = dotted_name(callee, self.src)
            {
                let line = line(
                    callee
                        .child_by_field_name("attribute")
                        .unwrap_or(callee)
                        .start_position(),
                );
                match self.resolve(&raw) {
                    Some(Resolved::Local(target)) => {
                        self.out
                            .add_edge(Edge::local(EdgeKind::Calls, source, target, line));
                    }
                    Some(Resolved::Ref(value, via_import)) => {
                        self.out.add_edge(with_via(
                            Edge::to_ref(EdgeKind::Calls, source, RefScheme::Py, value, line),
                            via_import,
                        ));
                    }
                    None => {}
                }
            }
            let mut cursor = current.walk();
            let children: Vec<TsNode> = current.named_children(&mut cursor).collect();
            stack.extend(children.into_iter().rev());
        }
    }

    /// Visible names, innermost scope first. A class body is visible only from itself, never from
    /// the functions nested in it.
    fn visible_frames(&self) -> impl Iterator<Item = &Frame> {
        let top = self.frames.len() - 1;
        self.frames
            .iter()
            .enumerate()
            .rev()
            .filter(move |(i, f)| f.kind != FrameKind::Class || *i == top)
            .map(|(_, f)| f)
    }

    fn local_definition(&self, name: &str) -> Option<(String, u32)> {
        self.visible_frames().find_map(|frame| {
            let qname = if frame.prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{}.{name}", frame.prefix)
            };
            self.defs.get(&qname).map(|index| (qname, *index))
        })
    }

    fn alias(&self, name: &str) -> Option<&str> {
        self.visible_frames()
            .find_map(|f| f.imports.get(name))
            .map(String::as_str)
    }

    /// `None` means "no edge": a builtin that nothing in the file redefines.
    fn resolve(&self, raw: &str) -> Option<Resolved> {
        let Some((head, rest)) = raw.split_once('.') else {
            if let Some((_, index)) = self.local_definition(raw) {
                return Some(Resolved::Local(index));
            }
            if let Some(target) = self.alias(raw) {
                return Some(Resolved::Ref(target.to_owned(), true));
            }
            return if is_builtin(raw) {
                None
            } else {
                Some(Resolved::Ref(raw.to_owned(), false))
            };
        };
        if matches!(head, "self" | "cls")
            && !rest.contains('.')
            && let Some(class) = self
                .frames
                .iter()
                .rev()
                .find(|f| f.kind == FrameKind::Class)
            && let Some(index) = self.method_of(&class.prefix, rest)
        {
            return Some(Resolved::Local(index));
        }
        if let Some((qname, _)) = self.local_definition(head) {
            if let Some(index) = self.defs.get(&format!("{qname}.{rest}")) {
                return Some(Resolved::Local(*index));
            }
            return Some(Resolved::Ref(raw.to_owned(), false));
        }
        if let Some(target) = self.alias(head) {
            return Some(Resolved::Ref(format!("{target}.{rest}"), true));
        }
        Some(Resolved::Ref(raw.to_owned(), false))
    }

    /// The method `name` of `class` or of a base class defined in this file.
    fn method_of(&self, class: &str, name: &str) -> Option<u32> {
        let mut queue = vec![class.to_owned()];
        let mut next = 0;
        while let Some(current) = queue.get(next).cloned() {
            next += 1;
            if next > 64 {
                break;
            }
            if let Some(index) = self.defs.get(&format!("{current}.{name}")) {
                return Some(*index);
            }
            if let Some(parents) = self.bases.get(&current) {
                for parent in parents {
                    if !queue.contains(parent) {
                        queue.push(parent.clone());
                    }
                }
            }
        }
        None
    }

    fn import_edges(&mut self, node: TsNode) {
        let source = self.frames[self.frames.len() - 1].node;
        let line = line(node.start_position());
        for item in imports(node, self.src) {
            let mut edge =
                Edge::to_ref(EdgeKind::Imports, source, RefScheme::Py, item.target, line)
                    .extra("import_type", item.kind)
                    .extra("via", "import");
            if let Some(module) = item.module {
                edge = edge.extra("module", module);
            }
            self.out.add_edge(edge);
        }
    }
}

struct Import {
    /// Dotted target as written, relative dots kept.
    target: String,
    /// Name bound in the scope, when the import binds one that differs from the target.
    alias: Option<(String, String)>,
    kind: &'static str,
    module: Option<String>,
}

/// The imports of one statement.
fn imports(node: TsNode, src: &[u8]) -> Vec<Import> {
    let text = |n: TsNode| collapse(n.utf8_text(src).unwrap_or(""));
    let mut out = Vec::new();
    let mut cursor = node.walk();
    let children: Vec<TsNode> = node.named_children(&mut cursor).collect();
    if node.kind() == "import_statement" {
        for child in children {
            match child.kind() {
                "dotted_name" => {
                    let target = text(child);
                    // `import a.b.c` binds `a`, through which `a.b.c.f()` is reached.
                    let head = target.split('.').next().unwrap_or(&target).to_owned();
                    out.push(Import {
                        alias: Some((head.clone(), head)),
                        target,
                        kind: "module",
                        module: None,
                    });
                }
                "aliased_import" => {
                    let (Some(name), Some(alias)) = (
                        child.child_by_field_name("name"),
                        child.child_by_field_name("alias"),
                    ) else {
                        continue;
                    };
                    let target = text(name);
                    out.push(Import {
                        alias: Some((text(alias), target.clone())),
                        target,
                        kind: "module",
                        module: None,
                    });
                }
                _ => {}
            }
        }
        return out;
    }
    let Some(module_node) = node.child_by_field_name("module_name") else {
        return out;
    };
    let module = text(module_node).replace(' ', "");
    if module == "__future__" {
        return out;
    }
    let join = |name: &str| {
        if module.ends_with('.') {
            format!("{module}{name}")
        } else {
            format!("{module}.{name}")
        }
    };
    for child in children {
        if child.id() == module_node.id() {
            continue;
        }
        match child.kind() {
            "wildcard_import" => out.push(Import {
                target: module.clone(),
                alias: None,
                kind: "from",
                module: Some(module.clone()),
            }),
            "dotted_name" => {
                let name = text(child);
                let target = join(&name);
                out.push(Import {
                    alias: Some((name, target.clone())),
                    target,
                    kind: "from",
                    module: Some(module.clone()),
                });
            }
            "aliased_import" => {
                let (Some(name), Some(alias)) = (
                    child.child_by_field_name("name"),
                    child.child_by_field_name("alias"),
                ) else {
                    continue;
                };
                let target = join(&text(name));
                out.push(Import {
                    alias: Some((text(alias), target.clone())),
                    target,
                    kind: "from",
                    module: Some(module.clone()),
                });
            }
            _ => {}
        }
    }
    out
}

/// Alias map of one scope: imports found in its own statements (control flow included), not in
/// the definitions nested in it.
fn scope_imports(scope: TsNode, src: &[u8]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut stack = vec![scope];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "import_statement" | "import_from_statement" => {
                for item in imports(node, src) {
                    if let Some((name, target)) = item.alias {
                        map.insert(name, target);
                    }
                }
            }
            "module"
            | "block"
            | "ERROR"
            | "if_statement"
            | "elif_clause"
            | "else_clause"
            | "for_statement"
            | "while_statement"
            | "try_statement"
            | "except_clause"
            | "except_group_clause"
            | "finally_clause"
            | "with_statement"
            | "match_statement"
            | "case_clause" => {
                let mut cursor = node.walk();
                let children: Vec<TsNode> = node.named_children(&mut cursor).collect();
                stack.extend(children.into_iter().rev());
            }
            _ => {}
        }
    }
    map
}

/// `a.b.c` for an identifier or an attribute chain of identifiers; `None` for anything else
/// (calls, subscripts, literals as the object).
fn dotted_name(node: TsNode, src: &[u8]) -> Option<String> {
    let mut parts = Vec::new();
    let mut current = node;
    loop {
        match current.kind() {
            "identifier" => {
                parts.push(current.utf8_text(src).ok()?);
                break;
            }
            "attribute" => {
                parts.push(
                    current
                        .child_by_field_name("attribute")?
                        .utf8_text(src)
                        .ok()?,
                );
                current = current.child_by_field_name("object")?;
            }
            _ => return None,
        }
    }
    parts.reverse();
    Some(parts.join("."))
}

/// Marks a `py` edge whose target came from an import of the file.
fn with_via(edge: Edge, via_import: bool) -> Edge {
    if via_import {
        edge.extra("via", "import")
    } else {
        edge
    }
}

fn is_async(def: TsNode) -> bool {
    let mut cursor = def.walk();
    def.children(&mut cursor)
        .take(2)
        .any(|c| c.kind() == "async")
}

fn line(point: Point) -> u32 {
    u32::try_from(point.row + 1).unwrap_or(u32::MAX)
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn strip_parens(text: &str) -> String {
    let text = text.trim();
    let text = text.strip_prefix('(').unwrap_or(text);
    let text = text.strip_suffix(')').unwrap_or(text);
    cut(&collapse(text))
}

/// Names that need no edge: Python builtins that the file does not define or import itself.
fn is_builtin(name: &str) -> bool {
    BUILTINS.binary_search(&name).is_ok()
}

const BUILTINS: &[&str] = &[
    "ArithmeticError",
    "AssertionError",
    "AttributeError",
    "BaseException",
    "BaseExceptionGroup",
    "BlockingIOError",
    "BrokenPipeError",
    "BufferError",
    "BytesWarning",
    "ChildProcessError",
    "ConnectionAbortedError",
    "ConnectionError",
    "ConnectionRefusedError",
    "ConnectionResetError",
    "DeprecationWarning",
    "EOFError",
    "Exception",
    "ExceptionGroup",
    "FileExistsError",
    "FileNotFoundError",
    "FloatingPointError",
    "FutureWarning",
    "GeneratorExit",
    "IOError",
    "ImportError",
    "ImportWarning",
    "IndentationError",
    "IndexError",
    "InterruptedError",
    "IsADirectoryError",
    "KeyError",
    "KeyboardInterrupt",
    "LookupError",
    "MemoryError",
    "ModuleNotFoundError",
    "NameError",
    "NotADirectoryError",
    "NotImplementedError",
    "OSError",
    "OverflowError",
    "PendingDeprecationWarning",
    "PermissionError",
    "ProcessLookupError",
    "RecursionError",
    "ReferenceError",
    "ResourceWarning",
    "RuntimeError",
    "RuntimeWarning",
    "StopAsyncIteration",
    "StopIteration",
    "SyntaxError",
    "SyntaxWarning",
    "SystemError",
    "SystemExit",
    "TabError",
    "TimeoutError",
    "TypeError",
    "UnboundLocalError",
    "UnicodeDecodeError",
    "UnicodeEncodeError",
    "UnicodeError",
    "UnicodeTranslateError",
    "UnicodeWarning",
    "UserWarning",
    "ValueError",
    "Warning",
    "ZeroDivisionError",
    "abs",
    "aiter",
    "all",
    "anext",
    "any",
    "ascii",
    "bin",
    "bool",
    "breakpoint",
    "bytearray",
    "bytes",
    "callable",
    "chr",
    "classmethod",
    "compile",
    "complex",
    "delattr",
    "dict",
    "dir",
    "divmod",
    "enumerate",
    "eval",
    "exec",
    "filter",
    "float",
    "format",
    "frozenset",
    "getattr",
    "globals",
    "hasattr",
    "hash",
    "help",
    "hex",
    "id",
    "input",
    "int",
    "isinstance",
    "issubclass",
    "iter",
    "len",
    "list",
    "locals",
    "map",
    "max",
    "memoryview",
    "min",
    "next",
    "object",
    "oct",
    "open",
    "ord",
    "pow",
    "print",
    "property",
    "range",
    "repr",
    "reversed",
    "round",
    "set",
    "setattr",
    "slice",
    "sorted",
    "staticmethod",
    "str",
    "sum",
    "super",
    "tuple",
    "type",
    "vars",
    "zip",
];

#[cfg(test)]
mod tests {
    use super::BUILTINS;

    #[test]
    fn builtin_table_is_sorted() {
        assert!(BUILTINS.windows(2).all(|w| w[0] < w[1]));
    }
}
