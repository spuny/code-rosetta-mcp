use crate::{index, storage::Storage};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{path::Path, process::Command, time::Duration};
use url::Url;

#[derive(Deserialize)]
struct Project {
    id: u64,
    path_with_namespace: String,
    http_url_to_repo: String,
    web_url: String,
    default_branch: Option<String>,
    #[serde(default)]
    empty_repo: bool,
}

fn git() -> Command {
    let mut command = Command::new("git");
    command.args(["-c", "credential.helper=", "-c",
        "credential.helper=!f() { printf 'username=oauth2\\npassword=%s\\n' \"$GITLAB_TOKEN\"; }; f",
        "-c", "credential.useHttpPath=true", "-c", "http.followRedirects=false"])
        .env("GIT_TERMINAL_PROMPT", "0");
    command
}

pub async fn sync(
    storage: &Storage,
    url: &str,
    groups: &[String],
    repos_dir: &Path,
) -> Result<usize> {
    if groups.is_empty() {
        bail!("at least one --group is required");
    }
    let mut base = Url::parse(url).context("invalid GitLab URL")?;
    if base.scheme() != "https"
        || !base.username().is_empty()
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
    {
        bail!(
            "GitLab synchronization requires an HTTPS URL without credentials, query or fragment"
        );
    }
    if !base.path().ends_with('/') {
        base.set_path(&format!("{}/", base.path()));
    }
    let token =
        std::env::var("GITLAB_TOKEN").context("set GITLAB_TOKEN for GitLab synchronization")?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    std::fs::create_dir_all(repos_dir)?;
    let repos_dir = repos_dir.canonicalize()?;
    let mut projects = std::collections::BTreeMap::new();
    for group in groups {
        let mut page = 1;
        loop {
            let mut endpoint = base.join("api/v4/groups/")?;
            endpoint
                .path_segments_mut()
                .map_err(|_| anyhow::anyhow!("invalid GitLab base URL"))?
                .pop_if_empty()
                .push(group)
                .push("projects");
            let response = client
                .get(endpoint)
                .header("PRIVATE-TOKEN", &token)
                .query(&[
                    ("include_subgroups", "true"),
                    ("per_page", "100"),
                    ("page", &page.to_string()),
                ])
                .send()
                .await
                .context("GitLab project listing request failed")?;
            if !response.status().is_success() {
                bail!(
                    "GitLab project listing failed with HTTP {}",
                    response.status().as_u16()
                );
            }
            let next = response
                .headers()
                .get("x-next-page")
                .and_then(|h| h.to_str().ok())
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let batch: Vec<Project> = response
                .json()
                .await
                .context("invalid GitLab project listing")?;
            for project in batch {
                projects.insert(project.id, project);
            }
            let Some(next) = next else { break };
            let next: usize = next.parse().context("invalid GitLab pagination")?;
            if next <= page || next > 10000 {
                bail!("invalid GitLab pagination sequence");
            }
            page = next;
        }
    }
    let mut count = 0;
    let mut failed = 0;
    for project in projects
        .into_values()
        .filter(|p| !p.empty_repo && p.default_branch.is_some())
    {
        index::validate_name(&project.path_with_namespace)?;
        let remote = Url::parse(&project.http_url_to_repo)?;
        if remote.origin() != base.origin()
            || remote.scheme() != "https"
            || !remote.username().is_empty()
            || remote.password().is_some()
            || remote.query().is_some()
            || remote.fragment().is_some()
        {
            bail!("GitLab returned a repository URL outside the configured origin");
        }
        let root = repos_dir.join(project.id.to_string());
        let branch = project.default_branch.clone().unwrap_or_default();
        if branch.starts_with('-') || branch.chars().any(char::is_control) {
            bail!("GitLab returned an invalid default branch");
        }
        let clone_root = root.clone();
        let remote = project.http_url_to_repo.clone();
        let fetched = tokio::task::spawn_blocking(move || -> Result<()> {
            if !clone_root.exists() {
                let result = git()
                    .args([
                        "clone",
                        "--depth=1",
                        "--single-branch",
                        "--branch",
                        &branch,
                        "--",
                        &remote,
                    ])
                    .arg(&clone_root)
                    .output()?;
                if !result.status.success() {
                    bail!("git clone failed");
                }
            } else {
                if clone_root.symlink_metadata()?.file_type().is_symlink()
                    || !clone_root.join(".git").is_dir()
                {
                    bail!("sync destination is not an owned git checkout");
                }
                let origin = git()
                    .arg("-C")
                    .arg(&clone_root)
                    .args(["remote", "get-url", "origin"])
                    .output()?;
                if !origin.status.success()
                    || String::from_utf8_lossy(&origin.stdout).trim() != remote
                {
                    bail!("sync checkout origin differs from GitLab");
                }
                let status = git()
                    .arg("-C")
                    .arg(&clone_root)
                    .args(["status", "--porcelain"])
                    .output()?;
                if !status.status.success() || !status.stdout.is_empty() {
                    bail!("refusing to overwrite changes in sync checkout");
                }
                let result = git()
                    .arg("-C")
                    .arg(&clone_root)
                    .args(["fetch", "--depth=1", "origin", &branch])
                    .output()?;
                if !result.status.success() {
                    bail!("git fetch failed");
                }
                let result = git()
                    .arg("-C")
                    .arg(&clone_root)
                    .args(["checkout", "--detach", "FETCH_HEAD"])
                    .output()?;
                if !result.status.success() {
                    bail!("git checkout failed");
                }
            }
            Ok(())
        })
        .await?;
        let outcome = match fetched {
            Ok(()) => index::index(
                storage,
                &root,
                &project.path_with_namespace,
                Some(project.web_url),
            )
            .await
            .map(|_| ()),
            Err(error) => Err(error),
        };
        if outcome.is_ok() {
            count += 1;
        } else {
            failed += 1;
            tracing::warn!(
                project_id = project.id,
                "repository sync failed; previous index retained"
            );
        }
    }
    if failed > 0 {
        bail!("{failed} repositories failed to synchronize; {count} succeeded");
    }
    Ok(count)
}
