use anyhow::{Context, Result, bail};
use rosetta_model::ParsedFile;
use serde::{Deserialize, Serialize};
use sqlx::{AnyPool, Row, any::AnyPoolOptions};
use std::{
    collections::{BTreeMap, HashSet},
    path::Path,
    time::Duration,
};

const VERSION: i64 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct File {
    pub hash: String,
    pub text: Option<String>,
    pub parsed: Option<ParsedFile>,
    pub skipped: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Repository {
    pub name: String,
    pub root: String,
    pub git_ref: Option<String>,
    pub web_url: Option<String>,
    pub generation: i64,
    pub parser_version: i32,
    #[serde(skip)]
    pub files: BTreeMap<String, File>,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub revision: i64,
    pub repositories: BTreeMap<String, Repository>,
}

fn insert_identity(identities: &mut HashSet<String>, repo: &str, path: &str) -> Result<()> {
    if repo.contains("::") || path.contains("::") {
        bail!("indexed paths must not contain the reserved symbol delimiter ::");
    }
    if !identities.insert(format!("{repo}/{path}")) {
        bail!("indexed file identity collision; choose a different repository name");
    }
    Ok(())
}

fn withhold_credentials(repo: &mut Repository) {
    let credential_root = crate::policy::credential_directory(&repo.root);
    for (path, file) in &mut repo.files {
        if credential_root
            || crate::policy::credential_path(path)
            || crate::policy::credential_path(&format!("{}/{path}", repo.root))
        {
            file.hash.clear();
            file.text = None;
            file.parsed = None;
            file.skipped = Some("credential path".into());
        }
    }
}

#[derive(Clone)]
pub struct Storage {
    pool: AnyPool,
    postgres: bool,
}

impl Storage {
    pub async fn sqlite(path: &Path) -> Result<Self> {
        let path = std::path::absolute(path)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options
            .open(&path)
            .context("cannot open SQLite database file")?;
        let encoded =
            url::Url::from_file_path(&path).map_err(|_| anyhow::anyhow!("invalid SQLite path"))?;
        let uri = format!("sqlite://{}?mode=rwc", encoded.path());
        Self::open(&uri, false).await
    }

    pub async fn postgres(url: &str) -> Result<Self> {
        if !url.starts_with("postgres://") && !url.starts_with("postgresql://") {
            bail!("PostgreSQL storage requires a postgres:// or postgresql:// database URL");
        }
        Self::open(url, true).await
    }

    async fn open(url: &str, postgres: bool) -> Result<Self> {
        sqlx::any::install_default_drivers();
        let pool = AnyPoolOptions::new()
            .max_connections(if postgres { 8 } else { 1 })
            .acquire_timeout(Duration::from_secs(10))
            .after_connect(move |connection, _| Box::pin(async move {
                if !postgres {
                    sqlx::raw_sql("PRAGMA busy_timeout=10000; PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
                        .execute(connection).await?;
                } else {
                    sqlx::raw_sql("SET statement_timeout = 30000; SET lock_timeout = 10000;")
                        .execute(connection).await?;
                }
                Ok(())
            }))
            .connect(url).await.context("cannot connect to selected storage")?;
        let storage = Self { pool, postgres };
        storage.migrate().await?;
        Ok(storage)
    }

    async fn migrate(&self) -> Result<()> {
        let mut tx = if self.postgres {
            self.pool.begin().await?
        } else {
            self.pool.begin_with("BEGIN IMMEDIATE").await?
        };
        if self.postgres {
            sqlx::query("SELECT pg_advisory_xact_lock(72657365747461)")
                .execute(&mut *tx)
                .await?;
        }
        sqlx::raw_sql("CREATE TABLE IF NOT EXISTS rosetta_meta (id BIGINT PRIMARY KEY, version BIGINT NOT NULL, revision BIGINT NOT NULL);
            CREATE TABLE IF NOT EXISTS rosetta_repositories (name TEXT PRIMARY KEY, generation BIGINT NOT NULL, payload TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS rosetta_files (repo TEXT NOT NULL REFERENCES rosetta_repositories(name) ON DELETE CASCADE, path TEXT NOT NULL, payload TEXT NOT NULL, PRIMARY KEY(repo, path));")
            .execute(&mut *tx).await?;
        sqlx::query("INSERT INTO rosetta_meta (id, version, revision) VALUES (1, $1, 0) ON CONFLICT (id) DO NOTHING")
            .bind(VERSION).execute(&mut *tx).await?;
        let version: i64 = sqlx::query_scalar("SELECT version FROM rosetta_meta WHERE id=1")
            .fetch_one(&mut *tx)
            .await?;
        if version != VERSION {
            bail!("unsupported public Rust database schema {version}; expected {VERSION}");
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn revision(&self) -> Result<i64> {
        Ok(
            sqlx::query_scalar("SELECT revision FROM rosetta_meta WHERE id=1")
                .fetch_one(&self.pool)
                .await?,
        )
    }

    pub async fn load(&self) -> Result<Snapshot> {
        let mut tx = self.pool.begin().await?;
        if self.postgres {
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
                .execute(&mut *tx)
                .await?;
        }
        let revision = sqlx::query_scalar("SELECT revision FROM rosetta_meta WHERE id=1")
            .fetch_one(&mut *tx)
            .await?;
        let mut repositories = BTreeMap::new();
        for row in sqlx::query("SELECT payload FROM rosetta_repositories ORDER BY name")
            .fetch_all(&mut *tx)
            .await?
        {
            let repo: Repository = serde_json::from_str(row.try_get::<&str, _>(0)?)?;
            repositories.insert(repo.name.clone(), repo);
        }
        for row in sqlx::query("SELECT repo, path, payload FROM rosetta_files ORDER BY repo, path")
            .fetch_all(&mut *tx)
            .await?
        {
            let repo: &str = row.try_get(0)?;
            let path: String = row.try_get(1)?;
            let file = serde_json::from_str(row.try_get::<&str, _>(2)?)?;
            repositories
                .get_mut(repo)
                .context("file references an unknown repository")?
                .files
                .insert(path, file);
        }
        let mut identities = HashSet::new();
        for repo in repositories.values_mut() {
            for path in repo.files.keys() {
                insert_identity(&mut identities, &repo.name, path)?;
            }
            withhold_credentials(repo);
        }
        tx.commit().await?;
        Ok(Snapshot {
            revision,
            repositories,
        })
    }

    pub async fn publish(&self, mut repo: Repository, expected_generation: i64) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE rosetta_meta SET revision=revision+1 WHERE id=1")
            .execute(&mut *tx)
            .await?;
        let generation: Option<i64> =
            sqlx::query_scalar("SELECT generation FROM rosetta_repositories WHERE name=$1")
                .bind(&repo.name)
                .fetch_optional(&mut *tx)
                .await?;
        if generation.unwrap_or(0) != expected_generation {
            bail!("repository changed during indexing; retry the index command");
        }
        let mut identities = HashSet::new();
        for path in repo.files.keys() {
            insert_identity(&mut identities, &repo.name, path)?;
        }
        for row in sqlx::query("SELECT repo, path FROM rosetta_files WHERE repo <> $1")
            .bind(&repo.name)
            .fetch_all(&mut *tx)
            .await?
        {
            insert_identity(&mut identities, row.try_get(0)?, row.try_get(1)?)?;
        }
        withhold_credentials(&mut repo);
        repo.generation = expected_generation + 1;
        let payload = serde_json::to_string(&repo)?;
        sqlx::query("INSERT INTO rosetta_repositories (name, generation, payload) VALUES ($1, $2, $3) ON CONFLICT (name) DO UPDATE SET generation=excluded.generation, payload=excluded.payload")
            .bind(&repo.name).bind(repo.generation).bind(payload).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM rosetta_files WHERE repo=$1")
            .bind(&repo.name)
            .execute(&mut *tx)
            .await?;
        for (path, file) in &repo.files {
            sqlx::query("INSERT INTO rosetta_files (repo, path, payload) VALUES ($1, $2, $3)")
                .bind(&repo.name)
                .bind(path)
                .bind(serde_json::to_string(file)?)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn remove(&self, name: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE rosetta_meta SET revision=revision+1 WHERE id=1")
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM rosetta_repositories WHERE name=$1")
            .bind(name)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
}
