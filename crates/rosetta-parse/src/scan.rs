//! Shared delimiter scanner for the Jinja2 and Go template parsers (owner P4): finds `{{ }}`,
//! `{% %}`, `{# #}` (Jinja) and `{{ }}` with `{{-`/`-}}` trim markers and `{{/* */}}` comments (Go)
//! with their byte ranges, respecting string literals. Never fails: an action that is never closed
//! is reported as unterminated and ends at the end of its line.
use memchr::{memchr, memmem};

/// The template language whose delimiters are scanned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Dialect {
    Jinja,
    Go,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ActionKind {
    /// `{{ ... }}` that produces output (Go: any action that is not control flow).
    Expr,
    /// `{% ... %}` (Go: `if`, `else`, `end`, `define`, `block`, `range`, `with`, `break`, `continue`).
    Stmt,
    /// `{# ... #}` or Go `{{/* ... */}}`.
    Comment,
}

/// One template action with byte offsets into the scanned text.
#[derive(Clone, Debug)]
pub(crate) struct Action {
    pub kind: ActionKind,
    /// First byte of the opening delimiter.
    pub start: usize,
    /// One past the last byte of the closing delimiter (for an unterminated action: the end of
    /// its line).
    pub end: usize,
    /// Content between the delimiters, without trim markers.
    pub inner_start: usize,
    pub inner_end: usize,
    pub terminated: bool,
}

impl Action {
    pub fn inner<'a>(&self, text: &'a str) -> &'a str {
        text.get(self.inner_start..self.inner_end).unwrap_or("")
    }
}

/// Longest stretch an action may span; anything longer is reported as unterminated, which keeps
/// the scan linear on hostile input.
const MAX_ACTION_BYTES: usize = 32 * 1024;

/// Go actions whose first word makes them control flow rather than output.
const GO_CONTROL: &[&str] = &[
    "if", "else", "end", "define", "block", "range", "with", "break", "continue",
];

/// Byte offset -> 1-based line number.
pub(crate) struct Lines {
    starts: Vec<usize>,
}

impl Lines {
    pub fn new(text: &str) -> Self {
        let mut starts = vec![0];
        let mut from = 0;
        while let Some(i) = memchr(b'\n', &text.as_bytes()[from..]) {
            from += i + 1;
            starts.push(from);
        }
        Self { starts }
    }

    pub fn line(&self, offset: usize) -> u32 {
        let n = self.starts.partition_point(|s| *s <= offset);
        u32::try_from(n.max(1)).unwrap_or(u32::MAX)
    }
}

/// All actions of `text` in order. Total: any input yields a list, never an error.
pub(crate) fn scan(text: &str, dialect: Dialect) -> Vec<Action> {
    let bytes = text.as_bytes();
    let last = Last::new(bytes);
    // Bytes that the string-aware search may still read before it is replaced by plain search.
    let mut budget = bytes.len().saturating_mul(4).saturating_add(1 << 20);
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(i) = memchr(b'{', &bytes[pos..]) {
        let open = pos + i;
        let kind_byte = bytes.get(open + 1).copied();
        let valid = match dialect {
            Dialect::Jinja => matches!(kind_byte, Some(b'{' | b'%' | b'#')),
            Dialect::Go => kind_byte == Some(b'{'),
        };
        if !valid {
            pos = open + 1;
            continue;
        }
        let action = scan_one(
            bytes,
            open,
            kind_byte.unwrap_or(b'{'),
            dialect,
            &last,
            &mut budget,
        );
        pos = action.end.max(open + 2);
        out.push(action);
    }
    out
}

/// Position of the last closer of each kind, so that an opener after it is known to be
/// unterminated without scanning.
struct Last {
    brace: Option<usize>,
    percent: Option<usize>,
    hash: Option<usize>,
}

impl Last {
    fn new(bytes: &[u8]) -> Self {
        Self {
            brace: memmem::rfind(bytes, b"}}"),
            percent: memmem::rfind(bytes, b"%}"),
            hash: memmem::rfind(bytes, b"#}"),
        }
    }

    fn after(&self, kind: u8, from: usize) -> bool {
        let last = match kind {
            b'%' => self.percent,
            b'#' => self.hash,
            _ => self.brace,
        };
        last.is_none_or(|l| l < from)
    }
}

fn closer(kind: u8) -> &'static [u8] {
    match kind {
        b'%' => b"%}",
        b'#' => b"#}",
        _ => b"}}",
    }
}

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n')
}

fn end_of_line(bytes: &[u8], from: usize) -> usize {
    let limit = bytes.len().min(from + MAX_ACTION_BYTES);
    memchr(b'\n', &bytes[from..limit]).map_or(limit, |i| from + i)
}

fn unterminated(bytes: &[u8], open: usize, inner_start: usize, kind: ActionKind) -> Action {
    let end = end_of_line(bytes, inner_start).max(inner_start);
    Action {
        kind,
        start: open,
        end,
        inner_start,
        inner_end: end,
        terminated: false,
    }
}

fn scan_one(
    bytes: &[u8],
    open: usize,
    kind_byte: u8,
    dialect: Dialect,
    last: &Last,
    budget: &mut usize,
) -> Action {
    let mut inner_start = open + 2;
    // Trim marker after the opening delimiter. Go requires whitespace after `{{-` (otherwise it
    // is a negative number); Jinja also knows `{%+`.
    match bytes.get(inner_start) {
        Some(b'-') => {
            let ok = match dialect {
                Dialect::Jinja => true,
                Dialect::Go => bytes.get(inner_start + 1).is_some_and(|b| is_space(*b)),
            };
            if ok {
                inner_start += 1;
            }
        }
        Some(b'+') if dialect == Dialect::Jinja && kind_byte == b'%' => inner_start += 1,
        _ => {}
    }
    let base_kind = match (dialect, kind_byte) {
        (Dialect::Jinja, b'%') => ActionKind::Stmt,
        (Dialect::Jinja, b'#') => ActionKind::Comment,
        _ => ActionKind::Expr,
    };
    if last.after(kind_byte, inner_start) {
        return unterminated(bytes, open, inner_start, base_kind);
    }
    if kind_byte == b'#' {
        return match memmem::find(&bytes[inner_start..], b"#}") {
            Some(i) => finish(
                bytes,
                open,
                inner_start,
                inner_start + i,
                dialect,
                base_kind,
            ),
            None => unterminated(bytes, open, inner_start, base_kind),
        };
    }
    if dialect == Dialect::Go
        && let Some(action) = go_comment(bytes, open, inner_start)
    {
        return action;
    }
    let limit = bytes.len().min(inner_start + MAX_ACTION_BYTES);
    let aware = if *budget > 0 {
        let (found, used) = find_closer(bytes, inner_start, limit, kind_byte, dialect);
        *budget = budget.saturating_sub(used.max(1));
        found
    } else {
        None
    };
    // Strings or braces confused the scan (or the budget is spent): first plain closer.
    let found = aware.or_else(|| {
        memmem::find(&bytes[inner_start..limit], closer(kind_byte)).map(|i| inner_start + i)
    });
    match found {
        Some(at) => finish(bytes, open, inner_start, at, dialect, base_kind),
        None => unterminated(bytes, open, inner_start, base_kind),
    }
}

/// `at` is the first byte of the closing delimiter.
fn finish(
    bytes: &[u8],
    open: usize,
    inner_start: usize,
    at: usize,
    dialect: Dialect,
    base_kind: ActionKind,
) -> Action {
    let mut inner_end = at;
    if inner_end > inner_start && bytes[inner_end - 1] == b'-' {
        let ok = match dialect {
            Dialect::Jinja => true,
            Dialect::Go => inner_end >= inner_start + 2 && is_space(bytes[inner_end - 2]),
        };
        if ok {
            inner_end -= 1;
        }
    }
    let kind = if dialect == Dialect::Go && base_kind == ActionKind::Expr {
        go_kind(bytes, inner_start, inner_end)
    } else {
        base_kind
    };
    Action {
        kind,
        start: open,
        end: at + 2,
        inner_start,
        inner_end,
        terminated: true,
    }
}

fn go_kind(bytes: &[u8], from: usize, to: usize) -> ActionKind {
    let inner = &bytes[from..to];
    let word_start = inner
        .iter()
        .position(|b| !is_space(*b))
        .unwrap_or(inner.len());
    let word = &inner[word_start..];
    let word_end = word
        .iter()
        .position(|b| !(b.is_ascii_alphanumeric() || *b == b'_'))
        .unwrap_or(word.len());
    let word = &word[..word_end];
    if GO_CONTROL.iter().any(|c| c.as_bytes() == word) {
        ActionKind::Stmt
    } else {
        ActionKind::Expr
    }
}

/// `{{/* ... */}}` and `{{- /* ... */ -}}`.
fn go_comment(bytes: &[u8], open: usize, inner_start: usize) -> Option<Action> {
    let mut k = inner_start;
    while bytes.get(k).is_some_and(|b| is_space(*b)) {
        k += 1;
    }
    if !bytes[k.min(bytes.len())..].starts_with(b"/*") {
        return None;
    }
    let limit = bytes.len().min(k + MAX_ACTION_BYTES);
    let close = memmem::find(&bytes[k + 2..limit], b"*/")? + k + 2;
    let after = close + 2;
    let mut m = after;
    while bytes.get(m).is_some_and(|b| is_space(*b) || *b == b'-') {
        m += 1;
    }
    if !bytes[m.min(bytes.len())..].starts_with(b"}}") {
        return None;
    }
    Some(Action {
        kind: ActionKind::Comment,
        start: open,
        end: m + 2,
        inner_start: k,
        inner_end: after,
        terminated: true,
    })
}

/// String- and brace-aware search for the closing delimiter; `None` when it is not found.
fn find_closer(
    bytes: &[u8],
    from: usize,
    limit: usize,
    kind: u8,
    dialect: Dialect,
) -> (Option<usize>, usize) {
    let mut j = from;
    let mut depth = 0usize;
    while j < limit {
        let b = bytes[j];
        match b {
            b'"' | b'\'' => {
                // A quoted string ends at its quote or, failing that, at the end of the line.
                let mut k = j + 1;
                while k < limit {
                    match bytes[k] {
                        b'\\' => k += 2,
                        c if c == b => break,
                        b'\n' => break,
                        _ => k += 1,
                    }
                }
                j = if k < limit && bytes[k] == b { k + 1 } else { k };
                continue;
            }
            b'`' if dialect == Dialect::Go => {
                let rest = &bytes[j + 1..limit];
                j = memchr(b'`', rest).map_or(limit, |i| j + 1 + i + 1);
                continue;
            }
            _ => {}
        }
        if dialect == Dialect::Jinja {
            if b == b'{' {
                depth += 1;
                j += 1;
                continue;
            }
            if b == b'}' && depth > 0 {
                depth -= 1;
                j += 1;
                continue;
            }
        }
        let c = closer(kind);
        if b == c[0] && bytes.get(j + 1) == Some(&c[1]) {
            return (Some(j), j - from);
        }
        j += 1;
    }
    (None, j.min(limit) - from)
}

/// Go template text with every action replaced by same-length filler so that the result can be
/// handed to a YAML parser: expressions become `x` (continuation lines of a multi-line action
/// become spaces), control actions and comments become spaces, and a line that holds nothing but
/// control actions, comments or block-inserting expressions (`toYaml`, `include`, `indent`,
/// `nindent`, `tpl`) is blanked completely. Byte length and every newline stay as they were.
#[allow(dead_code)] // called by the wave-2 hook in gotemplate.rs
/// Byte that [`mask`] writes over the first line of an output expression.
pub(crate) const MASK_FILL: u8 = b'x';

pub(crate) fn mask(text: &str) -> String {
    mask_with(text, Dialect::Go)
}

pub(crate) fn mask_with(text: &str, dialect: Dialect) -> String {
    let bytes = text.as_bytes();
    let actions = scan(text, dialect);
    // 0 = plain text, 1 = action that may be blanked with its line, 2 = output expression.
    let mut cover = vec![0u8; bytes.len()];
    for a in &actions {
        let end = a.end.min(bytes.len());
        let blank = match a.kind {
            ActionKind::Stmt | ActionKind::Comment => true,
            ActionKind::Expr => inserts_block(a.inner(text)),
        };
        for c in &mut cover[a.start..end] {
            *c = if blank { 1 } else { 2 };
        }
    }
    let mut out = bytes.to_vec();
    for a in &actions {
        let end = a.end.min(bytes.len());
        let mut first_line = true;
        for b in &mut out[a.start..end] {
            if *b == b'\n' || *b == b'\r' {
                first_line = false;
                continue;
            }
            *b = if a.kind == ActionKind::Expr && first_line {
                MASK_FILL
            } else {
                b' '
            };
        }
    }
    let mut from = 0;
    while from <= bytes.len() {
        let to = memchr(b'\n', &bytes[from..]).map_or(bytes.len(), |i| from + i);
        let mut covered = false;
        let mut blankable = true;
        for k in from..to {
            if is_space(bytes[k]) && cover[k] == 0 {
                continue;
            }
            match cover[k] {
                1 => covered = true,
                _ => {
                    blankable = false;
                    break;
                }
            }
        }
        if covered && blankable {
            for b in &mut out[from..to] {
                if *b != b'\r' {
                    *b = b' ';
                }
            }
        }
        from = to + 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| text.to_owned())
}

fn inserts_block(inner: &str) -> bool {
    inner
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|w| {
            matches!(
                w,
                "toYaml" | "include" | "indent" | "nindent" | "tpl" | "template"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str, d: Dialect) -> Vec<(ActionKind, String, bool)> {
        scan(text, d)
            .iter()
            .map(|a| (a.kind, a.inner(text).trim().to_owned(), a.terminated))
            .collect()
    }

    #[test]
    fn jinja_actions_and_trim_markers() {
        let t = "a {{- x -}} b {%- if y +%} {# c #}";
        let k = kinds(t, Dialect::Jinja);
        assert_eq!(k[0], (ActionKind::Expr, "x".into(), true));
        assert_eq!(k[1], (ActionKind::Stmt, "if y +".into(), true));
        assert_eq!(k[2], (ActionKind::Comment, "c".into(), true));
    }

    #[test]
    fn jinja_strings_and_braces_do_not_end_actions() {
        let t = "{{ '}}' }}{{ {'a': {'b': 1}} }}{% set x = \"%}\" %}";
        let k = kinds(t, Dialect::Jinja);
        assert_eq!(k.len(), 3, "{k:?}");
        assert_eq!(k[0].1, "'}}'");
        assert_eq!(k[1].1, "{'a': {'b': 1}}");
        assert_eq!(k[2].1, "set x = \"%}\"");
        assert!(k.iter().all(|a| a.2));
    }

    #[test]
    fn go_trim_comment_and_negative_numbers() {
        let t = "{{- .A -}}{{/* c */}}{{- /* d */ -}}{{-3}}{{ \"}}\" }}{{ `a}}\nb` }}";
        let k = kinds(t, Dialect::Go);
        assert_eq!(k[0], (ActionKind::Expr, ".A".into(), true));
        assert_eq!(k[1].0, ActionKind::Comment);
        assert_eq!(k[2].0, ActionKind::Comment);
        assert_eq!(k[3].1, "-3");
        assert_eq!(k[4].1, "\"}}\"");
        assert_eq!(k[5].1, "`a}}\nb`");
        assert_eq!(k.len(), 6);
    }

    #[test]
    fn go_control_actions_are_statements() {
        let t = "{{ if .A }}{{ else }}{{ end }}{{ define \"x\" }}{{ include \"x\" . }}";
        let k: Vec<ActionKind> = scan(t, Dialect::Go).iter().map(|a| a.kind).collect();
        assert_eq!(
            k,
            [
                ActionKind::Stmt,
                ActionKind::Stmt,
                ActionKind::Stmt,
                ActionKind::Stmt,
                ActionKind::Expr
            ]
        );
    }

    #[test]
    fn unterminated_actions_end_at_the_line_and_scanning_goes_on() {
        let t = "{{ a\nb {% c %}\n{% x";
        let a = scan(t, Dialect::Jinja);
        assert_eq!(a.len(), 3);
        assert!(!a[0].terminated);
        assert_eq!(&t[a[0].start..a[0].end], "{{ a");
        assert!(a[1].terminated);
        assert!(!a[2].terminated);
        assert!(scan("{{", Dialect::Go).iter().all(|a| !a.terminated));
    }

    #[test]
    fn plain_braces_are_not_actions() {
        assert!(scan("a { b } {c} ${d}", Dialect::Jinja).is_empty());
        assert!(scan("{% x %}", Dialect::Go).is_empty());
    }

    #[test]
    fn lines_are_one_based() {
        let l = Lines::new("a\nbb\n\nc");
        assert_eq!([0, 1, 2, 4, 5, 6].map(|o| l.line(o)), [1, 1, 2, 2, 3, 4]);
    }

    #[test]
    fn mask_keeps_length_and_lines() {
        let t = "a: {{ .Values.x }}\n{{- if .Values.on }}\nb: \"é{{ .Y }}\"\n{{- toYaml .Values.z | nindent 2 }}\n{{- end }}\nc: {{ .A\n  | quote }}\n";
        let m = mask(t);
        assert_eq!(m.len(), t.len());
        assert_eq!(m.matches('\n').count(), t.matches('\n').count());
        let lines: Vec<&str> = m.lines().collect();
        assert_eq!(lines[0], format!("a: {}", "x".repeat(15)));
        assert!(lines[1].trim().is_empty());
        assert_eq!(lines[2], format!("b: \"é{}\"", "x".repeat(8)));
        assert!(lines[3].trim().is_empty());
        assert!(lines[4].trim().is_empty());
        assert!(lines[5].starts_with("c: x"));
        assert!(lines[6].trim().is_empty());
        assert!(m.is_char_boundary(0));
    }

    #[test]
    fn mask_is_total() {
        for t in ["", "{{", "{{ x", "}} {{", "{{\n\n", "é{{é}}é"] {
            let m = mask(t);
            assert_eq!(m.len(), t.len(), "{t:?}");
        }
    }

    #[test]
    fn hostile_input_stays_linear() {
        let t = "{{ '\n".repeat(200_000);
        let started = std::time::Instant::now();
        let a = scan(&t, Dialect::Jinja);
        assert_eq!(a.len(), 200_000);
        assert!(started.elapsed().as_secs() < 5, "{:?}", started.elapsed());
        let t = "{{ x }}{% y %}".repeat(70_000);
        assert_eq!(scan(&t, Dialect::Jinja).len(), 140_000);
    }
}
