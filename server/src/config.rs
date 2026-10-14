use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug, Clone)]
#[command(name = "replica-server", version, about = "CRDT sync server for the Replica Obsidian plugin")]
pub struct Cli {
    /// Directory holding all server data (registry, vaults, blobs, snapshots).
    #[arg(long, global = true, env = "REPLICA_DATA_DIR", default_value = "./data")]
    pub data_dir: PathBuf,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// Run the sync server (default when no subcommand is given).
    Serve(ServeArgs),
    /// Manage users and their tokens.
    User {
        #[command(subcommand)]
        cmd: UserCmd,
    },
    /// Inspect vaults.
    Vault {
        #[command(subcommand)]
        cmd: VaultCmd,
    },
    /// Write a full-state snapshot of one vault (or all) and compact its log.
    Snapshot {
        /// `user/vault`; omit for every vault.
        target: Option<String>,
        /// Snapshots to keep per vault after pruning.
        #[arg(long, default_value_t = 14)]
        keep: usize,
    },
    /// Create a tar.gz backup of the data directory (safe while the server runs).
    Backup {
        /// Output file, or `-` for stdout (pipe into age/gpg to encrypt).
        #[arg(long, short)]
        out: Option<String>,
        /// Only this user's vaults.
        #[arg(long)]
        user: Option<String>,
        /// Only this vault name (with --user).
        #[arg(long)]
        vault: Option<String>,
    },
    /// Restore a tar.gz backup into the data directory. Stop the server first.
    Restore {
        /// Input file, or `-` for stdin.
        #[arg(long, short)]
        from: String,
        /// Allow restoring into a non-empty data dir (adds/overwrites the vaults in the archive).
        #[arg(long)]
        merge: bool,
    },
    /// Roll one vault back to a snapshot file. Destructive; stop the server first.
    RestoreSnapshot {
        /// `user/vault`
        target: String,
        /// Path to a `.ybin` snapshot.
        file: PathBuf,
        /// Confirm you understand the current log is replaced.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Parser, Debug, Clone)]
#[command(name = "serve")]
pub struct ServeArgs {
    /// Address to listen on.
    #[arg(long, env = "REPLICA_BIND", default_value = "0.0.0.0:8787")]
    pub bind: SocketAddr,

    /// Maximum blob size in megabytes.
    #[arg(long, env = "REPLICA_MAX_BLOB_MB", default_value_t = 256)]
    pub max_blob_mb: usize,

    /// Hours between automatic snapshots of active vaults (0 disables).
    #[arg(long, env = "REPLICA_SNAPSHOT_HOURS", default_value_t = 24)]
    pub snapshot_hours: u64,

    /// Automatic snapshots to keep per vault.
    #[arg(long, env = "REPLICA_SNAPSHOT_KEEP", default_value_t = 14)]
    pub snapshot_keep: usize,

    /// PEM certificate chain. With --tls-key, serve HTTPS/WSS directly (required for iOS).
    #[arg(long, env = "REPLICA_TLS_CERT", requires = "tls_key")]
    pub tls_cert: Option<PathBuf>,

    /// PEM private key for --tls-cert.
    #[arg(long, env = "REPLICA_TLS_KEY", requires = "tls_cert")]
    pub tls_key: Option<PathBuf>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum UserCmd {
    /// Create a user and print their first token.
    Add { name: String },
    /// List users.
    List,
    /// Issue an additional token for a user (e.g. one per device).
    Token {
        name: String,
        #[arg(long, default_value = "device")]
        label: String,
    },
    /// Revoke tokens of a user (all, or one label).
    Revoke {
        name: String,
        #[arg(long)]
        label: Option<String>,
    },
    /// Delete a user from the registry. Their vault directories are left on disk.
    Remove { name: String },
}

#[derive(Subcommand, Debug, Clone)]
pub enum VaultCmd {
    /// List vaults, optionally for one user.
    List {
        #[arg(long)]
        user: Option<String>,
    },
}

/// Extract a bearer token from the `Authorization` header or the `token` query param.
/// Obsidian mobile cannot set WebSocket headers, hence the query param fallback.
pub fn extract_token(headers: &axum::http::HeaderMap, query_token: Option<&str>) -> Option<String> {
    if let Some(v) = headers.get(axum::http::header::AUTHORIZATION) {
        if let Ok(s) = v.to_str() {
            if let Some(rest) = s.strip_prefix("Bearer ") {
                return Some(rest.trim().to_string());
            }
        }
    }
    query_token.map(|s| s.to_string())
}
