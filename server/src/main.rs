//! Replica server: a y-sync compatible WebSocket server backed by yrs, with per-user vaults.
//!
//! Clients connect to `/ws/{vault_name}?token=...`. The token identifies the user; the vault
//! name is scoped to that user and created on first use. Protocol = standard y-sync, so the
//! Obsidian plugin uses `y-websocket` unchanged.

mod blobs;
mod bridge;
mod cli;
mod client;
mod config;
mod guest;
mod ids;
mod presence;
mod remote;
mod sharing;
mod storage;
mod ticket;
mod vault;
mod ws;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::{
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    routing::{delete, get, post, put},
    Json, Router,
};
use clap::Parser;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

use crate::config::{Cli, Command, ServeArgs};
use crate::storage::Storage;
use crate::vault::DocRegistry;

pub struct AppState {
    pub serve: ServeArgs,
    pub storage: Storage,
    pub docs: Arc<DocRegistry>,
    pub guest_limiter: std::sync::Mutex<guest::GuestLimiter>,
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

    let identity = storage.identity()?;
    let node_id = identity.public().to_string();
    tracing::info!("server node id: {node_id}");

    // Peer-to-peer endpoint for server-to-server shares (phase 2).
    let endpoint = if args.no_p2p {
        tracing::info!("p2p disabled; tickets only work between vaults on this server");
        None
    } else {
        match remote::bind_endpoint(identity, &args.relay, args.p2p_port).await {
            Ok(ep) => {
                let relays_off = args.relay.iter().any(|r| r.trim() == "none");
                tracing::info!("p2p endpoint bound ({})", if relays_off { "direct only, no relay" } else { "relay + direct" });
                {
                    // Relay registration can take a few seconds; do not hold up HTTP for it.
                    let ep = ep.clone();
                    tokio::spawn(async move {
                        if !relays_off && tokio::time::timeout(Duration::from_secs(20), ep.online()).await.is_err() {
                            tracing::warn!("p2p endpoint not online after 20 s (relay unreachable?); direct connections may still work");
                        }
                        tracing::info!("p2p addresses: {:?}", remote::addr_strings(&ep));
                    });
                }
                Some(ep)
            }
            Err(e) => {
                // A busy UDP port must not take the sync server down; only server-to-server
                // shares are affected.
                tracing::error!("p2p endpoint unavailable (port {} busy?): {e:#}; continuing without server-to-server sharing", args.p2p_port);
                None
            }
        }
    };

    let max_blob = args.max_blob_mb * 1024 * 1024;
    let docs = Arc::new(DocRegistry::new(storage.clone(), node_id, endpoint.clone(), max_blob));
    let router = endpoint.map(|ep| {
        iroh::protocol::Router::builder(ep)
            .accept(remote::ALPN, remote::ShareProtocol { docs: docs.clone() })
            .spawn()
    });
    let state: SharedState = Arc::new(AppState { docs, storage, serve: args, guest_limiter: Default::default() });
    match &state.serve.public_url {
        Some(u) => tracing::info!("guest mode on: tickets carry public URL {u}"),
        None => tracing::info!("guest mode off (set --public-url to let people without a server join shares)"),
    }

    state.docs.start_presence_mirror();
    state.docs.preload_bridged();

    let app = Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/me", get(ws::whoami))
        .route("/ws/{vault}", get(ws::ws_handler))
        .route("/api/vaults/{vault}/sharing", get(sharing::get_sharing))
        .route("/api/vaults/{vault}/shares", post(sharing::create_share))
        .route("/api/shares/{id}", delete(sharing::delete_share).patch(sharing::move_share))
        .route("/api/shares/{id}/tickets", post(sharing::new_ticket))
        .route("/api/shares/{id}/grants/{label}", delete(sharing::revoke_ticket))
        .route("/api/shares/{id}/grants/{label}/restore", post(sharing::restore_ticket))
        .route("/api/vaults/{vault}/mounts", post(sharing::create_mount))
        .route("/api/mounts/{id}", delete(sharing::delete_mount).patch(sharing::move_mount))
        .route("/api/tickets/inspect", post(sharing::inspect_ticket))
        .route("/guest/shares/{id}", get(guest::info))
        .route("/guest/ws/{id}", get(guest::ws))
        .route(
            "/guest/blobs/{id}/{hash}",
            put(guest::put_blob).get(guest::get_blob).layer(DefaultBodyLimit::max(max_blob)),
        )
        .route(
            "/blobs/{vault}/{hash}",
            put(blobs::put_blob).get(blobs::get_blob).layer(DefaultBodyLimit::max(max_blob)),
        )
        .layer(CorsLayer::new().allow_origin(Any).allow_methods(Any).allow_headers(Any))
        // Span without the query string: guest routes carry the ticket secret there.
        .layer(TraceLayer::new_for_http().make_span_with(|req: &axum::http::Request<_>| {
            tracing::info_span!("request", method = %req.method(), path = %req.uri().path())
        }))
        .with_state(state.clone());

    if state.serve.snapshot_hours > 0 {
        let st = state.clone();
        tokio::spawn(async move {
            let every = Duration::from_secs(st.serve.snapshot_hours * 3600);
            loop {
                tokio::time::sleep(every).await;
                st.docs.snapshot_all(st.serve.snapshot_keep);
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
                .serve(app.into_make_service_with_connect_info::<std::net::SocketAddr>())
                .await?;
        }
        _ => {
            tracing::info!("replica-server listening on http://{}", state.serve.bind);
            tracing::info!("note: iOS needs https; put this behind Caddy/Tailscale or pass --tls-cert/--tls-key");
            axum_server::bind(state.serve.bind)
                .handle(handle)
                .serve(app.into_make_service_with_connect_info::<std::net::SocketAddr>())
                .await?;
        }
    }

    if let Some(r) = router {
        let _ = r.shutdown().await;
    }
    tracing::info!("flushing snapshots before exit");
    state.docs.snapshot_all(state.serve.snapshot_keep);
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
            "vaults_loaded": state.docs.loaded_count(),
            "node_id": state.docs.node_id,
        })),
    )
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}
