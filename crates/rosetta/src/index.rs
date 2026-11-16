use crate::storage::{File, Repository, Storage};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::Path,
    process::Command,
};
use walkdir::WalkDir;

pub const MAX_FILE_BYTES: u64 = 1_000_000;
const MAX_FILES: usize = 100_000;

#[derive(Debug, Serialize)]
pub struct IndexReport {
    pub repository: String,
    pub files: usize,
    pub parsed: usize,
    pub reused: usize,
    pub skipped: usize,
    pub parse_errors: usize,
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 200
        || name.chars().any(char::is_control)
        || name.contains(['\\', ':'])
        || name.split('/').any(|p| matches!(p, "" | "." | ".."))
    {
        bail!("repository name must be a relative name such as my-project or team/my-project");
    }
    Ok(())
}

fn excluded(path: &str) -> bool {
    path.split('/').any(|p| {
        matches!(
            p,
            ".git"
                | ".hg"
                | ".svn"
                | ".code-rosetta"
                | "node_modules"
                | "target"
                | ".terraform"
                | ".venv"
                | "venv"
                | "__pycache__"
                | "dist"
                | "build"
        )
    }) || [".db", ".db-wal", ".db-shm", ".min.js", ".min.css"]
        .iter()
        .any(|suffix| path.ends_with(suffix))
        || path.rsplit('/').next().is_some_and(|p| {
            matches!(
                p,
                "Cargo.lock"
                    | "package-lock.json"
                    | "yarn.lock"
                    | "pnpm-lock.yaml"
                    | "uv.lock"
                    | "poetry.lock"
            )
        })
}

fn git(root: &Path, args: &[&str]) -> Result<std::process::Output> {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .context("cannot execute git")
}

fn is_git_worktree(root: &Path) -> Result<bool> {
    let discovery = git(root, &["rev-parse", "--is-inside-work-tree"])?;
    if discovery.status.success() {
        return match discovery.stdout.trim_ascii() {
            b"true" => Ok(true),
            _ => {
                bail!("indexing requires a Git working tree or a plain directory, not Git metadata")
            }
        };
    }
    let mut marker = false;
    for directory in root.ancestors() {
        match directory.join(".git").symlink_metadata() {
            Ok(_) => {
                marker = true;
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => bail!("cannot inspect Git repository status; index was not changed"),
        }
    }
    if !marker
        && discovery.status.code() == Some(128)
        && discovery.stderr.starts_with(b"fatal: not a git repository")
    {
        return Ok(false);
    }
    bail!("Git repository discovery failed; index was not changed")
}

pub async fn index(
    storage: &Storage,
    root: &Path,
    name: &str,
    web_url: Option<String>,
) -> Result<IndexReport> {
    validate_name(name)?;
    if let Some(url) = &web_url {
        let url = url::Url::parse(url).context("invalid repository web URL")?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            bail!("repository web URL must be HTTP(S), without credentials, query or fragment");
        }
    }
    let root = root
        .canonicalize()
        .context("cannot resolve repository path")?;
    if !root.is_dir() {
        bail!("repository path must be a directory");
    }
    if crate::policy::credential_directory(&root.to_string_lossy()) {
        bail!("refusing to index a credential directory");
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    if home.and_then(|p| std::fs::canonicalize(p).ok()).as_ref() == Some(&root) {
        bail!("refusing to index the entire home directory; choose a project directory");
    }
    let previous = storage.load().await?.repositories.remove(name);
    let root_for_scan = root.clone();
    let name = name.to_owned();
    let (repository, report, expected) = tokio::task::spawn_blocking(move || -> Result<_> {
        let root = root_for_scan;
        let mut paths = BTreeSet::new();
        let is_git = is_git_worktree(&root)?;
        let mut git_ref = if is_git {
            let listing = git(
                &root,
                &[
                    "ls-files",
                    "-z",
                    "--cached",
                    "--exclude-standard",
                    "--",
                    ".",
                ],
            )?;
            if !listing.status.success() {
                bail!("git file listing failed; index was not changed");
            }
            for bytes in listing.stdout.split(|b| *b == 0).filter(|b| !b.is_empty()) {
                paths.insert(
                    std::str::from_utf8(bytes)
                        .context("non-UTF-8 repository path is unsupported")?
                        .to_owned(),
                );
            }
            let head = git(&root, &["rev-parse", "HEAD"])?;
            let clean = git(&root, &["diff", "--quiet", "HEAD", "--", "."])?;
            (head.status.success() && clean.status.success())
                .then(|| String::from_utf8_lossy(&head.stdout).trim().to_owned())
        } else {
            for entry in WalkDir::new(&root)
                .follow_links(false)
                .into_iter()
                .filter_entry(|e| {
                    e.path()
                        .strip_prefix(&root)
                        .is_ok_and(|p| !excluded(&p.to_string_lossy().replace('\\', "/")))
                })
            {
                let entry = entry.context("cannot walk repository; index was not changed")?;
                if !entry.file_type().is_dir() {
                    paths.insert(
                        entry
                            .path()
                            .strip_prefix(&root)?
                            .to_str()
                            .context("non-UTF-8 repository path is unsupported")?
                            .replace('\\', "/"),
                    );
                }
            }
            None
        };
        if paths.len() > MAX_FILES {
            bail!("repository exceeds the 100000-file indexing limit");
        }
        let mut report = IndexReport {
            repository: name.clone(),
            files: 0,
            parsed: 0,
            reused: 0,
            skipped: 0,
            parse_errors: 0,
        };
        let mut files = BTreeMap::new();
        for path in paths.into_iter().filter(|p| !excluded(p)) {
            if path.starts_with('/') || path.split('/').any(|s| s == "..") {
                bail!("unsafe repository path");
            }
            let full = root.join(&path);
            let metadata = match full.symlink_metadata() {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e).context("cannot inspect repository file"),
            };
            let reason = if metadata.file_type().is_symlink() {
                Some("symlink")
            } else if !metadata.is_file() {
                Some("not a regular file")
            } else if crate::policy::credential_path(&full.to_string_lossy()) {
                Some("credential filename")
            } else if metadata.len() > MAX_FILE_BYTES {
                Some("file too large")
            } else {
                None
            };
            if let Some(reason) = reason {
                files.insert(
                    path,
                    File {
                        hash: String::new(),
                        text: None,
                        parsed: None,
                        skipped: Some(reason.into()),
                    },
                );
                report.skipped += 1;
                continue;
            }
            if !full.canonicalize()?.starts_with(&root) {
                bail!("file resolves outside repository root");
            }
            let mut bytes = Vec::new();
            std::fs::File::open(&full)?
                .take(MAX_FILE_BYTES + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 > MAX_FILE_BYTES
                || bytes.contains(&0)
                || std::str::from_utf8(&bytes).is_err()
            {
                files.insert(
                    path,
                    File {
                        hash: String::new(),
                        text: None,
                        parsed: None,
                        skipped: Some("binary, non-UTF-8 or oversized".into()),
                    },
                );
                report.skipped += 1;
                continue;
            }
            let hash = format!("{:x}", Sha256::digest(&bytes));
            if let Some(old) = previous
                .as_ref()
                .filter(|r| r.parser_version == rosetta_parse::PARSER_VERSION)
                .and_then(|r| r.files.get(&path))
                .filter(|f| f.hash == hash && f.skipped.is_none())
            {
                files.insert(path, old.clone());
                report.reused += 1;
                continue;
            }
            let text = String::from_utf8(bytes)?;
            let parsed = rosetta_parse::detect(&path, text.as_bytes())
                .map(|language| rosetta_parse::parse(language, &path, &text));
            if let Some(parsed) = &parsed {
                report.parsed += 1;
                report.parse_errors += usize::from(parsed.parse_error.is_some());
            }
            files.insert(
                path,
                File {
                    hash,
                    text: Some(text),
                    parsed,
                    skipped: None,
                },
            );
        }
        report.files = files.len();
        if is_git && git_ref.is_some() {
            let clean = git(&root, &["diff", "--quiet", "HEAD", "--", "."])?;
            let head = git(&root, &["rev-parse", "HEAD"])?;
            if !clean.status.success()
                || !head.status.success()
                || git_ref.as_deref() != Some(String::from_utf8_lossy(&head.stdout).trim())
            {
                git_ref = None;
            }
        }
        let expected = previous.as_ref().map_or(0, |r| r.generation);
        let repo = Repository {
            name,
            root: root.to_string_lossy().into_owned(),
            git_ref,
            web_url,
            generation: expected,
            parser_version: rosetta_parse::PARSER_VERSION,
            files,
        };
        Ok((repo, report, expected))
    })
    .await??;
    storage.publish(repository, expected).await?;
    Ok(report)
}
