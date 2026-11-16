use crate::graph::Graph;
use rosetta_model::{NodeKind, RefScheme};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

const TF_RESERVED: &[&str] = &["count", "each", "path", "self", "terraform"];
const KUSTOMIZATION_FILES: &[&str] = &["kustomization.yaml", "kustomization.yml", "Kustomization"];

#[derive(Clone, Copy)]
struct PyRule {
    imports: bool,
    via_import: bool,
}
impl PyRule {
    const IMPORT: Self = Self {
        imports: true,
        via_import: true,
    };
}

pub fn resolve(graph: &mut Graph) {
    let mut indexes = BTreeMap::new();
    let mut packages: HashMap<String, BTreeSet<String>> = HashMap::new();
    let mut own_heads: HashMap<String, HashSet<String>> = HashMap::new();
    for name in graph.repositories.keys() {
        let files: Vec<FileRow> = graph
            .nodes
            .iter()
            .filter(|n| n.repo == *name && n.kind == "File")
            .map(|n| FileRow {
                id: n.id,
                path: n.path.clone(),
                language: Some(n.language.clone()),
            })
            .collect();
        let paths: HashSet<&str> = files.iter().map(|f| f.path.as_str()).collect();
        for file in &files {
            if file.language.as_deref() != Some("python") {
                continue;
            }
            let is_package = |dir: &str| paths.contains(in_dir(dir, "__init__.py").as_str());
            if let Some(package) = top_level_package(&file.path, is_package) {
                packages.entry(package).or_default().insert(name.clone());
            }
            for module in module_names(&file.path, is_package) {
                own_heads
                    .entry(name.clone())
                    .or_default()
                    .insert(module.split('.').next().unwrap_or("").into());
            }
        }
        let nodes = graph
            .nodes
            .iter()
            .filter(|n| n.repo == *name)
            .map(|n| NodeRow {
                id: n.id,
                file: graph.file_ids[&format!("{}/{}", n.repo, n.path)],
                kind: n.kind.parse().ok(),
                name: n.name.clone(),
                language: n.language.clone(),
                namespace: n
                    .extra
                    .get("namespace")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
                backend_key: n
                    .extra
                    .get("backend_key")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
            });
        indexes.insert(name.clone(), Index::build(files, nodes));
    }
    for edge in &mut graph.edges {
        let Some(reference) = &edge.target_ref else {
            continue;
        };
        let Some((scheme, value)) = reference.split_once(':') else {
            continue;
        };
        let Ok(scheme) = scheme.parse::<RefScheme>() else {
            continue;
        };
        let Some(index) = indexes.get(&edge.repo) else {
            continue;
        };
        let file = graph.file_ids[&format!("{}/{}", edge.repo, edge.path)];
        let source = graph
            .by_qualified
            .get(&edge.source)
            .map(|i| &graph.nodes[*i]);
        let namespace = source
            .and_then(|n| n.extra.get("namespace"))
            .and_then(|v| v.as_str());
        let rule = PyRule {
            imports: edge.kind == "IMPORTS",
            via_import: edge.extra.get("via").and_then(|v| v.as_str()) == Some("import"),
        };
        let mut target = if scheme == RefScheme::Py {
            index.resolve_py(value, &edge.path, rule, false)
        } else {
            index.resolve(scheme, value, &edge.path, file, namespace)
        };
        if target.is_none() && scheme == RefScheme::Py && rule.via_import && !value.starts_with('.')
        {
            let head = value.split('.').next().unwrap_or("");
            if !own_heads.get(&edge.repo).is_some_and(|h| h.contains(head))
                && let Some(repos) = packages.get(head).filter(|r| r.len() == 1)
                && let Some(index) = repos
                    .first()
                    .filter(|r| *r != &edge.repo)
                    .and_then(|r| indexes.get(r))
            {
                target = index.resolve_py(value, &edge.path, rule, true);
            }
        }
        if target.is_none() && scheme == RefScheme::TfState {
            target = single(
                indexes
                    .values()
                    .filter_map(|i| i.resolve_tfstate(value))
                    .collect(),
            );
        }
        edge.target = target
            .and_then(|id| graph.nodes.get((id - 1) as usize))
            .map(|n| n.qualified_name.clone());
    }
}

fn single(mut found: Vec<i64>) -> Option<i64> {
    found.sort_unstable();
    found.dedup();
    (found.len() == 1).then(|| found[0])
}

// ------------------------------------------------------------------------------------------
// In-memory index of one repository

struct FileRow {
    id: i64,
    path: String,
    language: Option<String>,
}

struct NodeRow {
    id: i64,
    file: i64,
    kind: Option<NodeKind>,
    name: String,
    language: String,
    namespace: Option<String>,
    backend_key: Option<String>,
}

struct K8sCandidate {
    node: i64,
    file: i64,
    dir: String,
    namespace: Option<String>,
}

#[derive(Default)]
struct Index {
    /// file id -> path
    paths: HashMap<i64, String>,
    by_path: HashMap<String, i64>,
    /// base name -> file ids (for the `template` suffix rule)
    by_base: HashMap<String, Vec<i64>>,
    /// file id -> its File node
    file_node: HashMap<i64, i64>,
    named: HashMap<(i64, NodeKind, String), i64>,
    /// directory -> `.tf` files, sorted by path
    tf_dirs: HashMap<String, Vec<i64>>,
    /// Python module name -> files
    modules: HashMap<String, Vec<i64>>,
    /// `Kind/name` (without any `@<line>` suffix) -> resources
    k8s: HashMap<String, Vec<K8sCandidate>>,
    /// Go template `define` name -> (node, file)
    helm: HashMap<String, Vec<(i64, i64)>>,
    /// Terraform backend key -> directories whose TerraformConfig uses it
    backends: HashMap<String, BTreeSet<String>>,
}

impl Index {
    fn build(
        files: impl IntoIterator<Item = FileRow>,
        nodes: impl IntoIterator<Item = NodeRow>,
    ) -> Self {
        let mut index = Index::default();
        let mut python = Vec::new();
        let mut tf: Vec<(String, i64)> = Vec::new();
        for f in files {
            let base = f.path.rsplit('/').next().unwrap_or(&f.path).to_owned();
            index.by_base.entry(base).or_default().push(f.id);
            index.by_path.insert(f.path.clone(), f.id);
            if f.language.as_deref() == Some("python") && f.path.ends_with(".py") {
                python.push(f.id);
            }
            if is_tf(&f.path) {
                tf.push((f.path.clone(), f.id));
            }
            index.paths.insert(f.id, f.path);
        }
        tf.sort();
        for (path, id) in tf {
            index
                .tf_dirs
                .entry(dir_of(&path).to_owned())
                .or_default()
                .push(id);
        }
        for id in python {
            let Some(path) = index.paths.get(&id) else {
                continue;
            };
            let names = module_names(path, |dir| {
                index.by_path.contains_key(&in_dir(dir, "__init__.py"))
            });
            for name in names {
                index.modules.entry(name).or_default().push(id);
            }
        }
        for n in nodes {
            let Some(kind) = n.kind else { continue };
            match kind {
                NodeKind::File => {
                    index.file_node.insert(n.file, n.id);
                }
                NodeKind::K8sResource => {
                    let base = n.name.split('@').next().unwrap_or(&n.name).to_owned();
                    let dir = index
                        .paths
                        .get(&n.file)
                        .map_or("", |p| dir_of(p))
                        .to_owned();
                    index.k8s.entry(base).or_default().push(K8sCandidate {
                        node: n.id,
                        file: n.file,
                        dir,
                        namespace: n.namespace.clone(),
                    });
                }
                NodeKind::Template if n.language == "gotemplate" => {
                    index
                        .helm
                        .entry(n.name.clone())
                        .or_default()
                        .push((n.id, n.file));
                }
                NodeKind::TerraformConfig => {
                    if let (Some(key), Some(path)) = (&n.backend_key, index.paths.get(&n.file))
                        && is_tf(path)
                    {
                        index
                            .backends
                            .entry(key.clone())
                            .or_default()
                            .insert(dir_of(path).to_owned());
                    }
                }
                _ => {}
            }
            index.named.insert((n.file, kind, n.name), n.id);
        }
        index
    }

    fn node(&self, file: i64, kind: NodeKind, name: &str) -> Option<i64> {
        self.named.get(&(file, kind, name.to_owned())).copied()
    }

    fn resolve(
        &self,
        scheme: RefScheme,
        value: &str,
        from: &str,
        from_file: i64,
        namespace: Option<&str>,
    ) -> Option<i64> {
        if value.is_empty() {
            return None;
        }
        match scheme {
            RefScheme::Py => self.resolve_py(value, from, PyRule::IMPORT, false),
            RefScheme::File => self
                .resolve_path(value, dir_of(from))
                .and_then(|f| self.file_node.get(&f).copied()),
            RefScheme::Template => self.resolve_template(value, dir_of(from)),
            RefScheme::Tf => self.resolve_tf(value, dir_of(from), from_file),
            RefScheme::TfModule => self.resolve_tfmodule(value, dir_of(from)),
            RefScheme::TfVar => {
                let (source, name) = value.split_once('|')?;
                let dir = join(dir_of(from), source)?;
                self.in_tf_dir(&dir, NodeKind::Variable, name, None)
            }
            RefScheme::TfState => self.resolve_tfstate(value),
            RefScheme::K8s => self.resolve_k8s(value, from, from_file, namespace),
            RefScheme::Helm => self.resolve_helm(value, from),
            RefScheme::Values => self.resolve_values(value, from),
        }
    }

    // ---- py

    /// A `py` reference under `rule`. `strict` (cross-repository): the whole name must resolve,
    /// with no shorter symbol match and no File-node fallback.
    fn resolve_py(&self, value: &str, from: &str, rule: PyRule, strict: bool) -> Option<i64> {
        if !rule.via_import {
            return None;
        }
        let (file, rest) = if value.starts_with('.') {
            self.py_module_relative(value, from)?
        } else {
            self.py_module_absolute(value, from)?
        };
        self.py_symbol(file, &rest, rule.imports && !strict, strict)
            .or_else(|| {
                (rule.imports && rest.is_empty())
                    .then(|| self.file_node.get(&file).copied())
                    .flatten()
            })
    }

    /// The module file named by the longest prefix of `value`, and the rest of the name.
    fn py_module_absolute<'v>(&self, value: &'v str, from: &str) -> Option<(i64, Vec<&'v str>)> {
        let parts: Vec<&str> = value.split('.').collect();
        if parts.iter().any(|p| !is_identifier(p)) {
            return None;
        }
        for i in (1..=parts.len()).rev() {
            if let Some(files) = self.modules.get(&parts[..i].join(".")) {
                let file = self.closest(files, from)?;
                return Some((file, parts[i..].to_vec()));
            }
        }
        None
    }

    fn py_module_relative<'v>(&self, value: &'v str, from: &str) -> Option<(i64, Vec<&'v str>)> {
        let dots = value.bytes().take_while(|b| *b == b'.').count();
        let rest = &value[dots..];
        let parts: Vec<&str> = if rest.is_empty() {
            Vec::new()
        } else {
            rest.split('.').collect()
        };
        if parts.iter().any(|p| !is_identifier(p)) {
            return None;
        }
        let mut base = dir_of(from).to_owned();
        for _ in 1..dots {
            if base.is_empty() {
                return None;
            }
            base = dir_of(&base).to_owned();
        }
        for i in (0..=parts.len()).rev() {
            let dir = if i == 0 {
                base.clone()
            } else {
                in_dir(&base, &parts[..i].join("/"))
            };
            let mut candidates = vec![in_dir(&dir, "__init__.py")];
            if i > 0 {
                candidates.insert(0, format!("{dir}.py"));
            }
            if let Some(file) = candidates.iter().find_map(|p| self.by_path.get(p)) {
                return Some((*file, parts[i..].to_vec()));
            }
        }
        None
    }

    /// The Class, Function or Method `rest` in `file`. Not strict: the longest leading part of
    /// `rest` that names a symbol (`Class.attr.x` gives `Class`). `file_fallback`: the File node
    /// when no symbol matches (imports only, never calls or inheritance).
    fn py_symbol(
        &self,
        file: i64,
        rest: &[&str],
        file_fallback: bool,
        strict: bool,
    ) -> Option<i64> {
        let shortest = if strict { rest.len() } else { 1 };
        for j in (shortest.max(1)..=rest.len()).rev() {
            let name = rest[..j].join(".");
            for kind in [NodeKind::Class, NodeKind::Function, NodeKind::Method] {
                if let Some(node) = self.node(file, kind, &name) {
                    return Some(node);
                }
            }
        }
        if file_fallback {
            self.file_node.get(&file).copied()
        } else {
            None
        }
    }

    /// The candidate sharing the longest leading path with `from`; `None` on a tie.
    fn closest(&self, files: &[i64], from: &str) -> Option<i64> {
        if let [only] = files {
            return Some(*only);
        }
        let mut best: Option<(usize, i64)> = None;
        let mut tied = false;
        for file in files {
            let score = self
                .paths
                .get(file)
                .map_or(0, |p| common_components(dir_of(p), dir_of(from)));
            match best {
                Some((b, _)) if score < b => {}
                Some((b, _)) if score == b => tied = true,
                _ => {
                    best = Some((score, *file));
                    tied = false;
                }
            }
        }
        if tied { None } else { best.map(|(_, f)| f) }
    }

    // ---- file / template

    /// A stored file: relative to `dir`, then to the repository root; a directory resolves to
    /// its kustomization file.
    fn resolve_path(&self, value: &str, dir: &str) -> Option<i64> {
        if value.contains("://") {
            return None;
        }
        let mut paths = Vec::new();
        if value.starts_with('/') {
            paths.extend(join("", value));
        } else {
            paths.extend(join(dir, value));
            paths.extend(join("", value));
        }
        for path in paths {
            if path.is_empty() {
                continue;
            }
            if let Some(file) = self.by_path.get(&path) {
                return Some(*file);
            }
            if let Some(file) = KUSTOMIZATION_FILES
                .iter()
                .find_map(|k| self.by_path.get(&in_dir(&path, k)))
            {
                return Some(*file);
            }
        }
        None
    }

    fn resolve_template(&self, value: &str, dir: &str) -> Option<i64> {
        let (path, macro_name) = match value.rsplit_once('#') {
            Some((p, m)) => (p, Some(m)),
            None => (value, None),
        };
        let file = self.resolve_path(path, dir).or_else(|| {
            let wanted = join("", path)?;
            let base = wanted.rsplit('/').next()?;
            let suffix = format!("/{wanted}");
            let found: Vec<i64> = self
                .by_base
                .get(base)?
                .iter()
                .filter(|f| self.paths.get(f).is_some_and(|p| p.ends_with(&suffix)))
                .copied()
                .collect();
            single(found)
        })?;
        match macro_name {
            None => self.file_node.get(&file).copied(),
            Some(m) => self
                .node(file, NodeKind::Macro, m)
                .or_else(|| self.node(file, NodeKind::Macro, &format!("macro:{m}"))),
        }
    }

    // ---- terraform

    fn in_tf_dir(&self, dir: &str, kind: NodeKind, name: &str, prefer: Option<i64>) -> Option<i64> {
        let found: Vec<(i64, i64)> = self
            .tf_dirs
            .get(dir)?
            .iter()
            .filter_map(|f| self.node(*f, kind, name).map(|n| (*f, n)))
            .collect();
        match found.as_slice() {
            [] => None,
            [(_, n)] => Some(*n),
            many => prefer.and_then(|p| many.iter().find(|(f, _)| *f == p).map(|(_, n)| *n)),
        }
    }

    fn resolve_tf(&self, value: &str, dir: &str, from_file: i64) -> Option<i64> {
        let parts: Vec<&str> = value.split('.').collect();
        let (kind, name) = match parts.as_slice() {
            ["var", x, ..] => (NodeKind::Variable, (*x).to_owned()),
            ["local", x, ..] => (NodeKind::Local, (*x).to_owned()),
            ["module", x, ..] => (NodeKind::Module, (*x).to_owned()),
            ["data", t, n, ..] => (NodeKind::DataSource, format!("{t}.{n}")),
            [t, n, ..] if !TF_RESERVED.contains(t) && *t != "data" => {
                (NodeKind::Resource, format!("{t}.{n}"))
            }
            _ => return None,
        };
        self.in_tf_dir(dir, kind, &name, Some(from_file))
    }

    fn resolve_tfmodule(&self, value: &str, dir: &str) -> Option<i64> {
        let target = join(dir, value.trim_end_matches('/'))?;
        let main = self.by_path.get(&in_dir(&target, "main.tf"));
        let file = main.or_else(|| self.tf_dirs.get(&target).and_then(|f| f.first()))?;
        self.file_node.get(file).copied()
    }

    fn resolve_tfstate(&self, value: &str) -> Option<i64> {
        let (key, output) = value.split_once('|')?;
        let found: Vec<i64> = self
            .backends
            .get(key)?
            .iter()
            .flat_map(|dir| {
                self.tf_dirs
                    .get(dir)
                    .into_iter()
                    .flatten()
                    .filter_map(|f| self.node(*f, NodeKind::Output, output))
            })
            .collect();
        single(found)
    }

    // ---- kubernetes / helm

    fn resolve_k8s(
        &self,
        value: &str,
        from: &str,
        from_file: i64,
        namespace: Option<&str>,
    ) -> Option<i64> {
        let all = self.k8s.get(value)?;
        let dir = dir_of(from);
        let same_file: Vec<&K8sCandidate> = all.iter().filter(|c| c.file == from_file).collect();
        let same_dir: Vec<&K8sCandidate> = all.iter().filter(|c| c.dir == dir).collect();
        let everywhere: Vec<&K8sCandidate> = all.iter().collect();
        let level = [same_file, same_dir, everywhere]
            .into_iter()
            .find(|l| !l.is_empty())?;
        if let [only] = level.as_slice() {
            return Some(only.node);
        }
        let matching: Vec<i64> = level
            .iter()
            .filter(|c| c.namespace.as_deref() == namespace)
            .map(|c| c.node)
            .collect();
        single(matching)
    }

    fn chart_root(&self, dir: &str) -> Option<String> {
        ancestors(dir).find(|a| self.by_path.contains_key(&in_dir(a, "Chart.yaml")))
    }

    fn resolve_helm(&self, value: &str, from: &str) -> Option<i64> {
        let all = self.helm.get(value)?;
        if let Some(root) = self.chart_root(dir_of(from)) {
            let in_chart: Vec<i64> = all
                .iter()
                .filter(|(_, f)| {
                    self.paths
                        .get(f)
                        .and_then(|p| self.chart_root(dir_of(p)))
                        .is_some_and(|r| r == root)
                })
                .map(|(n, _)| *n)
                .collect();
            match in_chart.len() {
                0 => {}
                1 => return in_chart.first().copied(),
                _ => return None,
            }
        }
        single(all.iter().map(|(n, _)| *n).collect())
    }

    fn resolve_values(&self, value: &str, from: &str) -> Option<i64> {
        let dir = dir_of(from);
        let root = self.chart_root(dir).or_else(|| {
            ancestors(dir).find(|a| {
                ["values.yaml", "values.yml"]
                    .iter()
                    .any(|v| self.by_path.contains_key(&in_dir(a, v)))
            })
        })?;
        ["values.yaml", "values.yml"].iter().find_map(|v| {
            let file = self.by_path.get(&in_dir(&root, v))?;
            self.node(*file, NodeKind::Section, value)
        })
    }
}

// ------------------------------------------------------------------------------------------
// Path helpers

fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(d, _)| d)
}

fn in_dir(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

fn is_tf(path: &str) -> bool {
    path.ends_with(".tf")
}

/// `rel` applied to `base` with `.`/`..` normalised; `None` when it leaves the repository.
fn join(base: &str, rel: &str) -> Option<String> {
    let mut parts: Vec<&str> = if rel.starts_with('/') {
        Vec::new()
    } else {
        base.split('/').filter(|s| !s.is_empty()).collect()
    };
    for segment in rel.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}

/// `dir`, its parent, ..., and finally `""` (the repository root).
fn ancestors(dir: &str) -> impl Iterator<Item = String> + '_ {
    let mut next = Some(dir.to_owned());
    std::iter::from_fn(move || {
        let current = next.take()?;
        if !current.is_empty() {
            next = Some(dir_of(&current).to_owned());
        }
        Some(current)
    })
}

fn common_components(a: &str, b: &str) -> usize {
    a.split('/')
        .zip(b.split('/'))
        .take_while(|(x, y)| x == y && !x.is_empty())
        .count()
}

fn is_identifier(part: &str) -> bool {
    let mut chars = part.chars();
    chars.next().is_some_and(|c| c == '_' || c.is_alphabetic())
        && chars.all(|c| c == '_' || c.is_alphanumeric())
}

/// The module components of a Python file and the index of its package root: the topmost
/// directory of the unbroken chain of packages (directories with `__init__.py`) holding the file.
/// `pkg/__init__.py` is the module `pkg`. `is_package` receives a directory path.
fn module_parts(path: &str, is_package: impl Fn(&str) -> bool) -> Option<(Vec<&str>, usize)> {
    let stem = path.strip_suffix(".py")?;
    let mut parts: Vec<&str> = stem.split('/').collect();
    let init = parts.last() == Some(&"__init__");
    if init {
        parts.pop();
    }
    if parts.is_empty() || parts.iter().any(|p| !is_identifier(p)) {
        return None;
    }
    // Directories holding the module: all of `parts` for a package's `__init__.py`.
    let dirs = if init { parts.len() } else { parts.len() - 1 };
    let mut root = dirs;
    while root > 0 && is_package(&parts[..root].join("/")) {
        root -= 1;
    }
    if init && root == dirs {
        root = dirs - 1;
    }
    Some((parts, root))
}

/// Module names of a Python file: the full path (`a/b/c.py` is `a.b.c`), the name rooted at its
/// package root (`x/app/rag/f.py` with `app/` and `app/rag/` packages is `app.rag.f`; a file in a
/// directory that is no package is just its stem), and the name without a leading `src`/`lib`.
fn module_names(path: &str, is_package: impl Fn(&str) -> bool) -> Vec<String> {
    let Some((parts, root)) = module_parts(path, is_package) else {
        return Vec::new();
    };
    let mut names = vec![parts.join(".")];
    let rooted = parts[root..].join(".");
    if !names.contains(&rooted) {
        names.push(rooted);
    }
    if parts.len() >= 2 && matches!(parts[0], "src" | "lib") {
        let stripped = parts[1..].join(".");
        if !names.contains(&stripped) {
            names.push(stripped);
        }
    }
    names
}

/// The top-level package a Python file belongs to, when there is one: its package root is a
/// directory at the repository root or directly under `src/` or `lib/`. Single-file modules and
/// packages nested below other directories never qualify (they are not importable from another
/// repository by their head).
fn top_level_package(path: &str, is_package: impl Fn(&str) -> bool) -> Option<String> {
    let (parts, root) = module_parts(path, &is_package)?;
    let dirs = if path.ends_with("/__init__.py") {
        parts.len()
    } else {
        parts.len() - 1
    };
    if root >= dirs || !is_package(&parts[..=root].join("/")) {
        return None;
    }
    let at_top = root == 0 || (root == 1 && matches!(parts[0], "src" | "lib"));
    at_top.then(|| parts[root].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_join_and_ancestors() {
        assert_eq!(join("a/b", "./c.sh").as_deref(), Some("a/b/c.sh"));
        assert_eq!(join("a/b", "../c").as_deref(), Some("a/c"));
        assert_eq!(join("a", "../../c"), None);
        assert_eq!(join("a/b", "/x/y").as_deref(), Some("x/y"));
        assert_eq!(
            ancestors("a/b").collect::<Vec<_>>(),
            vec!["a/b".to_owned(), "a".to_owned(), String::new()]
        );
        assert_eq!(ancestors("").collect::<Vec<_>>(), vec![String::new()]);
        assert_eq!(common_components("a/b/c", "a/b/d"), 2);
        assert_eq!(common_components("", "a"), 0);
    }

    #[test]
    fn python_module_names() {
        let pkgs = [
            "acme",
            "src/acme",
            "reports/x/backend/app",
            "reports/x/backend/app/rag",
        ];
        let is_pkg = |d: &str| pkgs.contains(&d);
        assert_eq!(module_names("acme/core.py", is_pkg), vec!["acme.core"]);
        assert_eq!(
            module_names("src/acme/core.py", is_pkg),
            vec!["src.acme.core", "acme.core"]
        );
        assert_eq!(
            module_names("scripts/run.py", is_pkg),
            vec!["scripts.run", "run"]
        );
        assert_eq!(module_names("app.py", is_pkg), vec!["app"]);
        assert_eq!(module_names("acme/__init__.py", is_pkg), vec!["acme"]);
        assert_eq!(
            module_names("reports/x/backend/app/rag/fusion.py", is_pkg),
            vec!["reports.x.backend.app.rag.fusion", "app.rag.fusion"]
        );
        assert_eq!(
            module_names("reports/x/backend/app/__init__.py", is_pkg),
            vec!["reports.x.backend.app", "app"]
        );
        assert_eq!(module_names("__init__.py", is_pkg), Vec::<String>::new());
        assert_eq!(module_names("my-tool/x.py", is_pkg), Vec::<String>::new());
        assert_eq!(module_names("x.txt", is_pkg), Vec::<String>::new());
    }

    #[test]
    fn only_root_packages_are_top_level() {
        let pkgs = [
            "acme",
            "acme/sub",
            "src/lib2",
            "reports/x/app",
            "tests/data",
        ];
        let is_pkg = |d: &str| pkgs.contains(&d);
        assert_eq!(
            top_level_package("acme/sub/m.py", is_pkg).as_deref(),
            Some("acme")
        );
        assert_eq!(
            top_level_package("acme/__init__.py", is_pkg).as_deref(),
            Some("acme")
        );
        assert_eq!(
            top_level_package("src/lib2/m.py", is_pkg).as_deref(),
            Some("lib2")
        );
        assert_eq!(top_level_package("app.py", is_pkg), None);
        assert_eq!(top_level_package("settings.py", is_pkg), None);
        assert_eq!(top_level_package("reports/x/app/main.py", is_pkg), None);
        assert_eq!(top_level_package("tests/data/__init__.py", is_pkg), None);
        assert_eq!(top_level_package("alembic/env.py", is_pkg), None);
    }
}
