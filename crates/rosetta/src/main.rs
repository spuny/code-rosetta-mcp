use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    extract::{Path as RoutePath, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use clap::{Parser, Subcommand, ValueEnum};
use rmcp::{
    ServiceExt,
    transport::{
        stdio,
        streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
        },
    },
};
use rosetta::{engine::Engine, mcp::Mcp, storage::Storage};
use serde_json::{Value, json};
use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Backend {
    Sqlite,
    Postgres,
}
#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum Transport {
    #[default]
    Stdio,
    Http,
}

#[derive(Parser)]
#[command(
    name = "rosetta",
    version,
    about = "Cross-language code graph and search for coding agents"
)]
struct Cli {
    #[arg(
        long,
        global = true,
        env = "ROSETTA_STORAGE",
        value_enum,
        default_value = "sqlite"
    )]
    storage: Backend,
    /// SQLite file path or PostgreSQL connection URL. Never inferred from transport.
    #[arg(long, global = true, env = "ROSETTA_DATABASE")]
    database: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve read-only MCP over stdio (default) or HTTP.
    Serve {
        #[arg(long, value_enum, default_value = "stdio")]
        transport: Transport,
        #[arg(long, default_value = "127.0.0.1:3000")]
        listen: SocketAddr,
        /// Acknowledge that remote HTTP access needs authentication/TLS outside Rosetta.
        #[arg(long)]
        allow_remote: bool,
        #[arg(long = "allowed-host")]
        allowed_hosts: Vec<String>,
        #[arg(long = "allowed-origin")]
        allowed_origins: Vec<String>,
    },
    /// Index tracked working-tree files, or a plain local directory.
    Index {
        #[arg(long, default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        web_url: Option<String>,
    },
    /// Synchronize selected GitLab groups once. Requires GITLAB_TOKEN.
    Sync {
        #[arg(long)]
        url: String,
        #[arg(long = "group", required = true)]
        groups: Vec<String>,
        #[arg(long, default_value = ".code-rosetta/repos")]
        repos_dir: PathBuf,
    },
    /// Remove one repository from the index, without deleting its checkout.
    Remove {
        name: String,
    },
    /// Create or validate the selected backend's schema.
    Migrate,
    Search {
        query: String,
        #[arg(long)]
        repo: Option<String>,
    },
    Grep {
        pattern: String,
        #[arg(short = 'E', long)]
        regex: bool,
        #[arg(long)]
        repo: Option<String>,
    },
    Query {
        pattern: String,
        target: Option<String>,
        #[arg(long)]
        repo: Option<String>,
    },
    Impact {
        #[arg(long = "files", value_delimiter = ',', required = true)]
        files: Vec<String>,
    },
    ReviewContext {
        #[arg(long = "files", value_delimiter = ',', required = true)]
        files: Vec<String>,
    },
    Stats {
        #[arg(long)]
        repo: Option<String>,
    },
    Repos,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
    match run(Cli::parse()).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rosetta: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    let storage = match cli.storage {
        Backend::Sqlite => {
            let path = cli
                .database
                .unwrap_or_else(|| ".code-rosetta/rust.db".into());
            if path.contains("://") {
                bail!(
                    "SQLite storage expects a file path; select --storage postgres for PostgreSQL URLs"
                );
            }
            Storage::sqlite(&PathBuf::from(path)).await?
        }
        Backend::Postgres => {
            Storage::postgres(
                &cli.database
                    .context("--storage postgres requires --database or ROSETTA_DATABASE")?,
            )
            .await?
        }
    };
    let engine = Engine::new(storage.clone());
    match cli.command {
        Command::Serve {
            transport,
            listen,
            allow_remote,
            allowed_hosts,
            allowed_origins,
        } => {
            engine.graph().await?;
            match transport {
                Transport::Stdio => {
                    let service = Mcp(engine)
                        .serve(stdio())
                        .await
                        .context("MCP initialization failed")?;
                    tokio::select! {
                        result = service.waiting() => { result.context("MCP stdio failed")?; },
                        _ = shutdown() => {},
                    }
                }
                Transport::Http => {
                    if !listen.ip().is_loopback() && !allow_remote {
                        bail!(
                            "non-loopback HTTP requires --allow-remote and an authenticated TLS boundary"
                        );
                    }
                    let stop = CancellationToken::new();
                    let hosts = if allowed_hosts.is_empty() {
                        vec!["localhost".into(), "127.0.0.1".into(), "::1".into()]
                    } else {
                        allowed_hosts
                    };
                    let options = Arc::new(HttpOptions {
                        hosts: hosts.clone(),
                        origins: allowed_origins.clone(),
                    });
                    let config = StreamableHttpServerConfig::default()
                        .with_cancellation_token(stop.clone())
                        .with_allowed_hosts(hosts)
                        .with_allowed_origins(allowed_origins)
                        .enforce_origin_validation();
                    let mcp_engine = engine.clone();
                    let service = StreamableHttpService::new(
                        move || Ok(Mcp(mcp_engine.clone())),
                        LocalSessionManager::default().into(),
                        config,
                    );
                    let api = Router::new()
                        .route("/api/v1/tools/{name}", post(call))
                        .route("/api/v1/stats", get(stats))
                        .route("/healthz", get(health))
                        .with_state(engine);
                    let app = api
                        .nest_service("/mcp", service)
                        .layer(middleware::from_fn_with_state(options, http_guard));
                    let listener = tokio::net::TcpListener::bind(listen)
                        .await
                        .context("cannot bind HTTP listener")?;
                    eprintln!("Rosetta MCP: http://{}/mcp", listener.local_addr()?);
                    axum::serve(listener, app)
                        .with_graceful_shutdown(async move {
                            shutdown().await;
                            stop.cancel();
                        })
                        .await?;
                }
            }
        }
        Command::Index {
            path,
            name,
            web_url,
        } => {
            let canonical = path.canonicalize()?;
            let name = name
                .or_else(|| {
                    canonical
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                })
                .context("--name is required for this path")?;
            print_json(&serde_json::to_value(
                rosetta::index::index(&storage, &canonical, &name, web_url).await?,
            )?);
        }
        Command::Sync {
            url,
            groups,
            repos_dir,
        } => {
            let count = rosetta::sync::sync(&storage, &url, &groups, &repos_dir).await?;
            print_json(&json!({"status":"ok","indexed_repositories":count}));
        }
        Command::Remove { name } => {
            storage.remove(&name).await?;
            print_json(&json!({"status":"ok"}));
        }
        Command::Migrate => print_json(&json!({"status":"ok"})),
        Command::Repos => print_json(&json!(
            storage
                .load()
                .await?
                .repositories
                .keys()
                .collect::<Vec<_>>()
        )),
        command => {
            let (name, args) = match command {
                Command::Search { query, repo } => {
                    ("search_nodes", json!({"query":query,"repo":repo}))
                }
                Command::Grep {
                    pattern,
                    regex,
                    repo,
                } => (
                    "grep_code",
                    json!({"pattern":pattern,"regex":regex,"repo":repo}),
                ),
                Command::Query {
                    pattern,
                    target,
                    repo,
                } => (
                    "query_graph",
                    json!({"pattern":pattern,"target":target,"repo":repo}),
                ),
                Command::Impact { files } => ("get_impact_radius", json!({"changed_files":files})),
                Command::ReviewContext { files } => {
                    ("get_review_context", json!({"changed_files":files}))
                }
                Command::Stats { repo } => ("list_graph_stats", json!({"repo":repo})),
                _ => unreachable!(),
            };
            print_json(&engine.call(name, args).await?);
        }
    }
    Ok(())
}

fn print_json(value: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).expect("JSON value serialization")
    );
}

async fn shutdown() {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

#[derive(Clone)]
struct HttpOptions {
    hosts: Vec<String>,
    origins: Vec<String>,
}

async fn http_guard(
    State(options): State<Arc<HttpOptions>>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .filter(|h| !h.contains('@'))
        .and_then(|h| h.parse::<axum::http::uri::Authority>().ok());
    let allowed = host.as_ref().is_some_and(|h| {
        options.hosts.iter().any(|a| {
            a.eq_ignore_ascii_case(h.as_str())
                || (a.eq_ignore_ascii_case(h.host().trim_matches(['[', ']'])) && !a.contains(':'))
                || (a == "::1" && h.host().trim_matches(['[', ']']) == "::1")
        })
    });
    let origin_allowed = match request.headers().get(header::ORIGIN) {
        None => true,
        Some(origin) => origin
            .to_str()
            .ok()
            .is_some_and(|s| options.origins.iter().any(|a| a == s)),
    };
    if !allowed || !origin_allowed {
        return (StatusCode::FORBIDDEN, "Host or Origin is not allowed").into_response();
    }
    next.run(request).await
}

async fn call(
    State(engine): State<Engine>,
    RoutePath(name): RoutePath<String>,
    Json(args): Json<Value>,
) -> Response {
    match tokio::time::timeout(Duration::from_secs(30), engine.call(&name, args)).await {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(error)) => (
            if error.unavailable {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::BAD_REQUEST
            },
            Json(serde_json::to_value(error).unwrap_or_default()),
        )
            .into_response(),
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(json!({"status":"error","error":"request timed out"})),
        )
            .into_response(),
    }
}

async fn stats(State(engine): State<Engine>) -> Response {
    match engine.call("list_graph_stats", json!({})).await {
        Ok(value) => Json(value).into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status":"error"})),
        )
            .into_response(),
    }
}

async fn health(State(engine): State<Engine>) -> Response {
    match engine.storage.revision().await {
        Ok(_) => Json(json!({"status":"ok"})).into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status":"error"})),
        )
            .into_response(),
    }
}
