//! Bash parser (owner P5). Contract: docs/design.md section 4, "Per-language output".
use rosetta_model::{Edge, EdgeKind, Language, Node, NodeKind, ParsedFile, RefScheme};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tree_sitter::{Node as TsNode, ParseOptions, Parser, Point};

/// Longest a single file may take to parse; a pathological file yields its File node only.
const PARSE_TIME_LIMIT: Duration = Duration::from_secs(5);

/// Parses one file. Must be total: never panic, bound recursion, stay fast on 1 MiB input.
pub(crate) fn parse(path: &str, text: &str) -> ParsedFile {
    let mut out = ParsedFile::new(Language::Bash, path, text);
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .is_err()
    {
        out.set_error("bash grammar unavailable");
        return out;
    }
    let deadline = Instant::now() + PARSE_TIME_LIMIT;
    let mut halt = |_: &tree_sitter::ParseState| Instant::now() > deadline;
    let bytes = text.as_bytes();
    let mut input = |offset: usize, _: Point| &bytes[offset.min(bytes.len())..];
    let Some(tree) = parser.parse_with_options(
        &mut input,
        None,
        Some(ParseOptions::new().progress_callback(&mut halt)),
    ) else {
        out.set_error("bash parser gave up (time limit)");
        return out;
    };
    let root = tree.root_node();
    if root.has_error() {
        out.set_error("syntax error");
    }
    let src = text.as_bytes();
    let functions = collect_functions(root, src, &mut out);
    variables(root, src, &mut out);
    edges(root, src, &functions, &mut out);
    out
}

fn line(node: TsNode) -> u32 {
    u32::try_from(node.start_position().row + 1).unwrap_or(u32::MAX)
}

fn end_line(node: TsNode) -> u32 {
    u32::try_from(node.end_position().row + 1).unwrap_or(u32::MAX)
}

fn text<'a>(node: TsNode, src: &'a [u8]) -> &'a str {
    node.utf8_text(src).unwrap_or("")
}

/// Pushes the children of `node` so that they pop in document order.
fn push_children<'t, T: Clone>(stack: &mut Vec<(TsNode<'t>, T)>, node: TsNode<'t>, ctx: &T) {
    let mut cursor = node.walk();
    let children: Vec<TsNode<'t>> = node.children(&mut cursor).collect();
    for child in children.into_iter().rev() {
        stack.push((child, ctx.clone()));
    }
}

/// Function nodes (at any depth; the parent is the enclosing function or the File node).
/// Returns the tree-sitter node id -> graph index map and the name -> index map is kept in `out`.
fn collect_functions(root: TsNode, src: &[u8], out: &mut ParsedFile) -> Functions {
    let mut functions = Functions::default();
    let mut stack: Vec<(TsNode, u32)> = vec![(root, 0)];
    while let Some((node, enclosing)) = stack.pop() {
        let mut ctx = enclosing;
        if node.kind() == "function_definition"
            && let Some(name) = node.child_by_field_name("name")
        {
            let name = text(name, src).trim();
            if !name.is_empty() {
                let index = out.add_node(Node::new(
                    NodeKind::Function,
                    name,
                    enclosing,
                    line(node),
                    end_line(node),
                ));
                functions.by_node.insert(node.id(), index);
                functions.by_name.entry(name.to_owned()).or_insert(index);
                ctx = index;
            }
        }
        push_children(&mut stack, node, &ctx);
    }
    functions
}

#[derive(Default)]
struct Functions {
    by_node: HashMap<usize, u32>,
    by_name: HashMap<String, u32>,
}

/// Variable nodes: top-level assignments and declaration commands.
fn variables(root: TsNode, src: &[u8], out: &mut ParsedFile) {
    let mut cursor = root.walk();
    let children: Vec<TsNode> = root.children(&mut cursor).collect();
    for child in children {
        match child.kind() {
            "variable_assignment" => {
                if let Some(name) = child.child_by_field_name("name") {
                    add_variable(out, text(name, src), child, false);
                }
            }
            "declaration_command" => declaration(child, src, out),
            _ => {}
        }
    }
}

fn declaration(node: TsNode, src: &[u8], out: &mut ParsedFile) {
    let mut cursor = node.walk();
    let parts: Vec<TsNode> = node.children(&mut cursor).collect();
    let keyword = parts.first().map_or("", |p| text(*p, src));
    let exports = match keyword {
        "export" => true,
        "declare" | "typeset" => parts.iter().skip(1).any(|p| {
            let t = text(*p, src);
            p.kind() == "word" && t.starts_with('-') && !t.starts_with("--") && t.contains('x')
        }),
        _ => false,
    };
    for part in parts.iter().skip(1) {
        let name = match part.kind() {
            "variable_assignment" => part.child_by_field_name("name").map(|n| text(n, src)),
            "variable_name" => Some(text(*part, src)),
            _ => None,
        };
        if let Some(name) = name {
            add_variable(out, name, *part, exports);
        }
    }
}

fn add_variable(out: &mut ParsedFile, name: &str, at: TsNode, exported: bool) {
    let name = name.trim();
    if name.is_empty() {
        return;
    }
    if let Some(index) = out.find(NodeKind::Variable, name) {
        if exported {
            out.nodes[index as usize]
                .extra
                .insert("exported".into(), true.into());
        }
        return;
    }
    out.add_node(
        Node::new(NodeKind::Variable, name, 0, line(at), end_line(at)).extra("exported", exported),
    );
}

/// IMPORTS for `source`/`.` and CALLS to functions of the file, from every command position.
fn edges(root: TsNode, src: &[u8], functions: &Functions, out: &mut ParsedFile) {
    let mut stack: Vec<(TsNode, u32)> = vec![(root, 0)];
    while let Some((node, source)) = stack.pop() {
        let mut ctx = source;
        match node.kind() {
            "function_definition" => {
                if let Some(index) = functions.by_node.get(&node.id()) {
                    ctx = *index;
                }
            }
            "command" => command(node, src, source, functions, out),
            _ => {}
        }
        push_children(&mut stack, node, &ctx);
    }
}

fn command(node: TsNode, src: &[u8], source: u32, functions: &Functions, out: &mut ParsedFile) {
    let Some(name) = node.child_by_field_name("name") else {
        return;
    };
    let name = text(name, src).trim();
    if name == "source" || name == "." {
        if let Some(path) = literal_argument(node, src) {
            out.add_edge(
                Edge::to_ref(EdgeKind::Imports, source, RefScheme::File, path, line(node))
                    .extra("via", name),
            );
        }
    } else if let Some(target) = functions.by_name.get(name) {
        out.add_edge(Edge::local(EdgeKind::Calls, source, *target, line(node)));
    }
}

/// First argument of a command when it is a plain literal (no expansion), unquoted.
fn literal_argument(node: TsNode, src: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    let argument = node
        .children_by_field_name("argument", &mut cursor)
        .next()?;
    let raw = text(argument, src);
    let value = match argument.kind() {
        "word" => raw.to_owned(),
        "raw_string" => raw.trim_matches('\'').to_owned(),
        "string" => {
            let mut inner = argument.walk();
            if argument
                .named_children(&mut inner)
                .any(|c| c.kind() != "string_content")
            {
                return None;
            }
            raw.trim_matches('"').to_owned()
        }
        _ => return None,
    };
    (!value.is_empty() && !value.contains(['$', '`'])).then_some(value)
}
