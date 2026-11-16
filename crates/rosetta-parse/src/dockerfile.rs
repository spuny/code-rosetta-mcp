//! Dockerfile parser (owner P5). Contract: docs/design.md section 4, "Per-language output".
//!
//! A line scanner: physical lines are joined into instructions (continuations, comment lines
//! inside a continuation, heredoc bodies), then each instruction is interpreted. Nothing here can
//! fail; unknown or malformed lines are skipped.
use regex::Regex;
use rosetta_model::{Edge, EdgeKind, Language, Node, NodeKind, ParsedFile, RefScheme};
use std::collections::HashMap;
use std::sync::LazyLock;

/// One logical instruction: keyword (uppercased), the rest of the text, and its line range.
struct Instruction {
    keyword: String,
    args: String,
    start: u32,
    end: u32,
}

struct Stage {
    index: u32,
    entrypoint: Option<(String, u32)>,
    cmd: Option<(String, u32)>,
    last_line: u32,
}

/// Parses one file. Must be total: never panic, bound recursion, stay fast on 1 MiB input.
pub(crate) fn parse(path: &str, text: &str) -> ParsedFile {
    let mut out = ParsedFile::new(Language::Dockerfile, path, text);
    let mut builder = Builder::default();
    for instruction in instructions(text) {
        builder.handle(&instruction, &mut out);
    }
    builder.finish_stage(&mut out);
    out
}

#[derive(Default)]
struct Builder {
    args: HashMap<String, String>,
    stage: Option<Stage>,
    stage_count: u32,
    aliases: HashMap<String, u32>,
    by_index: HashMap<String, u32>,
}

impl Builder {
    fn handle(&mut self, i: &Instruction, out: &mut ParsedFile) {
        match i.keyword.as_str() {
            "ARG" if self.stage.is_none() => self.global_arg(&i.args),
            "FROM" => self.from(i, out),
            "COPY" | "ADD" => self.copy(i, out),
            "EXPOSE" => self.expose(i, out),
            "ENTRYPOINT" => {
                if let Some(stage) = self.stage.as_mut() {
                    stage.entrypoint = Some((i.args.clone(), i.start));
                }
            }
            "CMD" => {
                if let Some(stage) = self.stage.as_mut() {
                    stage.cmd = Some((i.args.clone(), i.start));
                }
            }
            _ => {}
        }
        if let Some(stage) = self.stage.as_mut() {
            stage.last_line = stage.last_line.max(i.end);
        }
    }

    fn global_arg(&mut self, args: &str) {
        for token in tokens(args) {
            if let Some((name, value)) = token.split_once('=') {
                self.args.insert(name.to_owned(), unquote(value));
            }
        }
    }

    fn from(&mut self, i: &Instruction, out: &mut ParsedFile) {
        self.finish_stage(out);
        let mut platform = None;
        let mut words = Vec::new();
        for token in tokens(&i.args) {
            if let Some(value) = token.strip_prefix("--platform=") {
                platform = Some(value.to_owned());
            } else if !token.starts_with("--") {
                words.push(token);
            }
        }
        let Some(raw_image) = words.first() else {
            return;
        };
        let alias = match words.as_slice() {
            [_, keyword, alias, ..] if keyword.eq_ignore_ascii_case("as") => Some(alias.clone()),
            _ => None,
        };
        let image_ref = substitute(raw_image, &self.args);
        let (image, tag, digest) = split_image(&image_ref);
        let name = alias.clone().unwrap_or_else(|| match &tag {
            Some(tag) => format!("{image}:{tag}"),
            None => image.clone(),
        });
        let index = self.stage_count;
        let mut node = Node::new(NodeKind::Stage, name, 0, i.start, i.end)
            .extra("image", image.as_str())
            .extra("index", index);
        if let Some(tag) = &tag {
            node = node.extra("tag", tag.as_str());
        }
        if let Some(digest) = &digest {
            node = node.extra("digest", digest.as_str());
        }
        if let Some(alias) = &alias {
            node = node.extra("alias", alias.as_str());
        }
        if let Some(platform) = &platform {
            node = node.extra("platform", platform.as_str());
        }
        let node_index = out.add_node(node);
        if let Some(parent) = self.aliases.get(&image_ref.to_ascii_lowercase()) {
            out.add_edge(Edge::local(
                EdgeKind::DependsOn,
                node_index,
                *parent,
                i.start,
            ));
        }
        if let Some(alias) = alias {
            self.aliases
                .entry(alias.to_ascii_lowercase())
                .or_insert(node_index);
        }
        self.aliases.entry(index.to_string()).or_insert(node_index);
        self.stage_count += 1;
        self.stage = Some(Stage {
            index: node_index,
            entrypoint: None,
            cmd: None,
            last_line: i.end,
        });
    }

    fn copy(&mut self, i: &Instruction, out: &mut ParsedFile) {
        let Some(stage) = self.stage.as_ref() else {
            return;
        };
        let stage_index = stage.index;
        let (flags, paths) = split_flags(&i.args);
        let from = flags
            .iter()
            .find_map(|f| f.strip_prefix("--from=").map(str::to_owned));
        if let Some(from) = from {
            if let Some(parent) = self
                .aliases
                .get(&from.to_ascii_lowercase())
                .or_else(|| self.by_index.get(&from))
            {
                out.add_edge(Edge::local(
                    EdgeKind::DependsOn,
                    stage_index,
                    *parent,
                    i.start,
                ));
            }
            return;
        }
        if paths.len() < 2 || paths.iter().any(|p| p.contains("<<")) {
            return;
        }
        for source in &paths[..paths.len() - 1] {
            if plain_relative_path(source) {
                out.add_edge(
                    Edge::to_ref(
                        EdgeKind::References,
                        stage_index,
                        RefScheme::File,
                        source.as_str(),
                        i.start,
                    )
                    .extra("instruction", i.keyword.as_str()),
                );
            }
        }
    }

    fn expose(&mut self, i: &Instruction, out: &mut ParsedFile) {
        let Some(stage) = self.stage.as_ref() else {
            return;
        };
        let stage_node = stage.index;
        let stage_name = out.nodes[stage_node as usize].name.clone();
        for token in tokens(&i.args) {
            let (port, protocol) = match token.split_once('/') {
                Some((port, protocol)) => (port.to_owned(), protocol.to_ascii_lowercase()),
                None => (token.clone(), "tcp".to_owned()),
            };
            if port.is_empty() {
                continue;
            }
            let name = if protocol == "tcp" {
                format!("{stage_name}:{port}")
            } else {
                format!("{stage_name}:{port}/{protocol}")
            };
            out.add_node(
                Node::new(NodeKind::Port, name, stage_node, i.start, i.end)
                    .extra("port", port)
                    .extra("protocol", protocol),
            );
        }
    }

    /// Emits the Entrypoint node of the stage that just ended and extends the stage's line range.
    fn finish_stage(&mut self, out: &mut ParsedFile) {
        let Some(stage) = self.stage.take() else {
            return;
        };
        let stage_name = out.nodes[stage.index as usize].name.clone();
        let (kind, (command, line)) = match (stage.entrypoint, stage.cmd) {
            (Some(entry), _) => ("ENTRYPOINT", entry),
            (None, Some(cmd)) => ("CMD", cmd),
            (None, None) => {
                set_stage_end(out, stage.index, stage.last_line);
                return;
            }
        };
        let form = if command.trim_start().starts_with('[') {
            "exec"
        } else {
            "shell"
        };
        out.add_node(
            Node::new(NodeKind::Entrypoint, stage_name, stage.index, line, line)
                .extra("kind", kind)
                .extra("command", command.trim())
                .extra("form", form),
        );
        set_stage_end(out, stage.index, stage.last_line);
    }
}

fn set_stage_end(out: &mut ParsedFile, index: u32, last_line: u32) {
    let node = &mut out.nodes[index as usize];
    node.line_end = node.line_end.max(last_line);
}

/// Joins physical lines into instructions.
fn instructions(text: &str) -> Vec<Instruction> {
    let lines: Vec<&str> = text.lines().collect();
    let mut escape = '\\';
    let mut position = 0;
    // Parser directives: `# key=value` comments before anything else.
    while let Some(line) = lines.get(position) {
        let Some(caps) = DIRECTIVE.captures(line.trim()) else {
            break;
        };
        if caps[1].eq_ignore_ascii_case("escape") && matches!(&caps[2], "`" | "\\") {
            escape = caps[2].chars().next().unwrap_or('\\');
        }
        position += 1;
    }
    let mut out = Vec::new();
    while position < lines.len() {
        let trimmed = lines[position].trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            position += 1;
            continue;
        }
        let start = position;
        let mut joined = String::new();
        loop {
            let part = lines[position].trim_end();
            position += 1;
            if let Some(stripped) = part.strip_suffix(escape) {
                joined.push_str(stripped);
                joined.push(' ');
                // Comment and blank lines inside a continuation are dropped.
                while position < lines.len() {
                    let next = lines[position].trim();
                    if next.is_empty() || next.starts_with('#') {
                        position += 1;
                    } else {
                        break;
                    }
                }
                if position >= lines.len() {
                    break;
                }
            } else {
                joined.push_str(part);
                break;
            }
        }
        for terminator in heredoc_terminators(&joined) {
            while position < lines.len() {
                let body = lines[position];
                position += 1;
                let candidate = if terminator.1 {
                    body.trim_start_matches('\t')
                } else {
                    body
                };
                if candidate.trim_end() == terminator.0 {
                    break;
                }
            }
        }
        let joined = joined.trim();
        let (keyword, args) = match joined.split_once(char::is_whitespace) {
            Some((keyword, args)) => (keyword, args.trim()),
            None => (joined, ""),
        };
        if !keyword.is_empty() && keyword.chars().all(|c| c.is_ascii_alphabetic()) {
            out.push(Instruction {
                keyword: keyword.to_ascii_uppercase(),
                args: args.to_owned(),
                start: u32::try_from(start + 1).unwrap_or(u32::MAX),
                end: u32::try_from(position).unwrap_or(u32::MAX),
            });
        }
    }
    out
}

static DIRECTIVE: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::expect_used)]
    Regex::new(r"^#\s*([A-Za-z]+)\s*=\s*(\S+)\s*$").expect("constant pattern")
});

static HEREDOC: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::expect_used)]
    Regex::new(r#"(?:^|\s)<<(-?)(["']?)([A-Za-z_][A-Za-z0-9_]*)(["']?)"#).expect("constant pattern")
});

static VARIABLE: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::expect_used)]
    Regex::new(r"\$(?:\{([A-Za-z_][A-Za-z0-9_]*)(?::?-([^}]*))?\}|([A-Za-z_][A-Za-z0-9_]*))")
        .expect("constant pattern")
});

/// `(terminator, strip leading tabs)` for each heredoc opened on the logical line.
fn heredoc_terminators(line: &str) -> Vec<(String, bool)> {
    let keyword = line
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    if !matches!(keyword.as_str(), "RUN" | "COPY" | "ADD") {
        return Vec::new();
    }
    HEREDOC
        .captures_iter(line)
        .map(|c| (c[3].to_owned(), &c[1] == "-"))
        .collect()
}

/// Splits on whitespace, keeping quoted parts together; a JSON array becomes its elements.
fn tokens(text: &str) -> Vec<String> {
    let text = text.trim();
    if text.starts_with('[')
        && let Ok(list) = serde_json::from_str::<Vec<String>>(text)
    {
        return list;
    }
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for c in text.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => current.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                started = true;
            }
            None if c.is_whitespace() => {
                if started || !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            None => current.push(c),
        }
    }
    if started || !current.is_empty() {
        out.push(current);
    }
    out
}

fn unquote(value: &str) -> String {
    value.trim_matches(['"', '\'']).to_owned()
}

/// `(flags, remaining tokens)` where flags are the leading `--name[=value]` tokens.
fn split_flags(args: &str) -> (Vec<String>, Vec<String>) {
    let all = tokens(args);
    let split = all.iter().take_while(|t| t.starts_with("--")).count();
    let (flags, rest) = all.split_at(split);
    (flags.to_vec(), rest.to_vec())
}

/// `$VAR` and `${VAR}` / `${VAR:-default}` replaced from the global ARG defaults; unknown
/// variables stay as written.
fn substitute(text: &str, args: &HashMap<String, String>) -> String {
    VARIABLE
        .replace_all(text, |caps: &regex::Captures| {
            let name = caps
                .get(1)
                .or_else(|| caps.get(3))
                .map_or("", |m| m.as_str());
            match args.get(name) {
                Some(value) if !value.is_empty() => value.clone(),
                _ => caps
                    .get(2)
                    .map_or_else(|| caps[0].to_owned(), |m| m.as_str().to_owned()),
            }
        })
        .into_owned()
}

/// `(image, tag, digest)` of an image reference (a registry port is not a tag).
fn split_image(reference: &str) -> (String, Option<String>, Option<String>) {
    let (rest, digest) = match reference.split_once('@') {
        Some((rest, digest)) => (rest, Some(digest.to_owned())),
        None => (reference, None),
    };
    let slash = rest.rfind('/').map_or(0, |i| i + 1);
    match rest[slash..].rfind(':') {
        Some(colon) => {
            let at = slash + colon;
            (
                rest[..at].to_owned(),
                Some(rest[at + 1..].to_owned()),
                digest,
            )
        }
        None => (rest.to_owned(), None, digest),
    }
}

/// A COPY/ADD source that names a path inside the build context: not a glob, URL, variable,
/// absolute path or the context root itself.
fn plain_relative_path(path: &str) -> bool {
    !(path.is_empty()
        || path.starts_with('/')
        || path.contains("://")
        || path.starts_with("git@")
        || path.contains(['*', '?', '[', '$'])
        || matches!(path, "." | "./" | ".." | "../"))
}
