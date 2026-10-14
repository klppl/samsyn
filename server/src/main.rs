//! Replica server: a y-sync compatible WebSocket server backed by yrs + SQLite.
//!
//! One Yjs document per vault. Clients connect to `/ws/{vault_id}?token=...`
//! and speak the standard y-sync protocol (same one as y-websocket), so the
//! Obsidian plugin can use `y-protocols` unchanged.

mod blobs;
mod config;
mod store;
mod vault;
mod ws;

use std::sync::Arc;

use anyhow::Context;
use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, put},
    Json, Router,
};
use clap::Parser;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::store::Store;
use crate::vault::VaultRegistry;

pub struct AppState {
    pub config: Config,
    pub store: Store,
    pub vaults: VaultRegistry,
}

pub type SharedState = Arc<AppState>;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,tower_http=warn")),
        )
        .init();

    let config = Config::parse().resolve().context("resolving configuration")?;
    let store = Store::open(&config.data_dir.join("replica.db")).context("opening database")?;
    let state: SharedState = Arc::new(AppState {
        vaults: VaultRegistry::new(store.clone()),
        config,
        store,
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/ws/{vault_id}", get(ws::ws_handler))
        .route(
            "/blobs/{vault_id}/{hash}",
            put(blobs::put_blob).get(blobs::get_blob),
        )
        .layer(CorsLayer::new().allow_origin(Any).allow_methods(Any).allow_headers(Any))
        .layer(TraceLayer::new_for_http())
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(state.config.bind)
        .await
        .with_context(|| format!("binding {}", state.config.bind))?;

    tracing::info!("replica-server listening on http://{}", state.config.bind);
    tracing::info!("data dir: {}", state.config.data_dir.display());
    tracing::info!("auth token: {}", state.config.token);
    tracing::info!("paste the URL and token into the Replica plugin settings");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
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
