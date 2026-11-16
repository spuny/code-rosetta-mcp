//! Parsers: one module per language, all pure and total (docs/design.md section 4).
//!
//! [`detect`] decides the language from the path and the first bytes; [`parse`] turns the text
//! into a [`ParsedFile`]. A parser never resolves anything outside its file: targets outside
//! the file are [`rosetta_model::Ref`]s that the linker resolves after publication.
use rosetta_model::{Language, ParsedFile};

mod bash;
mod dockerfile;
mod gotemplate;
mod hcl;
mod jinja;
mod python;
mod scan;
mod yaml;

/// Bump whenever the output of any parser changes for some input: every stored file is then
/// parsed again on the next synchronization.
pub const PARSER_VERSION: i32 = 2;

/// Bytes of the file head that [`detect`] looks at.
pub const HEAD_BYTES: usize = 8192;

/// The language of a repository file, from its repository-relative `/`-separated `path` and its
/// first bytes (at most [`HEAD_BYTES`] are used). First match wins:
///
/// 1. `Dockerfile`, `Dockerfile.*`, `*.dockerfile`, `Containerfile` -> Dockerfile
/// 2. `.py` -> Python
/// 3. `.tf`, `.tfvars`, `.hcl` -> Hcl
/// 4. `.j2`, `.jinja`, `.jinja2` -> Jinja
/// 5. `.tpl` -> GoTemplate
/// 6. a directory component named `templates`, extension `.yaml`/`.yml`/`.txt`, and `{{` in the
///    head -> GoTemplate
/// 7. `.yaml`, `.yml` -> Yaml
/// 8. `.sh`, `.bash` -> Bash
/// 9. no extension and a first line `#!` naming `sh` or `bash` (directly or through `env`) -> Bash
///
/// Extensions compare case-insensitively; file names (`Dockerfile`) case-sensitively.
pub fn detect(path: &str, head: &[u8]) -> Option<Language> {
    let head = &head[..head.len().min(HEAD_BYTES)];
    let (dirs, name) = match path.rsplit_once('/') {
        Some((dirs, name)) => (dirs, name),
        None => ("", path),
    };
    if name.is_empty() {
        return None;
    }
    let extension = name
        .rsplit_once('.')
        .filter(|(stem, _)| !stem.is_empty())
        .map(|(_, ext)| ext.to_ascii_lowercase());
    if name == "Dockerfile"
        || name.starts_with("Dockerfile.")
        || name == "Containerfile"
        || extension.as_deref() == Some("dockerfile")
    {
        return Some(Language::Dockerfile);
    }
    match extension.as_deref() {
        Some("py") => return Some(Language::Python),
        Some("tf" | "tfvars" | "hcl") => return Some(Language::Hcl),
        Some("j2" | "jinja" | "jinja2") => return Some(Language::Jinja),
        Some("tpl") => return Some(Language::GoTemplate),
        _ => {}
    }
    let in_templates = dirs.split('/').any(|d| d == "templates");
    if in_templates
        && matches!(extension.as_deref(), Some("yaml" | "yml" | "txt"))
        && memchr::memmem::find(head, b"{{").is_some()
    {
        return Some(Language::GoTemplate);
    }
    match extension.as_deref() {
        Some("yaml" | "yml") => Some(Language::Yaml),
        Some("sh" | "bash") => Some(Language::Bash),
        None if shebang_is_shell(head) => Some(Language::Bash),
        _ => None,
    }
}

fn shebang_is_shell(head: &[u8]) -> bool {
    let Some(rest) = head.strip_prefix(b"#!") else {
        return false;
    };
    let line = rest.split(|b| *b == b'\n').next().unwrap_or_default();
    let Ok(line) = std::str::from_utf8(line) else {
        return false;
    };
    let mut words = line.split_whitespace();
    let Some(first) = words.next() else {
        return false;
    };
    let mut program = first.rsplit('/').next().unwrap_or(first);
    if program == "env" {
        program = words.find(|w| !w.starts_with('-')).unwrap_or("");
    }
    matches!(program, "sh" | "bash")
}

/// Parses `text` as `language`. Total: never panics, never fails; the worst case is the File node
/// alone with `parse_error` set. `path` is the repository-relative path (used for the File node's
/// name and for path-dependent rules such as Helm detection).
pub fn parse(language: Language, path: &str, text: &str) -> ParsedFile {
    let run = || match language {
        Language::Python => python::parse(path, text),
        Language::Hcl => hcl::parse(path, text),
        Language::Yaml => yaml::parse(path, text),
        Language::Jinja => jinja::parse(path, text),
        Language::Dockerfile => dockerfile::parse(path, text),
        Language::Bash => bash::parse(path, text),
        Language::GoTemplate => gotemplate::parse(path, text),
    };
    // Parsers are pure functions of their input, so nothing is left half-updated by a panic.
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) {
        Ok(parsed) => parsed,
        Err(_) => ParsedFile::failed(language, path, text, "parser failed on this input"),
    }
}
