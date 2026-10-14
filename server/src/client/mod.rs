//! Headless sync client: keeps a plain directory in sync with a vault on a replica server.
//! Same document model and reconcile rules as the Obsidian plugin, driven by a filesystem
//! watcher instead of editor events.

pub mod diff;
mod engine;
mod state;
mod ws;

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use tokio::sync::mpsc;

use self::engine::Engine;
use self::ws::WsEvent;

#[derive(Parser, Debug, Clone)]
#[command(name = "sync")]
pub struct SyncArgs {
    /// Server base URL, e.g. https://sync.example.com or http://127.0.0.1:8787
    #[arg(long, env = "REPLICA_SERVER")]
    pub server: String,

    /// User token (rp_...).
    #[arg(long, env = "REPLICA_TOKEN")]
    pub token: String,

    /// Vault name on the server (same "Vault ID" as in the plugin).
    #[arg(long, env = "REPLICA_VAULT")]
    pub vault: String,

    /// Local directory to keep in sync.
    #[arg(long, env = "REPLICA_DIR", default_value = ".")]
    pub dir: PathBuf,

    /// Comma separated extensions merged as text; everything else is a whole-file blob.
    #[arg(long, env = "REPLICA_TEXT_EXT", default_value = "md,txt")]
    pub text_ext: String,

    /// Path prefixes to skip (repeatable). Dotfiles and .replica/ are always skipped.
    #[arg(long)]
    pub exclude: Vec<String>,

    /// Do not sync non-text files.
    #[arg(long)]
    pub no_binary: bool,

    /// Quiet time after a file change before it is pushed (ms).
    #[arg(long, default_value_t = 300)]
    pub debounce_ms: u64,

    /// Let a reconcile remove many files from the shared vault (otherwise held back).
    #[arg(long)]
    pub allow_mass_delete: bool,

    /// Name shown to other devices.
    #[arg(long, env = "REPLICA_DEVICE")]
    pub device: Option<String>,

    /// Sync once (connect, reconcile, flush) and exit. Handy for cron.
    #[arg(long)]
    pub once: bool,
}

impl SyncArgs {
    pub fn http_base(&self) -> String {
        let mut s = self.server.trim().trim_end_matches('/').to_string();
        if !s.contains("://") {
            s = format!("http://{s}");
        }
        if let Some(rest) = s.strip_prefix("ws://") {
            s = format!("http://{rest}");
        } else if let Some(rest) = s.strip_prefix("wss://") {
            s = format!("https://{rest}");
        }
        s
    }

    pub fn ws_url(&self) -> String {
        let http = self.http_base();
        let ws = if let Some(rest) = http.strip_prefix("https://") {
            format!("wss://{rest}")
        } else {
            format!("ws://{}", http.trim_start_matches("http://"))
        };
        format!("{ws}/ws/{}?token={}", self.vault, self.token)
    }
}

pub async fn run(args: SyncArgs) -> anyhow::Result<()> {
    anyhow::ensure!(args.dir.is_dir(), "{} is not a directory", args.dir.display());
    let mut engine = Engine::new(&args)?;

    let (ws_tx, mut ws_rx) = mpsc::unbounded_channel::<WsEvent>();
    ws::spawn(args.ws_url(), ws_tx);

    let (fs_tx, mut fs_rx) = mpsc::unbounded_channel::<Vec<PathBuf>>();
    let _watcher = engine.start_watcher(fs_tx)?;

    // A directory with no cached doc waits for the server before reconciling, so files that
    // already exist on both sides merge instead of duplicating. Otherwise reconcile right away.
    let mut reconciled = false;
    let first_sync_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    if engine.had_cached_doc {
        engine.reconcile().await?;
        reconciled = true;
    }

    let mut save_tick = tokio::time::interval(Duration::from_secs(2));
    let mut idle_since: Option<tokio::time::Instant> = None;
    tracing::info!("syncing {} <-> {}/{}", args.dir.display(), args.http_base(), args.vault);

    loop {
        tokio::select! {
            ev = ws_rx.recv() => match ev {
                Some(WsEvent::Connected(tx)) => engine.on_connected(tx),
                Some(WsEvent::Message(bytes)) => {
                    engine.on_message(&bytes).await;
                    if !reconciled && engine.synced {
                        engine.reconcile().await?;
                        reconciled = true;
                    }
                    idle_since = Some(tokio::time::Instant::now());
                }
                Some(WsEvent::Disconnected) => engine.on_disconnected(),
                None => break,
            },
            Some(paths) = fs_rx.recv() => {
                if engine.on_fs_batch(&paths).await {
                    idle_since = Some(tokio::time::Instant::now());
                }
            }
            _ = save_tick.tick() => {
                engine.flush_persistence();
                if !reconciled && tokio::time::Instant::now() > first_sync_deadline {
                    tracing::warn!("no server state after 10s; reconciling offline");
                    engine.reconcile().await?;
                    reconciled = true;
                }
                if args.once && reconciled && engine.synced {
                    let quiet = idle_since.map(|t| t.elapsed() > Duration::from_secs(2)).unwrap_or(true);
                    if quiet {
                        tracing::info!("once: synced and idle, exiting");
                        break;
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutting down");
                break;
            }
        }
    }
    engine.shutdown();
    Ok(())
}
