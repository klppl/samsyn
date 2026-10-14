//! Replica server: a y-sync compatible WebSocket server backed by yrs, with per-user vaults.
//!
//! Clients connect to `/ws/{vault_name}?token=...`. The token identifies the user; the vault
//! name is scoped to that user and created on first use. Protocol = standard y-sync, so the
//! Obsidian plugin uses `y-websocket` unchanged.

mod blobs;
mod cli;
mod client;
mod config;
mod ids;
mod storage;
mod vault;
mod ws;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::{
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    routing::{get, put},
    Json, Router,
};
use clap::Parser;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

use crate::config::{Cli, Command, ServeArgs};
use crate::storage::Storage;
use crate::vault::VaultRegistry;

pub struct AppState {
    pub serve: ServeArgs,
    pub storage: Storage,
    pub vaults: VaultRegistry,
}

pub type SharedState = Arc<AppState>;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,tower_http=warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Command::Serve(ServeArgs::parse_from(["serve"])));

    match command {
        Command::Serve(args) => {
            let storage = Storage::open(&cli.data_dir).context("opening data dir")?;
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(serve(args, storage))
        }
        Command::Sync(args) => tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(client::run(args)),
        Command::User { cmd } => cli::user(&Storage::open(&cli.data_dir)?, cmd),
        Command::Vault { cmd } => cli::vault(&Storage::open(&cli.data_dir)?, cmd),
        Command::Snapshot { target, keep } => cli::snapshot(&Storage::open(&cli.data_dir)?, target, keep),
        Command::Backup { out, user, vault } => cli::backup(&Storage::open(&cli.data_dir)?, out, user, vault),
        Command::Restore { from, merge } => cli::restore(&cli.data_dir, &from, merge),
        Command::RestoreSnapshot { target, file, yes } => {
            cli::restore_snapshot(&Storage::open(&cli.data_dir)?, &target, &file, yes)
        }
    }
}

async fn serve(args: ServeArgs, storage: Storage) -> anyhow::Result<()> {
    // Zero-config first start: create an owner user and print the token once.
    if storage.registry().user_count()? == 0 {
        let u = storage.registry().create_user("owner")?;
        let token = storage.registry().create_token(&u.id, "initial")?;
        tracing::info!("first start: created user 'owner'");
        tracing::info!("========================================================");
        tracing::info!("  token: {token}");
        tracing::info!("  paste the server URL and this token into the plugin");
        tracing::info!("  add more users with: replica-server user add <name>");
        tracing::info!("========================================================");
    }

    let max_blob = args.max_blob_mb * 1024 * 1024;
    let state: SharedState = Arc::new(AppState {
        vaults: VaultRegistry::new(storage.clone()),
        storage,
        serve: args,
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/me", get(ws::whoami))
        .route("/ws/{vault}", get(ws::ws_handler))
        .route(
            "/blobs/{vault}/{hash}",
            put(blobs::put_blob).get(blobs::get_blob).layer(DefaultBodyLimit::max(max_blob)),
        )
        .layer(CorsLayer::new().allow_origin(Any).allow_methods(Any).allow_headers(Any))
        .layer(TraceLayer::new_for_http())
        .with_state(state.clone());

    if state.serve.snapshot_hours > 0 {
        let st = state.clone();
        tokio::spawn(async move {
            let every = Duration::from_secs(st.serve.snapshot_hours * 3600);
            loop {
                tokio::time::sleep(every).await;
                st.vaults.snapshot_all(st.serve.snapshot_keep);
            }
        });
    }

    tracing::info!("data dir: {}", state.storage.root().display());

    let handle = axum_server::Handle::new();
    {
        let handle = handle.clone();
        tokio::spawn(async move {
            shutdown_signal().await;
            handle.graceful_shutdown(Some(Duration::from_secs(5)));
        });
    }

    match (&state.serve.tls_cert, &state.serve.tls_key) {
        (Some(cert), Some(key)) => {
            let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key)
                .await
                .with_context(|| format!("loading TLS cert {} / key {}", cert.display(), key.display()))?;
            tracing::info!("replica-server listening on https://{} (TLS)", state.serve.bind);
            axum_server::bind_rustls(state.serve.bind, tls)
                .handle(handle)
                .serve(app.into_make_service())
                .await?;
        }
        _ => {
            tracing::info!("replica-server listening on http://{}", state.serve.bind);
            tracing::info!("note: iOS needs https; put this behind Caddy/Tailscale or pass --tls-cert/--tls-key");
            axum_server::bind(state.serve.bind)
                .handle(handle)
                .serve(app.into_make_service())
                .await?;
        }
    }

    tracing::info!("flushing snapshots before exit");
    state.vaults.snapshot_all(state.serve.snapshot_keep);
    Ok(())
}

async fn index() -> &'static str {
    "replica-server is running. Connect the Obsidian Replica plugin to this URL."
}

async fn health(State(state): State<SharedState>) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "ok": true,
            "version": env!("CARGO_PKG_VERSION"),
            "vaults_loaded": state.vaults.loaded_count(),
        })),
    )
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}
