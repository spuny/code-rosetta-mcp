//! JSON shapes of the API (docs/design.md section 6). Field names are stable.
use crate::kinds::NodeKind;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A node as the API returns it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeDto {
    pub id: i64,
    pub kind: String,
    pub name: String,
    pub qualified_name: String,
    /// Repository name (GitLab `path_with_namespace`).
    pub repo: String,
    /// Path inside the repository, `/`-separated.
    pub path: String,
    pub line_start: u32,
    pub line_end: u32,
    pub language: String,
    pub parent_name: String,
    pub signature: String,
    pub is_test: bool,
    pub extra: Map<String, Value>,
    /// Published commit of the repository.
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
    pub web_url: Option<String>,
}

/// An edge as the API returns it. `source` and `target` are qualified names; `target` is null
/// while the reference is unresolved, and `target_ref` is `"scheme:value"` for every edge that was
/// emitted as a reference (kept after resolution).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EdgeDto {
    pub id: i64,
    pub kind: String,
    pub source: String,
    pub target: Option<String>,
    pub target_ref: Option<String>,
    pub repo: String,
    /// Path of the file whose parse produced the edge.
    pub path: String,
    pub line: u32,
    pub extra: Map<String, Value>,
}

/// A grep hit or a file hit. `line` and `text` are absent for file hits (`files_only`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HitDto {
    pub repo: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// The matching line, cut to 300 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
    pub web_url: Option<String>,
}

/// `<repo>/<path>` for a File node, `<repo>/<path>::<Kind>::<name>` for every other node.
pub fn qualified_name(repo: &str, path: &str, kind: NodeKind, name: &str) -> String {
    if kind == NodeKind::File {
        format!("{repo}/{path}")
    } else {
        format!("{repo}/{path}::{kind}::{name}")
    }
}

/// `<repo_web_url>/-/blob/<ref>/<path>#L<line>`: the path is percent-encoded per segment, the
/// fragment is left out when `line` is `None` or 0. `None` when the web URL or the ref is unknown.
pub fn web_url(
    repo_web_url: Option<&str>,
    git_ref: Option<&str>,
    path: &str,
    line: Option<u32>,
) -> Option<String> {
    let base = repo_web_url?.trim_end_matches('/');
    let git_ref = git_ref.filter(|r| !r.is_empty())?;
    let encoded: Vec<String> = path.split('/').map(encode_segment).collect();
    let blob = if base.starts_with("https://github.com/") || base.starts_with("http://github.com/")
    {
        "blob"
    } else {
        "-/blob"
    };
    let mut url = format!("{base}/{blob}/{git_ref}/{}", encoded.join("/"));
    if let Some(line) = line.filter(|l| *l > 0) {
        url.push_str(&format!("#L{line}"));
    }
    Some(url)
}

fn encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_links() {
        assert_eq!(
            qualified_name("g/a", "app.py", NodeKind::File, "app.py"),
            "g/a/app.py"
        );
        assert_eq!(
            qualified_name("g/a", "app.py", NodeKind::Method, "C.m"),
            "g/a/app.py::Method::C.m"
        );
        assert_eq!(
            web_url(Some("https://gl/g/a/"), Some("abc"), "d ir/x#.py", Some(3)).as_deref(),
            Some("https://gl/g/a/-/blob/abc/d%20ir/x%23.py#L3")
        );
        assert_eq!(
            web_url(Some("https://gl/g/a"), Some("abc"), "x", Some(0)).as_deref(),
            Some("https://gl/g/a/-/blob/abc/x")
        );
        assert_eq!(
            web_url(
                Some("https://github.com/team/repo"),
                Some("abc"),
                "src/a.py",
                Some(2)
            )
            .as_deref(),
            Some("https://github.com/team/repo/blob/abc/src/a.py#L2")
        );
        assert_eq!(web_url(None, Some("abc"), "x", None), None);
        assert_eq!(web_url(Some("https://gl/g/a"), None, "x", None), None);
    }

    #[test]
    fn hit_shape() {
        let hit = HitDto {
            repo: "g/a".into(),
            path: "x".into(),
            line: None,
            text: None,
            git_ref: None,
            web_url: None,
        };
        assert_eq!(
            serde_json::to_value(&hit).unwrap(),
            serde_json::json!({"repo":"g/a","path":"x","ref":null,"web_url":null})
        );
    }
}
