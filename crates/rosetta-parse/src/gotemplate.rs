//! Go template parser for Helm charts (owner P4). Contract: docs/design.md section 4,
//! "Go template (scanner)".
//!
//! Lexical: [`crate::scan`] cuts the file into actions, each action is tokenized just far enough
//! to find `define`/`block` names, `include`/`template` calls with literal names and `.Values`
//! paths. The block structure (`if`, `range`, `with`, `define`, `end`) is tracked so that a call is
//! attributed to its enclosing template and so that `.field` inside `with .Values.x` is read as a
//! path below `x`.
use crate::scan::{Action, ActionKind, Dialect, Lines, scan};
use rosetta_model::{Edge, EdgeKind, Language, Node, NodeKind, ParsedFile, RefScheme, cut_value};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// Most `.Values` paths kept in the File node's extra.
const MAX_PATHS: usize = 100;

/// Parses one file. Must be total: never panic, bound recursion, stay fast on 1 MiB input.
pub(crate) fn parse(path: &str, text: &str) -> ParsedFile {
    let mut p = ParsedFile::new(Language::GoTemplate, path, text);
    if text.is_empty() {
        return p;
    }
    let lines = Lines::new(text);
    let actions = scan(text, Dialect::Go);
    let mut walker = Walker::new(&mut p, &lines);
    walker.collect(&actions, text);
    walker.finish();
    if let Some(a) = actions.iter().find(|a| !a.terminated) {
        p.set_error(format!(
            "unterminated template action at line {}",
            lines.line(a.start)
        ));
    }
    masked_yaml(text, path, &mut p);
    p
}

/// Adds the YAML structure of a templated manifest: the template actions are masked and the
/// result goes through the YAML extractor (nodes marked `extra.templated = true`). When the masked
/// text is not valid YAML only the template-level output is kept, without a YAML error.
fn masked_yaml(text: &str, path: &str, out: &mut ParsedFile) {
    if path.ends_with(".tpl") || !text.contains(':') {
        return;
    }
    let masked = crate::scan::mask(text);
    let mut probe = ParsedFile::new(Language::GoTemplate, path, text);
    crate::yaml::extract(&masked, path, true, &mut probe);
    if probe.parse_error.is_none() {
        crate::yaml::extract(&masked, path, true, out);
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    /// `.A.B` (first segment first); an empty list is a lone `.`.
    Field(Vec<String>),
    /// `$`, `$x` with an optional field chain.
    Var(String, Vec<String>),
    Punct(char),
}

#[derive(Clone, Debug)]
struct Token {
    tok: Tok,
    at: usize,
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn lex(src: &str, base: usize) -> Vec<Token> {
    let mut out = Vec::new();
    let mut chars = src.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let at = base + i;
        if c.is_whitespace() {
            continue;
        }
        let tok = if c == '"' {
            let mut s = String::new();
            while let Some((_, d)) = chars.next() {
                if d == '\\' {
                    if let Some((_, n)) = chars.next() {
                        s.push(n);
                    }
                } else if d == '"' {
                    break;
                } else {
                    s.push(d);
                }
            }
            Tok::Str(s)
        } else if c == '`' {
            let mut s = String::new();
            for (_, d) in chars.by_ref() {
                if d == '`' {
                    break;
                }
                s.push(d);
            }
            Tok::Str(s)
        } else if c == '\'' {
            for (_, d) in chars.by_ref() {
                if d == '\'' {
                    break;
                }
            }
            Tok::Punct('\'')
        } else if c == '.' {
            Tok::Field(field_chain(&mut chars))
        } else if c == '$' {
            let mut name = String::new();
            while let Some(&(_, d)) = chars.peek() {
                if is_ident_char(d) {
                    name.push(d);
                    chars.next();
                } else {
                    break;
                }
            }
            let fields = if chars.peek().is_some_and(|(_, d)| *d == '.') {
                chars.next();
                field_chain(&mut chars)
            } else {
                Vec::new()
            };
            Tok::Var(name, fields)
        } else if is_ident_char(c) {
            let mut s = String::from(c);
            while let Some(&(_, d)) = chars.peek() {
                if is_ident_char(d) {
                    s.push(d);
                    chars.next();
                } else {
                    break;
                }
            }
            Tok::Ident(s)
        } else {
            Tok::Punct(c)
        };
        out.push(Token { tok, at });
    }
    out
}

/// Reads `A.B.C` after a consumed `.`.
fn field_chain(chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>) -> Vec<String> {
    let mut fields = Vec::new();
    loop {
        let mut seg = String::new();
        while let Some(&(_, d)) = chars.peek() {
            if is_ident_char(d) {
                seg.push(d);
                chars.next();
            } else {
                break;
            }
        }
        if seg.is_empty() {
            break;
        }
        fields.push(seg);
        if chars.peek().is_some_and(|(_, d)| *d == '.') {
            let mut ahead = chars.clone();
            ahead.next();
            if ahead.peek().is_some_and(|(_, d)| is_ident_char(*d)) {
                chars.next();
                continue;
            }
        }
        break;
    }
    fields
}

#[derive(Clone, Debug, PartialEq)]
enum Dot {
    Root,
    Path(String),
    Unknown,
}

struct Open {
    kind: &'static str,
    template: Option<u32>,
    dot: Dot,
    /// Dot of the enclosing block, restored by `else` in `with`/`range`.
    outer: Dot,
}

struct Walker<'a> {
    p: &'a mut ParsedFile,
    lines: &'a Lines,
    defined: HashMap<String, Vec<u32>>,
    included: Vec<String>,
    paths: Vec<String>,
    path_seen: HashSet<String>,
    /// First line and paths per top-level key, in first-seen order.
    keys: Vec<(String, u32, Vec<String>)>,
    calls: Vec<(u32, String, u32)>,
    stack: Vec<Open>,
    last_line: u32,
}

impl<'a> Walker<'a> {
    fn new(p: &'a mut ParsedFile, lines: &'a Lines) -> Self {
        let last_line = p.nodes[0].line_end.max(1);
        Self {
            p,
            lines,
            defined: HashMap::new(),
            included: Vec::new(),
            paths: Vec::new(),
            path_seen: HashSet::new(),
            keys: Vec::new(),
            calls: Vec::new(),
            stack: Vec::new(),
            last_line,
        }
    }

    fn collect(&mut self, actions: &[Action], text: &str) {
        for a in actions {
            if a.kind == ActionKind::Comment {
                continue;
            }
            let toks = lex(a.inner(text), a.inner_start);
            let line = self.lines.line(a.start);
            let end_line = self.lines.line(a.end.saturating_sub(1));
            self.action(&toks, a.kind, line, end_line);
        }
    }

    fn dot(&self) -> Dot {
        self.stack.last().map_or(Dot::Root, |o| o.dot.clone())
    }

    fn current_template(&self) -> u32 {
        self.stack
            .iter()
            .rev()
            .find_map(|o| o.template)
            .unwrap_or(0)
    }

    fn action(&mut self, toks: &[Token], kind: ActionKind, line: u32, end_line: u32) {
        let first = toks.first().and_then(|t| match &t.tok {
            Tok::Ident(s) => Some(s.as_str()),
            _ => None,
        });
        // A pipeline may open with a declaration: `{{ with $x := ... }}`; the keyword stays first.
        let mut body = toks;
        if kind == ActionKind::Stmt {
            match first {
                Some("define") | Some("block") => {
                    let name = toks.get(1).and_then(|t| match &t.tok {
                        Tok::Str(s) => Some(s.clone()),
                        _ => None,
                    });
                    let is_block = first == Some("block");
                    let outer = self.dot();
                    let template = name.map(|name| {
                        let index = self.p.add_node(
                            Node::new(NodeKind::Template, name.clone(), 0, line, self.last_line)
                                .extra("directive", if is_block { "block" } else { "define" }),
                        );
                        self.defined.entry(name).or_default().push(index);
                        index
                    });
                    if is_block && let Some(index) = template {
                        let name = self.p.nodes[index as usize].name.clone();
                        let source = self.current_template();
                        self.calls.push((source, name, line));
                    }
                    self.stack.push(Open {
                        kind: "define",
                        template,
                        dot: Dot::Unknown,
                        outer,
                    });
                    body = toks.get(2..).unwrap_or(&[]);
                }
                Some("if") => {
                    let dot = self.dot();
                    self.stack.push(Open {
                        kind: "if",
                        template: None,
                        dot: dot.clone(),
                        outer: dot,
                    });
                    body = &toks[1..];
                }
                Some(word @ ("range" | "with")) => {
                    let outer = self.dot();
                    let subject = self.subject_path(&toks[1..]);
                    let dot = match (word, subject) {
                        ("with", Some(path)) => Dot::Path(path),
                        _ => Dot::Unknown,
                    };
                    self.stack.push(Open {
                        kind: if word == "range" { "range" } else { "with" },
                        template: None,
                        dot,
                        outer: outer.clone(),
                    });
                    // The subject is evaluated in the enclosing dot context.
                    self.read_in(&toks[1..], &outer);
                    return;
                }
                Some("else") => {
                    if let Some(top) = self.stack.last_mut()
                        && matches!(top.kind, "with" | "range")
                    {
                        top.dot = top.outer.clone();
                    }
                    body = &toks[1..];
                    if body.first().is_some_and(
                        |t| matches!(&t.tok, Tok::Ident(w) if w == "if" || w == "with"),
                    ) {
                        body = &body[1..];
                    }
                }
                Some("end") => {
                    if let Some(open) = self.stack.pop()
                        && let Some(template) = open.template
                    {
                        let node = &mut self.p.nodes[template as usize];
                        node.line_end = end_line.max(node.line_start);
                    }
                    return;
                }
                _ => {}
            }
        }
        let dot = self.dot();
        self.read_in(body, &dot);
    }

    /// `.Values.a.b` of a `with` subject when it is exactly one field chain.
    fn subject_path(&self, toks: &[Token]) -> Option<String> {
        let [t] = toks else {
            return None;
        };
        let Tok::Field(chain) = &t.tok else {
            return None;
        };
        self.values_path(chain, &self.dot())
            .filter(|p| !p.is_empty())
    }

    /// Path below `.Values` for a field chain read in dot context `dot`.
    fn values_path(&self, chain: &[String], dot: &Dot) -> Option<String> {
        if chain.first().map(String::as_str) == Some("Values") {
            return Some(chain[1..].join("."));
        }
        match dot {
            Dot::Path(base) if !chain.is_empty() => Some(format!("{base}.{}", chain.join("."))),
            _ => None,
        }
    }

    fn record(&mut self, path: &str, line: u32) {
        let Some(key) = path.split('.').next().filter(|k| !k.is_empty()) else {
            return;
        };
        if self.path_seen.insert(path.to_owned()) && self.paths.len() < MAX_PATHS {
            self.paths.push(path.to_owned());
        }
        match self.keys.iter_mut().find(|(k, _, _)| k == key) {
            Some((_, _, paths)) => {
                if !paths.iter().any(|p| p == path) && paths.len() < MAX_PATHS {
                    paths.push(path.to_owned());
                }
            }
            None => self
                .keys
                .push((key.to_owned(), line, vec![path.to_owned()])),
        }
    }

    fn read_in(&mut self, toks: &[Token], dot: &Dot) {
        let source = self.current_template();
        let mut i = 0;
        while i < toks.len() {
            let t = &toks[i];
            let at_line = self.lines.line(t.at);
            match &t.tok {
                Tok::Ident(word) if word == "include" || word == "template" => {
                    if let Some(Token {
                        tok: Tok::Str(name),
                        ..
                    }) = toks.get(i + 1)
                    {
                        if !self.included.contains(name) {
                            self.included.push(name.clone());
                        }
                        self.calls.push((source, name.clone(), at_line));
                        i += 2;
                        continue;
                    }
                }
                Tok::Ident(word) if word == "index" => {
                    // `index .Values "a" "b"` reads the path a.b.
                    if let Some(Token {
                        tok: Tok::Field(chain),
                        ..
                    }) = toks.get(i + 1)
                        && chain.len() == 1
                        && chain[0] == "Values"
                    {
                        let keys: Vec<&str> = toks[i + 2..]
                            .iter()
                            .map_while(|t| match &t.tok {
                                Tok::Str(s) => Some(s.as_str()),
                                _ => None,
                            })
                            .collect();
                        if !keys.is_empty() {
                            self.record(&keys.join("."), at_line);
                        }
                        i += 2 + keys.len();
                        continue;
                    }
                }
                Tok::Field(chain) => {
                    if let Some(path) = self.values_path(chain, dot) {
                        self.record(&path, at_line);
                    }
                }
                // `$.Values.x`: `$` is the root context.
                Tok::Var(name, fields)
                    if name.is_empty() && fields.first().map(String::as_str) == Some("Values") =>
                {
                    self.record(&fields[1..].join("."), at_line);
                }
                _ => {}
            }
            i += 1;
        }
    }

    fn finish(mut self) {
        let calls = std::mem::take(&mut self.calls);
        for (source, name, line) in calls {
            let target = self.defined.get(&name).and_then(|v| v.first()).copied();
            let edge = match target {
                Some(t) => Edge::local(EdgeKind::Calls, source, t, line),
                None => Edge::to_ref(EdgeKind::Calls, source, RefScheme::Helm, name, line),
            };
            self.p.add_edge(edge);
        }
        let keys = std::mem::take(&mut self.keys);
        for (key, line, paths) in keys {
            self.p.add_edge(
                Edge::to_ref(EdgeKind::ReadsConfig, 0, RefScheme::Values, key, line)
                    .extra("paths", Value::from(paths)),
            );
        }
        let mut defined: Vec<(u32, String)> = self
            .defined
            .values()
            .flatten()
            .map(|i| (*i, self.p.nodes[*i as usize].name.clone()))
            .collect();
        defined.sort();
        let file = self.p.file_mut();
        let mut put = |key: &str, items: Vec<String>| {
            if !items.is_empty() {
                file.extra
                    .insert(key.to_owned(), cut_value(Value::from(items)));
            }
        };
        put("values_paths", std::mem::take(&mut self.paths));
        put(
            "templates_defined",
            defined.into_iter().map(|(_, n)| n).collect(),
        );
        put("templates_included", std::mem::take(&mut self.included));
    }
}
