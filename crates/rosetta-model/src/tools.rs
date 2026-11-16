//! Request and result types of the six tools. MCP (`rosetta-mcp`), the HTTP API and the CLI
//! client all use these, so they live here, next to the DTOs they contain.
//!
//! Requests are what a client sends (MCP `arguments`, HTTP JSON body). Optional fields are
//! `Option`; the tool implementation applies the defaults and caps below and reports a clamped
//! limit in `limit_clamped`. `group` is accepted and ignored (there are no groups any more);
//! `repo_root` is rejected by the implementation with [`REPO_ROOT_REJECTED`].
use crate::dto::{EdgeDto, HitDto, NodeDto};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const SEARCH_NODES_DEFAULT_LIMIT: u32 = 20;
pub const SEARCH_NODES_MAX_LIMIT: u32 = 500;
pub const GREP_DEFAULT_LIMIT: u32 = 100;
/// Cap of `limit` in match mode.
pub const GREP_MAX_MATCHES: u32 = 5000;
/// Cap of `limit` in `files_only` mode.
pub const GREP_MAX_FILES: u32 = 20_000;
/// Byte budget of one grep response (serialized hits).
pub const GREP_MAX_RESPONSE_BYTES: usize = 5_000_000;
/// Characters of a grep match line that are returned.
pub const GREP_MAX_LINE_CHARS: usize = 300;
pub const QUERY_DEFAULT_LIMIT: u32 = 100;
pub const QUERY_MAX_LIMIT: u32 = 1000;
pub const MAX_CHANGED_FILES: usize = 50;
pub const IMPACT_DEFAULT_DEPTH: u32 = 2;
pub const IMPACT_MAX_DEPTH: u32 = 6;
pub const IMPACT_DEFAULT_RESULTS: u32 = 200;
pub const IMPACT_MAX_RESULTS: u32 = 5000;
pub const REVIEW_DEFAULT_LINES: u32 = 200;
pub const REVIEW_MAX_LINES: u32 = 2000;
/// At most this many entries in `sources`.
pub const SOURCES_MAX_ENTRIES: usize = 50;
/// At most this many bytes per entry in `sources`.
pub const SOURCES_MAX_BYTES: usize = 200_000;

pub const REPO_ROOT_REJECTED: &str = "repo_root is not supported: index local paths with the CLI, address indexed files as \"<repo>/<path>\" and filter with `repo`";

/// Outcome of a tool call. `error` results are returned as MCP `isError` results and HTTP 400.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    Ambiguous,
    NotFound,
    Error,
}

/// A failed tool call: `{"status":"error","error":"<message>"}`. Messages never contain secrets.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolError {
    pub status: Status,
    pub error: String,
    /// The service, not the request, failed (database or index unavailable): HTTP answers 503
    /// instead of 400. Not part of the JSON.
    #[serde(skip)]
    pub unavailable: bool,
}

impl ToolError {
    /// A problem with the request.
    pub fn new(error: impl Into<String>) -> Self {
        Self {
            status: Status::Error,
            error: error.into(),
            unavailable: false,
        }
    }

    /// The service cannot answer right now; retrying later may succeed.
    pub fn unavailable(error: impl Into<String>) -> Self {
        Self {
            unavailable: true,
            ..Self::new(error)
        }
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.error)
    }
}

impl std::error::Error for ToolError {}

pub type ToolResult<T> = Result<T, ToolError>;

/// Present on a result when the requested limit exceeded the cap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitClamped {
    pub requested: u32,
    pub applied: u32,
}

// ---------------------------------------------------------------- search_nodes

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchNodesRequest {
    pub query: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    /// Glob on the repository name (`*` stays inside one path segment, `**` crosses segments).
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub repo_root: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SearchNodesResult {
    pub status: Status,
    pub summary: String,
    pub query: String,
    pub results: Vec<NodeDto>,
    /// More nodes matched than `limit`.
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_clamped: Option<LimitClamped>,
}

// ---------------------------------------------------------------- grep_code

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrepCodeRequest {
    pub pattern: String,
    #[serde(default)]
    pub regex: Option<bool>,
    #[serde(default)]
    pub ignore_case: Option<bool>,
    #[serde(default)]
    pub repo: Option<String>,
    /// Glob on `<repo>/<path>` or on the path inside the repository.
    #[serde(default)]
    pub path_glob: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub files_only: Option<bool>,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub repo_root: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GrepCodeResult {
    pub status: Status,
    pub summary: String,
    pub pattern: String,
    pub regex: bool,
    /// `<repo>/<path>` of every file with a hit, in result order.
    pub files: Vec<String>,
    /// Something was left out (limit or byte budget).
    pub truncated: bool,
    /// Match mode only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matches: Option<Vec<HitDto>>,
    /// `files_only` mode only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_hits: Option<Vec<HitDto>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_clamped: Option<LimitClamped>,
}

// ---------------------------------------------------------------- query_graph

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryGraphRequest {
    /// One of [`crate::QueryPattern`]'s names.
    pub pattern: String,
    /// Qualified name, `<repo>/<path>` of a file, or a node name (resolved through search).
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub repo_root: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QueryGraphResult {
    pub status: Status,
    pub summary: String,
    pub pattern: String,
    /// The resolved target's qualified name (or the target as given when not resolved).
    pub target: Option<String>,
    pub description: String,
    pub results: Vec<NodeDto>,
    pub edges: Vec<EdgeDto>,
    /// Status `ambiguous`: the candidates to choose from.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<NodeDto>,
    pub truncated: bool,
    /// `cross_language` only: number of cross-language edges in scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_edges: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_clamped: Option<LimitClamped>,
}

// ---------------------------------------------------------------- get_impact_radius

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImpactRadiusRequest {
    /// `<repo>/<path>` entries (a bare path is accepted when it matches exactly one repository).
    pub changed_files: Vec<String>,
    #[serde(default)]
    pub max_depth: Option<u32>,
    #[serde(default)]
    pub max_results: Option<u32>,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub repo_root: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ImpactRadiusResult {
    pub status: Status,
    pub summary: String,
    /// Resolved entries as `<repo>/<path>`.
    pub changed_files: Vec<String>,
    /// Entries that are not indexed (for example files added in a merge request).
    pub unknown_files: Vec<String>,
    pub changed_nodes: Vec<NodeDto>,
    pub impacted_nodes: Vec<NodeDto>,
    /// `<repo>/<path>`, sorted.
    pub impacted_files: Vec<String>,
    pub edges: Vec<EdgeDto>,
    pub truncated: bool,
    pub total_impacted: u64,
}

// ---------------------------------------------------------------- get_review_context

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewContextRequest {
    pub changed_files: Vec<String>,
    #[serde(default)]
    pub max_depth: Option<u32>,
    #[serde(default)]
    pub include_source: Option<bool>,
    #[serde(default)]
    pub max_lines_per_file: Option<u32>,
    /// The caller's own text of changed files (`"<repo>/<path>"` -> text), used for snippets
    /// instead of the indexed master version. At most 50 entries of 200 kB.
    #[serde(default)]
    pub sources: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub repo_root: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReviewGraph {
    pub changed_nodes: Vec<NodeDto>,
    pub impacted_nodes: Vec<NodeDto>,
    pub edges: Vec<EdgeDto>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReviewContext {
    pub changed_files: Vec<String>,
    pub unknown_files: Vec<String>,
    pub impacted_files: Vec<String>,
    pub graph: ReviewGraph,
    /// Entry as supplied -> numbered lines (`"<n>: <line>"` joined by `\n`, with
    /// `"\n... (<k> more lines)"` when cut), or a parenthesised note when not available.
    pub source_snippets: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReviewContextResult {
    pub status: Status,
    pub summary: String,
    pub context: ReviewContext,
    pub truncated: bool,
}

// ---------------------------------------------------------------- list_graph_stats

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphStatsRequest {
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub repo_root: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphStatsResult {
    pub status: Status,
    pub summary: String,
    pub repositories: u64,
    pub files_count: u64,
    pub total_nodes: u64,
    pub total_edges: u64,
    pub unresolved_edges: u64,
    pub nodes_by_kind: BTreeMap<String, u64>,
    pub edges_by_kind: BTreeMap<String, u64>,
    /// Sorted.
    pub languages: Vec<String>,
    /// RFC 3339 time of the newest publication in scope.
    pub last_updated: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_accept_minimal_and_legacy_arguments() {
        let r: GrepCodeRequest = serde_json::from_str(r#"{"pattern":"x"}"#).unwrap();
        assert_eq!((r.pattern.as_str(), r.regex, r.limit), ("x", None, None));
        let r: ImpactRadiusRequest =
            serde_json::from_str(r#"{"changed_files":["g/a/x.py"],"group":"old"}"#).unwrap();
        assert_eq!(r.group.as_deref(), Some("old"));
        assert_eq!(
            serde_json::to_value(ToolError::new("bad")).unwrap(),
            serde_json::json!({"status":"error","error":"bad"})
        );
        assert_eq!(serde_json::to_value(Status::NotFound).unwrap(), "not_found");
    }
}
