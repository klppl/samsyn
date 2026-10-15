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
    #[arg(long, env = "REPLICA_SERVER", required_unless_present = "ticket")]
    pub server: Option<String>,

    /// User token (rp_...).
    #[arg(long, env = "REPLICA_TOKEN", required_unless_present = "ticket")]
    pub token: Option<String>,

    /// Vault name on the server (same "Vault ID" as in the plugin).
    #[arg(long, env = "REPLICA_VAULT", required_unless_present = "ticket")]
    pub vault: Option<String>,

    /// Guest mode: sync one shared folder or note using a ticket instead of a server account.
    /// The ticket must come from a server started with --public-url.
    #[arg(long, env = "REPLICA_TICKET", conflicts_with_all = ["server", "token", "vault"])]
    pub ticket: Option<String>,

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

/// What we sync against: a vault on our own server, or a share on the owner's server (guest).
#[derive(Debug, Clone)]
pub enum Target {
    Vault { http: String, token: String, vault: String },
    Guest { http: String, share_id: String, secret: String, name: String },
}

fn normalize_http(raw: &str) -> String {
    let mut s = raw.trim().trim_end_matches('/').to_string();
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

impl SyncArgs {
    pub fn target(&self) -> anyhow::Result<Target> {
        if let Some(t) = &self.ticket {
            let t = crate::ticket::Ticket::decode(t)?;
            anyhow::ensure!(
                !t.url.is_empty(),
                "this ticket has no server URL (the owner's server runs without --public-url); it can only be mounted through a replica server"
            );
            return Ok(Target::Guest { http: normalize_http(&t.url), share_id: t.share_id, secret: t.secret, name: t.name });
        }
        Ok(Target::Vault {
            http: normalize_http(self.server.as_deref().unwrap_or_default()),
            token: self.token.clone().unwrap_or_default(),
            vault: self.vault.clone().unwrap_or_default(),
        })
    }

    pub fn http_base(&self) -> String {
        match self.target() {
            Ok(Target::Vault { http, .. }) | Ok(Target::Guest { http, .. }) => http,
            Err(_) => String::new(),
        }
    }

    /// Credential for HTTP calls: the user token, or the ticket secret for guests.
    pub fn bearer(&self) -> String {
        match self.target() {
            Ok(Target::Vault { token, .. }) => token,
            Ok(Target::Guest { secret, .. }) => secret,
            Err(_) => String::new(),
        }
    }

    /// Human-readable name of what we sync against, for logs.
    pub fn label(&self) -> String {
        match self.target() {
            Ok(Target::Vault { http, vault, .. }) => format!("{http}/{vault}"),
            Ok(Target::Guest { http, name, .. }) => format!("{http} share \"{name}\" (guest)"),
            Err(_) => "?".into(),
        }
    }

    pub fn is_guest(&self) -> bool {
        self.ticket.is_some()
    }

    pub fn ws_url(&self) -> String {
        let http = self.http_base();
        let ws = if let Some(rest) = http.strip_prefix("https://") {
            format!("wss://{rest}")
        } else {
            format!("ws://{}", http.trim_start_matches("http://"))
        };
        match self.target() {
            Ok(Target::Guest { share_id, secret, .. }) => format!("{ws}/guest/ws/{share_id}?secret={secret}"),
            Ok(Target::Vault { token, vault, .. }) => format!("{ws}/ws/{vault}?token={token}"),
            Err(_) => String::new(),
        }
    }

    pub fn blob_url(&self, hash: &str) -> String {
        match self.target() {
            Ok(Target::Guest { http, share_id, .. }) => format!("{http}/guest/blobs/{share_id}/{hash}"),
            Ok(Target::Vault { http, vault, .. }) => format!("{http}/blobs/{vault}/{hash}"),
            Err(_) => String::new(),
        }
    }

    /// Where to learn about read-only status: the vault's sharing list, or the share's guest info.
    pub fn sharing_url(&self) -> String {
        match self.target() {
            Ok(Target::Guest { http, share_id, .. }) => format!("{http}/guest/shares/{share_id}"),
            Ok(Target::Vault { http, vault, .. }) => format!("{http}/api/vaults/{vault}/sharing"),
            Err(_) => String::new(),
        }
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
    let mut sharing_tick = tokio::time::interval(Duration::from_secs(20));
    sharing_tick.tick().await; // first refresh happens on connect
    let mut idle_since: Option<tokio::time::Instant> = None;
    tracing::info!("syncing {} <-> {}", args.dir.display(), args.label());

    loop {
        tokio::select! {
            ev = ws_rx.recv() => match ev {
                Some(WsEvent::Connected(tx)) => {
                    engine.on_connected(tx);
                    engine.refresh_sharing().await;
                }
                Some(WsEvent::Message(bytes)) => {
                    engine.on_message(&bytes).await;
                    if !reconciled && engine.synced {
                        engine.reconcile().await?;
                        reconciled = true;
                    }
                    idle_since = Some(tokio::time::Instant::now());
                }
                Some(WsEvent::Disconnected) => engine.on_disconnected(),
                Some(WsEvent::Fatal(msg)) => {
                    engine.on_disconnected();
                    engine.flush_persistence();
                    anyhow::bail!("{msg}; check the token or ticket (a revoked ticket stays revoked)");
                }
                None => break,
            },
            Some(paths) = fs_rx.recv() => {
                if engine.on_fs_batch(&paths).await {
                    idle_since = Some(tokio::time::Instant::now());
                }
            }
            _ = sharing_tick.tick() => {
                if engine.synced {
                    engine.refresh_sharing().await;
                }
            }
            _ = save_tick.tick() => {
                engine.retry_pending_blobs().await;
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
