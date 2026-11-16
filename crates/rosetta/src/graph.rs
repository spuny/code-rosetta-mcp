use crate::storage::{Repository, Snapshot};
use globset::{GlobBuilder, GlobMatcher};
use regex::RegexBuilder;
use rosetta_model::{
    EdgeDto, NodeDto, NodeKind, QueryPattern, Target, qualified_name, tokenize, tools::*,
};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

pub struct Graph {
    pub revision: i64,
    pub repositories: BTreeMap<String, Repository>,
    pub nodes: Vec<NodeDto>,
    pub edges: Vec<EdgeDto>,
    pub file_ids: HashMap<String, i64>,
    pub by_qualified: HashMap<String, usize>,
}

fn glob(pattern: Option<&str>) -> ToolResult<Option<GlobMatcher>> {
    pattern
        .map(|p| {
            if p.len() > 4096 {
                return Err(ToolError::new("glob exceeds 4096 bytes"));
            }
            GlobBuilder::new(p)
                .literal_separator(true)
                .build()
                .map(|g| g.compile_matcher())
                .map_err(|_| ToolError::new("invalid glob"))
        })
        .transpose()
}

fn matches(glob: &Option<GlobMatcher>, text: &str) -> bool {
    glob.as_ref().is_none_or(|g| g.is_match(text))
}

fn cap(
    requested: Option<u32>,
    default: u32,
    maximum: u32,
) -> ToolResult<(usize, Option<LimitClamped>)> {
    let requested = requested.unwrap_or(default);
    if requested == 0 {
        return Err(ToolError::new("limit must be at least 1"));
    }
    let applied = requested.min(maximum);
    Ok((
        applied as usize,
        (requested > maximum).then_some(LimitClamped { requested, applied }),
    ))
}

pub fn legacy(repo_root: &Option<String>) -> ToolResult<()> {
    if repo_root.is_some() {
        return Err(ToolError::new(REPO_ROOT_REJECTED));
    }
    Ok(())
}

impl Graph {
    pub fn new(snapshot: Snapshot) -> Self {
        let mut graph = Self {
            revision: snapshot.revision,
            repositories: snapshot.repositories,
            nodes: Vec::new(),
            edges: Vec::new(),
            file_ids: HashMap::new(),
            by_qualified: HashMap::new(),
        };
        for repo in graph.repositories.values() {
            for (path, file) in &repo.files {
                let base = graph.nodes.len();
                graph
                    .file_ids
                    .insert(format!("{}/{path}", repo.name), (base + 1) as i64);
                if let Some(parsed) = &file.parsed {
                    for node in &parsed.nodes {
                        graph.nodes.push(NodeDto {
                            id: (graph.nodes.len() + 1) as i64,
                            kind: node.kind.to_string(),
                            name: node.name.clone(),
                            qualified_name: qualified_name(&repo.name, path, node.kind, &node.name),
                            repo: repo.name.clone(),
                            path: path.clone(),
                            line_start: node.line_start,
                            line_end: node.line_end,
                            language: parsed.language.to_string(),
                            parent_name: node
                                .parent
                                .and_then(|i| parsed.nodes.get(i as usize))
                                .map_or_else(String::new, |n| n.name.clone()),
                            signature: node.signature.clone(),
                            is_test: node.is_test,
                            extra: node.extra.clone(),
                            git_ref: repo.git_ref.clone(),
                            web_url: rosetta_model::web_url(
                                repo.web_url.as_deref(),
                                repo.git_ref.as_deref(),
                                path,
                                Some(node.line_start),
                            ),
                        });
                    }
                    for (i, node) in parsed.nodes.iter().enumerate() {
                        if let Some(parent) = node.parent {
                            graph.edges.push(EdgeDto {
                                id: (graph.edges.len() + 1) as i64,
                                kind: "CONTAINS".into(),
                                source: graph.nodes[base + parent as usize].qualified_name.clone(),
                                target: Some(graph.nodes[base + i].qualified_name.clone()),
                                target_ref: None,
                                repo: repo.name.clone(),
                                path: path.clone(),
                                line: node.line_start,
                                extra: Map::new(),
                            });
                        }
                    }
                    for edge in &parsed.edges {
                        let (target, target_ref) = match &edge.target {
                            Target::Local(i) => (
                                graph
                                    .nodes
                                    .get(base + *i as usize)
                                    .map(|n| n.qualified_name.clone()),
                                None,
                            ),
                            Target::Ref(r) => (None, Some(r.to_string())),
                        };
                        graph.edges.push(EdgeDto {
                            id: (graph.edges.len() + 1) as i64,
                            kind: edge.kind.to_string(),
                            source: graph.nodes[base + edge.source as usize]
                                .qualified_name
                                .clone(),
                            target,
                            target_ref,
                            repo: repo.name.clone(),
                            path: path.clone(),
                            line: edge.line,
                            extra: edge.extra.clone(),
                        });
                    }
                } else {
                    let name = path.rsplit('/').next().unwrap_or(path);
                    graph.nodes.push(NodeDto {
                        id: (base + 1) as i64,
                        kind: "File".into(),
                        name: name.into(),
                        qualified_name: format!("{}/{path}", repo.name),
                        repo: repo.name.clone(),
                        path: path.clone(),
                        line_start: u32::from(file.text.is_some()),
                        line_end: file.text.as_ref().map_or(0, |t| t.lines().count() as u32),
                        language: "text".into(),
                        parent_name: String::new(),
                        signature: String::new(),
                        is_test: false,
                        extra: Map::new(),
                        git_ref: repo.git_ref.clone(),
                        web_url: rosetta_model::web_url(
                            repo.web_url.as_deref(),
                            repo.git_ref.as_deref(),
                            path,
                            None,
                        ),
                    });
                }
            }
        }
        graph.by_qualified = graph
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.qualified_name.clone(), i))
            .collect();
        crate::linker::resolve(&mut graph);
        graph
    }

    pub fn search(&self, request: SearchNodesRequest) -> ToolResult<SearchNodesResult> {
        legacy(&request.repo_root)?;
        let query = request.query.trim();
        if query.is_empty() || query.len() > 4096 {
            return Err(ToolError::new("query must contain 1..4096 bytes"));
        }
        if let Some(kind) = &request.kind {
            kind.parse::<NodeKind>()
                .map_err(|_| ToolError::new("unknown node kind"))?;
        }
        let (limit, limit_clamped) = cap(
            request.limit,
            SEARCH_NODES_DEFAULT_LIMIT,
            SEARCH_NODES_MAX_LIMIT,
        )?;
        let repo = glob(request.repo.as_deref())?;
        let wanted = tokenize(query);
        let mut ranked: Vec<_> = self
            .nodes
            .iter()
            .filter(|n| {
                matches(&repo, &n.repo)
                    && request.kind.as_ref().is_none_or(|k| k == &n.kind)
                    && request.language.as_ref().is_none_or(|l| l == &n.language)
            })
            .filter_map(|n| {
                let text = format!(
                    "{} {} {}",
                    n.name,
                    n.signature,
                    serde_json::to_string(&n.extra).unwrap_or_default()
                );
                let tokens = tokenize(&text);
                let exact = n.name == query;
                let insensitive = n.name.eq_ignore_ascii_case(query);
                (exact
                    || insensitive
                    || (!wanted.is_empty()
                        && wanted
                            .iter()
                            .all(|w| tokens.iter().any(|t| t.starts_with(w)))))
                .then_some((
                    if exact {
                        0
                    } else if insensitive {
                        1
                    } else {
                        2
                    },
                    n,
                ))
            })
            .collect();
        ranked.sort_by(|(a, x), (b, y)| a.cmp(b).then(x.qualified_name.cmp(&y.qualified_name)));
        let truncated = ranked.len() > limit;
        let results: Vec<_> = ranked
            .into_iter()
            .take(limit)
            .map(|(_, n)| n.clone())
            .collect();
        Ok(SearchNodesResult {
            status: Status::Ok,
            summary: format!("{} matching nodes", results.len()),
            query: request.query,
            results,
            truncated,
            limit_clamped,
        })
    }

    pub fn grep(&self, request: GrepCodeRequest) -> ToolResult<GrepCodeResult> {
        legacy(&request.repo_root)?;
        if request.pattern.is_empty() || request.pattern.len() > 4096 {
            return Err(ToolError::new("pattern must contain 1..4096 bytes"));
        }
        let regex_mode = request.regex.unwrap_or(false);
        let pattern = if regex_mode {
            request.pattern.clone()
        } else {
            regex::escape(&request.pattern)
        };
        let regex = RegexBuilder::new(&pattern)
            .case_insensitive(request.ignore_case.unwrap_or(false))
            .size_limit(2_000_000)
            .dfa_size_limit(2_000_000)
            .build()
            .map_err(|_| ToolError::new("invalid or oversized regular expression"))?;
        let files_only = request.files_only.unwrap_or(false);
        let maximum = if files_only {
            GREP_MAX_FILES
        } else {
            GREP_MAX_MATCHES
        };
        let (limit, limit_clamped) = cap(request.limit, GREP_DEFAULT_LIMIT, maximum)?;
        let repo_filter = glob(request.repo.as_deref())?;
        let path_filter = glob(request.path_glob.as_deref())?;
        let mut hits = Vec::new();
        let mut files = BTreeSet::new();
        let mut bytes = 0;
        let mut truncated = false;
        'scan: for repo in self
            .repositories
            .values()
            .filter(|r| matches(&repo_filter, &r.name))
        {
            for (path, file) in &repo.files {
                let qualified = format!("{}/{path}", repo.name);
                if !matches(&path_filter, path) && !matches(&path_filter, &qualified) {
                    continue;
                }
                let Some(text) = &file.text else { continue };
                for (i, line) in text.lines().enumerate().filter(|(_, l)| regex.is_match(l)) {
                    let hit = rosetta_model::HitDto {
                        repo: repo.name.clone(),
                        path: path.clone(),
                        line: (!files_only).then_some((i + 1) as u32),
                        text: (!files_only)
                            .then(|| line.chars().take(GREP_MAX_LINE_CHARS).collect()),
                        git_ref: repo.git_ref.clone(),
                        web_url: rosetta_model::web_url(
                            repo.web_url.as_deref(),
                            repo.git_ref.as_deref(),
                            path,
                            (!files_only).then_some((i + 1) as u32),
                        ),
                    };
                    let size = serde_json::to_vec(&hit).map_or(0, |s| s.len());
                    if hits.len() == limit || bytes + size > GREP_MAX_RESPONSE_BYTES {
                        truncated = true;
                        break 'scan;
                    }
                    bytes += size;
                    hits.push(hit);
                    files.insert(qualified.clone());
                    if files_only {
                        break;
                    }
                }
            }
        }
        Ok(GrepCodeResult {
            status: Status::Ok,
            summary: format!("{} hits in {} files", hits.len(), files.len()),
            pattern: request.pattern,
            regex: regex_mode,
            files: files.into_iter().collect(),
            truncated,
            matches: (!files_only).then(|| hits.clone()),
            file_hits: files_only.then_some(hits),
            limit_clamped,
        })
    }

    fn node(&self, name: &str) -> Option<&NodeDto> {
        self.by_qualified.get(name).map(|i| &self.nodes[*i])
    }

    pub fn query(&self, request: QueryGraphRequest) -> ToolResult<QueryGraphResult> {
        legacy(&request.repo_root)?;
        let pattern: QueryPattern = request
            .pattern
            .parse()
            .map_err(|_| ToolError::new("unknown query pattern"))?;
        let (limit, limit_clamped) = cap(request.limit, QUERY_DEFAULT_LIMIT, QUERY_MAX_LIMIT)?;
        let repo = glob(request.repo.as_deref())?;
        let mut result = QueryGraphResult {
            status: Status::Ok,
            summary: String::new(),
            pattern: request.pattern,
            target: request.target.clone(),
            description: pattern.description().into(),
            results: Vec::new(),
            edges: Vec::new(),
            candidates: Vec::new(),
            truncated: false,
            total_edges: None,
            limit_clamped,
        };
        if pattern == QueryPattern::CrossLanguage {
            let edges: Vec<_> = self
                .edges
                .iter()
                .filter(|e| matches(&repo, &e.repo))
                .filter(|e| {
                    e.target
                        .as_deref()
                        .and_then(|t| self.node(t))
                        .zip(self.node(&e.source))
                        .is_some_and(|(a, b)| a.language != b.language)
                })
                .collect();
            result.total_edges = Some(edges.len() as u64);
            result.truncated = edges.len() > limit;
            result.edges = edges.into_iter().take(limit).cloned().collect();
            result.summary = format!("{} cross-language edges", result.total_edges.unwrap_or(0));
            return Ok(result);
        }
        let target = request
            .target
            .as_deref()
            .filter(|t| !t.trim().is_empty() && t.len() <= 4096)
            .ok_or_else(|| ToolError::new("a nonempty target of at most 4096 bytes is required"))?;
        let exact = self.node(target).filter(|n| matches(&repo, &n.repo));
        let mut candidates: Vec<_> = exact.into_iter().collect();
        if candidates.is_empty() {
            candidates = self
                .nodes
                .iter()
                .filter(|n| {
                    matches(&repo, &n.repo)
                        && (n.name == target || (n.kind == "File" && n.path == target))
                })
                .collect();
        }
        if pattern == QueryPattern::FileSummary {
            candidates.retain(|n| n.kind == "File");
        }
        if candidates.len() != 1 {
            result.status = if candidates.is_empty() {
                Status::NotFound
            } else {
                Status::Ambiguous
            };
            result.summary = if candidates.is_empty() {
                "target was not found".into()
            } else {
                "target is ambiguous; use a qualified name".into()
            };
            result.candidates = candidates.into_iter().take(20).cloned().collect();
            return Ok(result);
        }
        let node = candidates[0];
        result.target = Some(node.qualified_name.clone());
        if pattern == QueryPattern::FileSummary {
            let mut nodes: Vec<_> = self
                .nodes
                .iter()
                .filter(|n| n.repo == node.repo && n.path == node.path)
                .collect();
            nodes.sort_by_key(|n| (n.line_start, n.id));
            result.truncated = nodes.len() > limit;
            result.results = nodes.into_iter().take(limit).cloned().collect();
        } else {
            let mut found = BTreeSet::new();
            let mut edges = Vec::new();
            let mut scope = HashSet::from([node.qualified_name.clone()]);
            if pattern == QueryPattern::ImportsOf {
                for _ in 0..8 {
                    let children: Vec<_> = self
                        .edges
                        .iter()
                        .filter(|e| e.kind == "CONTAINS" && scope.contains(&e.source))
                        .filter_map(|e| e.target.clone())
                        .collect();
                    let before = scope.len();
                    scope.extend(children);
                    if scope.len() == before {
                        break;
                    }
                }
            }
            for edge in &self.edges {
                let outgoing = edge.source == node.qualified_name;
                let incoming = edge.target.as_deref() == Some(&node.qualified_name);
                let source_file = self
                    .node(&edge.source)
                    .is_some_and(|n| n.repo == node.repo && n.path == node.path);
                let target_file = edge
                    .target
                    .as_deref()
                    .and_then(|t| self.node(t))
                    .is_some_and(|n| n.repo == node.repo && n.path == node.path);
                let (keep, forward) = match pattern {
                    QueryPattern::CallersOf => (
                        incoming && matches!(edge.kind.as_str(), "CALLS" | "CALLS_MACRO"),
                        false,
                    ),
                    QueryPattern::CalleesOf => (
                        outgoing && matches!(edge.kind.as_str(), "CALLS" | "CALLS_MACRO"),
                        true,
                    ),
                    QueryPattern::ImportsOf => {
                        (scope.contains(&edge.source) && edge.kind == "IMPORTS", true)
                    }
                    QueryPattern::ImportersOf => {
                        (target_file && !source_file && edge.kind == "IMPORTS", false)
                    }
                    QueryPattern::ChildrenOf => (outgoing && edge.kind == "CONTAINS", true),
                    QueryPattern::InheritorsOf => (incoming && edge.kind == "INHERITS", false),
                    QueryPattern::ReferencesTo => (
                        incoming
                            && matches!(
                                edge.kind.as_str(),
                                "REFERENCES"
                                    | "READS_CONFIG"
                                    | "RENDERS"
                                    | "PASSES_VAR"
                                    | "USES_MODULE"
                                    | "USES_VARIABLE"
                                    | "EXTENDS"
                                    | "INCLUDES"
                                    | "DEPENDS_ON"
                                    | "REMOTE_STATE"
                            ),
                        false,
                    ),
                    QueryPattern::TestsFor => (
                        incoming
                            && matches!(edge.kind.as_str(), "CALLS" | "IMPORTS")
                            && self.node(&edge.source).is_some_and(|n| n.is_test),
                        false,
                    ),
                    _ => (false, false),
                };
                if keep {
                    if let Some(name) = if forward {
                        edge.target.as_ref()
                    } else {
                        Some(&edge.source)
                    } {
                        found.insert(name.clone());
                    }
                    edges.push(edge);
                }
            }
            if pattern == QueryPattern::TestsFor {
                for n in self.nodes.iter().filter(|n| {
                    let wanted = format!(
                        "test_{}",
                        node.name.rsplit('.').next().unwrap_or(&node.name)
                    );
                    n.is_test
                        && n.repo == node.repo
                        && (n.name == wanted || n.name.ends_with(&format!(".{wanted}")))
                }) {
                    found.insert(n.qualified_name.clone());
                }
            }
            result.truncated = found.len() > limit || edges.len() > limit;
            result.results = found
                .into_iter()
                .take(limit)
                .filter_map(|n| self.node(&n).cloned())
                .collect();
            result.edges = edges.into_iter().take(limit).cloned().collect();
        }
        result.summary = format!(
            "{} nodes, {} edges",
            result.results.len(),
            result.edges.len()
        );
        Ok(result)
    }

    pub fn impact(&self, request: ImpactRadiusRequest) -> ToolResult<ImpactRadiusResult> {
        legacy(&request.repo_root)?;
        if request.changed_files.len() > MAX_CHANGED_FILES {
            return Err(ToolError::new("at most 50 changed files are allowed"));
        }
        let depth = request
            .max_depth
            .unwrap_or(IMPACT_DEFAULT_DEPTH)
            .min(IMPACT_MAX_DEPTH);
        let (maximum, _) = cap(
            request.max_results,
            IMPACT_DEFAULT_RESULTS,
            IMPACT_MAX_RESULTS,
        )?;
        let mut changed_files = BTreeSet::new();
        let mut unknown_files = Vec::new();
        for path in request.changed_files {
            if path.is_empty() || path.len() > 4096 || path.split('/').any(|p| p == "..") {
                return Err(ToolError::new("invalid changed file path"));
            }
            if self.file_ids.contains_key(&path) {
                changed_files.insert(path);
                continue;
            }
            let found: Vec<_> = self
                .nodes
                .iter()
                .filter(|n| n.kind == "File" && n.path == path)
                .collect();
            match found.as_slice() {
                [node] => {
                    changed_files.insert(node.qualified_name.clone());
                }
                [] => unknown_files.push(path),
                _ => {
                    return Err(ToolError::new(
                        "changed file is ambiguous; use <repo>/<path>",
                    ));
                }
            }
        }
        let changed: BTreeSet<String> = self
            .nodes
            .iter()
            .filter(|n| changed_files.contains(&format!("{}/{}", n.repo, n.path)))
            .map(|n| n.qualified_name.clone())
            .collect();
        let mut adjacent: HashMap<&str, Vec<&str>> = HashMap::new();
        for edge in self.edges.iter().filter(|e| e.kind != "CONTAINS") {
            if let Some(target) = &edge.target {
                adjacent.entry(&edge.source).or_default().push(target);
                adjacent.entry(target).or_default().push(&edge.source);
            }
        }
        let mut seen: HashSet<String> = changed.iter().cloned().collect();
        let mut queue: VecDeque<_> = changed.iter().map(|n| (n.clone(), 0)).collect();
        let mut impacted = BTreeSet::new();
        let mut truncated = false;
        while let Some((name, level)) = queue.pop_front() {
            if level >= depth {
                continue;
            }
            let mut neighbours = adjacent.get(name.as_str()).cloned().unwrap_or_default();
            neighbours.sort_unstable();
            for target in neighbours {
                if seen.contains(target) {
                    continue;
                }
                if impacted.len() == maximum {
                    truncated = true;
                    continue;
                }
                seen.insert(target.to_owned());
                impacted.insert(target.to_owned());
                queue.push_back((target.to_owned(), level + 1));
            }
        }
        let impacted_nodes: Vec<_> = impacted
            .iter()
            .filter_map(|n| self.node(n).cloned())
            .collect();
        let impacted_files: BTreeSet<_> = impacted_nodes
            .iter()
            .map(|n| format!("{}/{}", n.repo, n.path))
            .filter(|p| !changed_files.contains(p))
            .collect();
        let all_edges: Vec<_> = self
            .edges
            .iter()
            .filter(|e| {
                e.kind != "CONTAINS"
                    && seen.contains(&e.source)
                    && e.target.as_ref().is_some_and(|t| seen.contains(t))
            })
            .collect();
        truncated |= all_edges.len() > 2000 || changed.len() > IMPACT_MAX_RESULTS as usize;
        let mut changed_nodes: Vec<_> = self
            .nodes
            .iter()
            .filter(|n| changed.contains(&n.qualified_name))
            .take(IMPACT_MAX_RESULTS as usize)
            .cloned()
            .collect();
        changed_nodes.sort_by(|a, b| a.qualified_name.cmp(&b.qualified_name));
        Ok(ImpactRadiusResult {
            status: Status::Ok,
            summary: format!("{} impacted nodes", impacted_nodes.len()),
            changed_files: changed_files.into_iter().collect(),
            unknown_files,
            changed_nodes,
            total_impacted: impacted_nodes.len() as u64,
            impacted_nodes,
            impacted_files: impacted_files.into_iter().collect(),
            edges: all_edges.into_iter().take(2000).cloned().collect(),
            truncated,
        })
    }

    pub fn review(&self, request: ReviewContextRequest) -> ToolResult<ReviewContextResult> {
        legacy(&request.repo_root)?;
        let sources = request.sources.unwrap_or_default();
        if sources.len() > SOURCES_MAX_ENTRIES
            || sources.values().any(|s| s.len() > SOURCES_MAX_BYTES)
        {
            return Err(ToolError::new("sources exceeds entry or byte limits"));
        }
        if sources.keys().any(|k| !request.changed_files.contains(k)) {
            return Err(ToolError::new(
                "sources must refer to changed_files entries",
            ));
        }
        let max_lines = request
            .max_lines_per_file
            .unwrap_or(REVIEW_DEFAULT_LINES)
            .clamp(1, REVIEW_MAX_LINES) as usize;
        let impact = self.impact(ImpactRadiusRequest {
            changed_files: request.changed_files.clone(),
            max_depth: request.max_depth,
            max_results: Some(IMPACT_DEFAULT_RESULTS),
            ..Default::default()
        })?;
        let mut snippets = BTreeMap::new();
        let mut truncated = impact.truncated;
        if request.include_source.unwrap_or(true) {
            for entry in request.changed_files {
                let node = self.node(&entry).or_else(|| {
                    self.nodes
                        .iter()
                        .find(|n| n.kind == "File" && n.path == entry)
                });
                let file = node.and_then(|n| self.repositories.get(&n.repo)?.files.get(&n.path));
                if file.is_some_and(|f| f.skipped.is_some()) {
                    snippets.insert(entry, "(content withheld by file policy)".into());
                    continue;
                }
                let text = sources
                    .get(&entry)
                    .map(String::as_str)
                    .or_else(|| file.and_then(|f| f.text.as_deref()));
                let snippet = match text {
                    None => "(source not available)".into(),
                    Some(text) => {
                        let mut bytes = 0;
                        let lines: Vec<_> = text
                            .lines()
                            .enumerate()
                            .take(max_lines)
                            .map_while(|(i, l)| {
                                let value = format!("{}: {}", i + 1, l);
                                bytes += value.len();
                                (bytes <= SOURCES_MAX_BYTES).then_some(value)
                            })
                            .collect();
                        let cut = lines.len() < text.lines().count();
                        truncated |= cut;
                        let mut snippet = lines.join("\n");
                        if cut {
                            snippet.push_str("\n... (source truncated)");
                        }
                        snippet
                    }
                };
                snippets.insert(entry, snippet);
            }
        }
        Ok(ReviewContextResult {
            status: impact.status,
            summary: impact.summary,
            truncated,
            context: ReviewContext {
                changed_files: impact.changed_files,
                unknown_files: impact.unknown_files,
                impacted_files: impact.impacted_files,
                graph: ReviewGraph {
                    changed_nodes: impact.changed_nodes,
                    impacted_nodes: impact.impacted_nodes,
                    edges: impact.edges,
                },
                source_snippets: snippets,
            },
        })
    }

    pub fn stats(&self, request: GraphStatsRequest) -> ToolResult<GraphStatsResult> {
        legacy(&request.repo_root)?;
        let repo = glob(request.repo.as_deref())?;
        let mut result = GraphStatsResult {
            status: Status::Ok,
            summary: String::new(),
            repositories: self
                .repositories
                .keys()
                .filter(|r| matches(&repo, r))
                .count() as u64,
            files_count: 0,
            total_nodes: 0,
            total_edges: 0,
            unresolved_edges: 0,
            nodes_by_kind: BTreeMap::new(),
            edges_by_kind: BTreeMap::new(),
            languages: Vec::new(),
            last_updated: None,
        };
        let mut languages = BTreeSet::new();
        for node in self.nodes.iter().filter(|n| matches(&repo, &n.repo)) {
            result.total_nodes += 1;
            result.files_count += u64::from(node.kind == "File");
            *result.nodes_by_kind.entry(node.kind.clone()).or_default() += 1;
            languages.insert(node.language.clone());
        }
        for edge in self.edges.iter().filter(|e| matches(&repo, &e.repo)) {
            result.total_edges += 1;
            result.unresolved_edges += u64::from(edge.target.is_none());
            *result.edges_by_kind.entry(edge.kind.clone()).or_default() += 1;
        }
        result.languages = languages.into_iter().collect();
        result.summary = format!(
            "{} repositories, {} files, {} nodes, {} edges",
            result.repositories, result.files_count, result.total_nodes, result.total_edges
        );
        Ok(result)
    }

    pub fn call(&self, name: &str, arguments: Value) -> ToolResult<Value> {
        fn decode<T: serde::de::DeserializeOwned>(value: Value) -> ToolResult<T> {
            serde_json::from_value(value).map_err(|_| ToolError::new("invalid tool arguments"))
        }
        fn encode<T: serde::Serialize>(result: ToolResult<T>) -> ToolResult<Value> {
            serde_json::to_value(result?)
                .map_err(|_| ToolError::new("cannot serialize tool result"))
        }
        match name {
            "search_nodes" => encode(self.search(decode(arguments)?)),
            "grep_code" => encode(self.grep(decode(arguments)?)),
            "query_graph" => encode(self.query(decode(arguments)?)),
            "get_impact_radius" => encode(self.impact(decode(arguments)?)),
            "get_review_context" => encode(self.review(decode(arguments)?)),
            "list_graph_stats" => encode(self.stats(decode(arguments)?)),
            _ => Err(ToolError::new("unknown tool")),
        }
    }
}
