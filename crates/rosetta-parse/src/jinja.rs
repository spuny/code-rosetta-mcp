//! Jinja2 parser (owner P4). Contract: docs/design.md section 4, "Jinja2 (scanner)".
//!
//! Lexical: the template is cut into actions by [`crate::scan`], statements are interpreted by tag
//! name and the expressions in `{{ }}` and tags are tokenized only far enough to find free
//! variables, macro calls and literal template paths. Nothing here fails.
//!
//! Node names: Template `template`; Block and Macro by their name; Variable
//! `var:<scope>:<name>` with scope `template`, `block:<name>` or `macro:<name>`.
use crate::scan::{Action, ActionKind, Dialect, Lines, scan};
use rosetta_model::{Edge, EdgeKind, Language, Node, NodeKind, ParsedFile, RefScheme};
use std::collections::{HashMap, HashSet};

/// Names that are never free variables.
const EXCLUDED: &[&str] = &[
    "loop",
    "self",
    "super",
    "range",
    "dict",
    "namespace",
    "lipsum",
    "cycler",
    "joiner",
    "true",
    "false",
    "none",
    "True",
    "False",
    "None",
    "and",
    "or",
    "not",
    "in",
    "is",
    "if",
    "else",
];

/// Parses one file. Must be total: never panic, bound recursion, stay fast on 1 MiB input.
pub(crate) fn parse(path: &str, text: &str) -> ParsedFile {
    let mut p = ParsedFile::new(Language::Jinja, path, text);
    if text.is_empty() {
        return p;
    }
    let lines = Lines::new(text);
    let last_line = p.nodes[0].line_end.max(1);
    let template = p.add_node(Node::new(NodeKind::Template, "template", 0, 1, last_line));
    let actions = scan(text, Dialect::Jinja);
    let mut walker = Walker::new(p, &lines, template, last_line);
    walker.collect_macros(&actions, text);
    walker.run(&actions, text);
    walker.finish(&actions, &lines)
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    Num,
    Punct(String),
}

#[derive(Clone, Debug)]
struct Token {
    tok: Tok,
    at: usize,
}

impl Token {
    fn ident(&self) -> Option<&str> {
        match &self.tok {
            Tok::Ident(s) => Some(s),
            _ => None,
        }
    }

    fn is(&self, punct: &str) -> bool {
        matches!(&self.tok, Tok::Punct(p) if p == punct)
    }

    fn is_ident(&self, word: &str) -> bool {
        self.ident() == Some(word)
    }
}

fn lex(src: &str, base: usize) -> Vec<Token> {
    let mut out = Vec::new();
    let mut chars = src.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let at = base + i;
        if c.is_whitespace() {
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let mut s = String::from(c);
            while let Some(&(_, d)) = chars.peek() {
                if d.is_alphanumeric() || d == '_' {
                    s.push(d);
                    chars.next();
                } else {
                    break;
                }
            }
            out.push(Token {
                tok: Tok::Ident(s),
                at,
            });
        } else if c.is_ascii_digit() {
            while let Some(&(_, d)) = chars.peek() {
                if d.is_ascii_digit() || d == '_' {
                    chars.next();
                } else {
                    break;
                }
            }
            let mut ahead = chars.clone();
            if ahead.next().is_some_and(|(_, d)| d == '.')
                && ahead.next().is_some_and(|(_, d)| d.is_ascii_digit())
            {
                chars.next();
                while chars.peek().is_some_and(|(_, d)| d.is_ascii_digit()) {
                    chars.next();
                }
            }
            out.push(Token { tok: Tok::Num, at });
        } else if c == '"' || c == '\'' {
            let mut s = String::new();
            while let Some((_, d)) = chars.next() {
                if d == '\\' {
                    if let Some((_, n)) = chars.next() {
                        s.push(n);
                    }
                } else if d == c {
                    break;
                } else {
                    s.push(d);
                }
            }
            out.push(Token {
                tok: Tok::Str(s),
                at,
            });
        } else {
            let mut punct = String::from(c);
            if let Some(&(_, d)) = chars.peek() {
                let pair = matches!(
                    (c, d),
                    ('=' | '!' | '<' | '>', '=') | ('*', '*') | ('/', '/')
                );
                if pair {
                    punct.push(d);
                    chars.next();
                }
            }
            out.push(Token {
                tok: Tok::Punct(punct),
                at,
            });
        }
    }
    out
}

/// Index of the first token at bracket depth 0 for which `pred` holds.
fn top_level(toks: &[Token], pred: impl Fn(&Token) -> bool) -> Option<usize> {
    let mut depth = 0i32;
    for (i, t) in toks.iter().enumerate() {
        if let Tok::Punct(p) = &t.tok {
            match p.as_str() {
                "(" | "[" | "{" => depth += 1,
                ")" | "]" | "}" => depth -= 1,
                _ => {}
            }
        }
        if depth <= 0 && pred(t) {
            return Some(i);
        }
    }
    None
}

/// Splits at top-level commas.
fn split_commas(toks: &[Token]) -> Vec<&[Token]> {
    let mut out = Vec::new();
    let mut from = 0;
    let mut depth = 0i32;
    for (i, t) in toks.iter().enumerate() {
        if let Tok::Punct(p) = &t.tok {
            match p.as_str() {
                "(" | "[" | "{" => depth += 1,
                ")" | "]" | "}" => depth -= 1,
                "," if depth == 0 => {
                    out.push(&toks[from..i]);
                    from = i + 1;
                }
                _ => {}
            }
        }
    }
    if from < toks.len() {
        out.push(&toks[from..]);
    }
    out
}

/// Most frames open at once (see [`Walker::push`]).
const MAX_FRAMES: usize = 1000;

struct Frame {
    tag: &'static str,
    bound: HashSet<String>,
    /// Node of a scope frame (template, block, macro).
    scope: Option<u32>,
}

struct Walker<'a> {
    p: ParsedFile,
    lines: &'a Lines,
    template: u32,
    last_line: u32,
    frames: Vec<Frame>,
    /// How many open frames bind each name, so lookups do not walk the frame stack.
    bound_count: HashMap<String, u32>,
    /// Indexes into `frames` of the frames that open a scope (template, block, macro).
    scope_frames: Vec<usize>,
    macros: HashSet<String>,
    /// `{% import "x" as y %}`: alias -> template path.
    aliases: HashMap<String, String>,
    /// `{% from "x" import m as n %}`: local name -> (template path, macro name).
    imported: HashMap<String, (String, String)>,
    labels: HashMap<u32, String>,
    vars: HashMap<(u32, String), u32>,
    pending: Vec<(u32, String, u32)>,
    raw: bool,
}

impl<'a> Walker<'a> {
    fn new(p: ParsedFile, lines: &'a Lines, template: u32, last_line: u32) -> Self {
        let mut labels = HashMap::new();
        labels.insert(template, "template".to_owned());
        Self {
            p,
            lines,
            template,
            last_line,
            frames: vec![Frame {
                tag: "template",
                bound: HashSet::new(),
                scope: Some(template),
            }],
            bound_count: HashMap::new(),
            scope_frames: vec![0],
            macros: HashSet::new(),
            aliases: HashMap::new(),
            imported: HashMap::new(),
            labels,
            vars: HashMap::new(),
            pending: Vec::new(),
            raw: false,
        }
    }

    fn collect_macros(&mut self, actions: &[Action], text: &str) {
        for a in actions.iter().filter(|a| a.kind == ActionKind::Stmt) {
            let toks = lex(a.inner(text), a.inner_start);
            if toks.first().is_some_and(|t| t.is_ident("macro"))
                && let Some(name) = toks.get(1).and_then(Token::ident)
            {
                self.macros.insert(name.to_owned());
            }
        }
    }

    fn run(&mut self, actions: &[Action], text: &str) {
        for a in actions {
            if self.raw {
                if a.kind == ActionKind::Stmt
                    && lex(a.inner(text), a.inner_start)
                        .first()
                        .is_some_and(|t| t.is_ident("endraw"))
                {
                    self.raw = false;
                }
                continue;
            }
            match a.kind {
                ActionKind::Comment => {}
                ActionKind::Expr => {
                    let toks = lex(a.inner(text), a.inner_start);
                    self.read(&toks);
                }
                ActionKind::Stmt => self.stmt(a, text),
            }
        }
    }

    fn finish(mut self, actions: &[Action], lines: &Lines) -> ParsedFile {
        let pending = std::mem::take(&mut self.pending);
        for (source, name, line) in pending {
            if let Some(target) = self.p.find(NodeKind::Macro, &name) {
                self.p
                    .add_edge(Edge::local(EdgeKind::CallsMacro, source, target, line));
            }
        }
        if let Some(a) = actions.iter().find(|a| !a.terminated) {
            self.p.set_error(format!(
                "unterminated template action at line {}",
                lines.line(a.start)
            ));
        }
        self.p
    }

    fn scope(&self) -> u32 {
        self.scope_frames
            .last()
            .and_then(|i| self.frames.get(*i))
            .and_then(|f| f.scope)
            .unwrap_or(self.template)
    }

    fn is_bound(&self, name: &str) -> bool {
        self.bound_count.get(name).is_some_and(|n| *n > 0)
    }

    fn bind(&mut self, name: &str) {
        if let Some(frame) = self.frames.iter_mut().rev().find(|f| f.tag != "if")
            && frame.bound.insert(name.to_owned())
        {
            *self.bound_count.entry(name.to_owned()).or_insert(0) += 1;
        }
    }

    /// Opens a frame. Beyond [`MAX_FRAMES`] open frames further openers are ignored (their end
    /// tags then close the innermost frame of the same tag), which bounds every stack walk.
    fn push(&mut self, tag: &'static str, bound: HashSet<String>, scope: Option<u32>) {
        if self.frames.len() >= MAX_FRAMES {
            self.p
                .set_error(format!("template nesting deeper than {MAX_FRAMES}"));
            return;
        }
        for name in &bound {
            *self.bound_count.entry(name.clone()).or_insert(0) += 1;
        }
        if scope.is_some() {
            self.scope_frames.push(self.frames.len());
        }
        self.frames.push(Frame { tag, bound, scope });
    }

    /// Closes the innermost open frame with `tag` (and anything left open inside it); an end tag
    /// without an opener is ignored.
    fn close(&mut self, tag: &str, end_line: u32) {
        let Some(at) = self
            .frames
            .iter()
            .rposition(|f| f.tag == tag && f.tag != "template")
        else {
            return;
        };
        if let Some(node) = self.frames[at].scope {
            self.p.nodes[node as usize].line_end =
                end_line.max(self.p.nodes[node as usize].line_start);
        }
        for frame in self.frames.drain(at..) {
            for name in frame.bound {
                if let Some(n) = self.bound_count.get_mut(&name) {
                    *n = n.saturating_sub(1);
                }
            }
        }
        while self.scope_frames.last().is_some_and(|i| *i >= at) {
            self.scope_frames.pop();
        }
    }

    fn free(&mut self, name: &str, at: usize) {
        let scope = self.scope();
        let key = (scope, name.to_owned());
        if self.vars.contains_key(&key) {
            return;
        }
        let line = self.lines.line(at);
        let label = self.labels.get(&scope).cloned().unwrap_or_default();
        let index = self.p.add_node(Node::new(
            NodeKind::Variable,
            format!("var:{label}:{name}"),
            scope,
            line,
            line,
        ));
        self.vars.insert(key, index);
        self.p
            .add_edge(Edge::local(EdgeKind::UsesVariable, scope, index, line));
    }

    fn macro_call_ref(&mut self, path: &str, name: &str, at: usize) {
        let source = self.scope();
        let line = self.lines.line(at);
        self.p.add_edge(
            Edge::to_ref(
                EdgeKind::CallsMacro,
                source,
                RefScheme::Template,
                format!("{path}#{name}"),
                line,
            )
            .extra("macro", name),
        );
    }

    /// Reads an expression: free variables, macro calls.
    fn read(&mut self, toks: &[Token]) {
        let mut parens: Vec<char> = Vec::new();
        let mut after_is = false;
        for (i, t) in toks.iter().enumerate() {
            let prev = i.checked_sub(1).and_then(|k| toks.get(k));
            let next = toks.get(i + 1);
            match &t.tok {
                Tok::Punct(p) => match p.as_str() {
                    "(" => parens.push('('),
                    "[" => parens.push('['),
                    "{" => parens.push('{'),
                    ")" | "]" | "}" => {
                        parens.pop();
                    }
                    _ => {}
                },
                Tok::Ident(name) => {
                    if after_is {
                        if name == "not" {
                            continue;
                        }
                        after_is = false;
                        continue;
                    }
                    if name == "is" {
                        after_is = true;
                        continue;
                    }
                    if prev.is_some_and(|p| p.is("|")) {
                        continue;
                    }
                    let call = next.is_some_and(|n| n.is("("));
                    if prev.is_some_and(|p| p.is(".")) {
                        if call
                            && let Some(alias) = i.checked_sub(2).and_then(|k| toks[k].ident())
                            && let Some(path) = self.aliases.get(alias).cloned()
                        {
                            self.macro_call_ref(&path, name, t.at);
                        }
                        continue;
                    }
                    if call {
                        if self.macros.contains(name) {
                            let source = self.scope();
                            let line = self.lines.line(t.at);
                            self.pending.push((source, name.clone(), line));
                        } else if let Some((path, original)) = self.imported.get(name).cloned() {
                            self.macro_call_ref(&path, &original, t.at);
                        }
                        continue;
                    }
                    if parens.last() == Some(&'(') && next.is_some_and(|n| n.is("=")) {
                        continue;
                    }
                    if EXCLUDED.contains(&name.as_str())
                        || self.macros.contains(name)
                        || self.is_bound(name)
                        || self.aliases.contains_key(name)
                        || self.imported.contains_key(name)
                    {
                        continue;
                    }
                    self.free(name, t.at);
                }
                Tok::Str(_) | Tok::Num => {
                    if after_is {
                        after_is = false;
                    }
                }
            }
        }
    }

    fn template_edge(&mut self, kind: EdgeKind, path: &str, line: u32) {
        if path.is_empty() {
            return;
        }
        self.p.add_edge(Edge::to_ref(
            kind,
            self.template,
            RefScheme::Template,
            path,
            line,
        ));
    }

    fn stmt(&mut self, a: &Action, text: &str) {
        let toks = lex(a.inner(text), a.inner_start);
        let Some(tag) = toks.first().and_then(Token::ident) else {
            return;
        };
        let rest = &toks[1..];
        let line = self.lines.line(a.start);
        let end_line = self.lines.line(a.end.saturating_sub(1));
        match tag {
            "block" => {
                let Some(name) = rest.first().and_then(Token::ident) else {
                    return;
                };
                let parent = self.scope();
                let index = self.p.add_node(Node::new(
                    NodeKind::Block,
                    name,
                    parent,
                    line,
                    self.last_line,
                ));
                self.labels.insert(index, format!("block:{name}"));
                self.push("block", HashSet::new(), Some(index));
            }
            "endblock" => self.close("block", end_line),
            "macro" => self.macro_def(rest, line),
            "endmacro" => self.close("macro", end_line),
            "extends" => match rest.first().map(|t| &t.tok) {
                Some(Tok::Str(path)) => {
                    let path = path.clone();
                    self.template_edge(EdgeKind::Extends, &path, line);
                }
                _ => self.read(rest),
            },
            "include" => self.include(rest, line),
            "import" => self.import(rest, line),
            "from" => self.import_from(rest, line),
            "set" => self.set(rest),
            "for" => self.for_loop(rest),
            "endfor" => self.close("for", end_line),
            "if" => {
                self.read(rest);
                self.push("if", HashSet::new(), None);
            }
            "elif" => self.read(rest),
            "endif" => self.close("if", end_line),
            "with" => self.with(rest),
            "endwith" => self.close("with", end_line),
            "call" => self.call(rest),
            "endcall" => self.close("call", end_line),
            "filter" => {
                self.read(rest.get(1..).unwrap_or(&[]));
                self.push("filter", HashSet::new(), None);
            }
            "endfilter" => self.close("filter", end_line),
            "endset" => self.close("set", end_line),
            "autoescape" => {
                self.read(rest);
                self.push("autoescape", HashSet::new(), None);
            }
            "endautoescape" => self.close("autoescape", end_line),
            "trans" => self.push("trans", HashSet::new(), None),
            "endtrans" => self.close("trans", end_line),
            "do" | "print" => self.read(rest),
            "raw" => self.raw = true,
            _ => {}
        }
    }

    fn macro_def(&mut self, rest: &[Token], line: u32) {
        let Some(name) = rest.first().and_then(Token::ident) else {
            return;
        };
        let mut names = Vec::new();
        let mut defaults: Vec<&[Token]> = Vec::new();
        if rest.get(1).is_some_and(|t| t.is("(")) {
            let close = top_level_close(rest, 1);
            let args = &rest[2..close.min(rest.len())];
            for arg in split_commas(args) {
                if let Some(arg_name) = arg.first().and_then(Token::ident) {
                    names.push(arg_name.to_owned());
                    if arg.get(1).is_some_and(|t| t.is("=")) {
                        defaults.push(&arg[2..]);
                    }
                }
            }
        }
        let parent = self.scope();
        let index = self.p.add_node(
            Node::new(NodeKind::Macro, name, parent, line, self.last_line)
                .signature(names.join(", ")),
        );
        self.labels.insert(index, format!("macro:{name}"));
        let mut bound: HashSet<String> = names.into_iter().collect();
        bound.extend(["caller", "varargs", "kwargs"].map(str::to_owned));
        self.push("macro", bound, Some(index));
        for d in defaults {
            self.read(d);
        }
    }

    fn include(&mut self, rest: &[Token], line: u32) {
        match rest.first().map(|t| &t.tok) {
            Some(Tok::Str(path)) => {
                let path = path.clone();
                self.template_edge(EdgeKind::Includes, &path, line);
            }
            Some(Tok::Punct(p)) if p == "[" => {
                let close = top_level_close(rest, 0);
                let inner = &rest[1..close.min(rest.len())];
                let mut dynamic = false;
                for t in inner {
                    match &t.tok {
                        Tok::Str(path) => {
                            let path = path.clone();
                            self.template_edge(EdgeKind::Includes, &path, line);
                        }
                        Tok::Punct(p) if p == "," => {}
                        _ => dynamic = true,
                    }
                }
                if dynamic {
                    self.read(inner);
                }
            }
            _ => {
                let end = top_level(rest, |t| {
                    t.is_ident("ignore") || t.is_ident("with") || t.is_ident("without")
                })
                .unwrap_or(rest.len());
                self.read(&rest[..end]);
            }
        }
    }

    fn import(&mut self, rest: &[Token], line: u32) {
        let as_at = rest.iter().position(|t| t.is_ident("as"));
        let source = &rest[..as_at.unwrap_or(rest.len())];
        let path = match source.first().map(|t| &t.tok) {
            Some(Tok::Str(path)) if source.len() == 1 => Some(path.clone()),
            _ => {
                self.read(source);
                None
            }
        };
        if let Some(path) = &path {
            self.template_edge(EdgeKind::Imports, path, line);
        }
        if let Some(alias) = as_at.and_then(|k| rest.get(k + 1)).and_then(Token::ident) {
            self.bind(alias);
            if let Some(path) = path {
                self.aliases.insert(alias.to_owned(), path);
            }
        }
    }

    fn import_from(&mut self, rest: &[Token], line: u32) {
        let import_at = rest.iter().position(|t| t.is_ident("import"));
        let source = &rest[..import_at.unwrap_or(rest.len())];
        let path = match source.first().map(|t| &t.tok) {
            Some(Tok::Str(path)) if source.len() == 1 => Some(path.clone()),
            _ => {
                self.read(source);
                None
            }
        };
        if let Some(path) = &path {
            self.template_edge(EdgeKind::Imports, path, line);
        }
        let Some(at) = import_at else {
            return;
        };
        let names = &rest[at + 1..];
        let end = names
            .iter()
            .position(|t| t.is_ident("with") || t.is_ident("without"))
            .unwrap_or(names.len());
        for item in split_commas(&names[..end]) {
            let Some(original) = item.first().and_then(Token::ident) else {
                continue;
            };
            let local = if item.get(1).is_some_and(|t| t.is_ident("as")) {
                item.get(2).and_then(Token::ident).unwrap_or(original)
            } else {
                original
            };
            self.bind(local);
            if let Some(path) = &path {
                self.imported
                    .insert(local.to_owned(), (path.clone(), original.to_owned()));
            }
        }
    }

    fn set(&mut self, rest: &[Token]) {
        let eq = top_level(rest, |t| t.is("="));
        let (targets, value) = match eq {
            Some(i) => (&rest[..i], Some(&rest[i + 1..])),
            None => {
                let end = top_level(rest, |t| t.is("|")).unwrap_or(rest.len());
                (&rest[..end], None)
            }
        };
        if let Some(value) = value {
            self.read(value);
        }
        for (i, t) in targets.iter().enumerate() {
            let Some(name) = t.ident() else { continue };
            if i > 0 && targets[i - 1].is(".") {
                continue;
            }
            if targets.get(i + 1).is_some_and(|n| n.is(".")) {
                self.read(std::slice::from_ref(t));
            } else {
                self.bind(name);
            }
        }
        if value.is_none() {
            self.push("set", HashSet::new(), None);
        }
    }

    fn for_loop(&mut self, rest: &[Token]) {
        let in_at = rest.iter().position(|t| t.is_ident("in"));
        let Some(in_at) = in_at else {
            self.push("for", HashSet::from(["loop".to_owned()]), None);
            return;
        };
        let mut bound: HashSet<String> = rest[..in_at]
            .iter()
            .filter_map(Token::ident)
            .map(str::to_owned)
            .collect();
        bound.insert("loop".to_owned());
        let after = &rest[in_at + 1..];
        let cond_at = top_level(after, |t| t.is_ident("if"));
        let iterable = &after[..cond_at.unwrap_or(after.len())];
        let iterable = match iterable.last() {
            Some(t) if t.is_ident("recursive") => &iterable[..iterable.len() - 1],
            _ => iterable,
        };
        self.read(iterable);
        self.push("for", bound, None);
        if let Some(c) = cond_at {
            self.read(&after[c + 1..]);
        }
    }

    fn with(&mut self, rest: &[Token]) {
        let mut bound = HashSet::new();
        let mut values: Vec<&[Token]> = Vec::new();
        for arg in split_commas(rest) {
            if let Some(name) = arg.first().and_then(Token::ident)
                && arg.get(1).is_some_and(|t| t.is("="))
            {
                bound.insert(name.to_owned());
                values.push(&arg[2..]);
            }
        }
        for v in values {
            self.read(v);
        }
        self.push("with", bound, None);
    }

    fn call(&mut self, rest: &[Token]) {
        let mut bound = HashSet::new();
        let mut body = rest;
        if rest.first().is_some_and(|t| t.is("(")) {
            let close = top_level_close(rest, 0);
            for arg in split_commas(&rest[1..close.min(rest.len())]) {
                if let Some(name) = arg.first().and_then(Token::ident) {
                    bound.insert(name.to_owned());
                }
            }
            body = rest.get(close + 1..).unwrap_or(&[]);
        }
        self.read(body);
        self.push("call", bound, None);
    }
}

/// Index of the bracket that closes the one at `open` (or the length when it never closes).
fn top_level_close(toks: &[Token], open: usize) -> usize {
    let mut depth = 0i32;
    for (i, t) in toks.iter().enumerate().skip(open) {
        if let Tok::Punct(p) = &t.tok {
            match p.as_str() {
                "(" | "[" | "{" => depth += 1,
                ")" | "]" | "}" => {
                    depth -= 1;
                    if depth == 0 {
                        return i;
                    }
                }
                _ => {}
            }
        }
    }
    toks.len()
}
