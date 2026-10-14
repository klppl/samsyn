use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use rand::RngCore;

/// Command line / environment configuration.
#[derive(Parser, Debug, Clone)]
#[command(name = "replica-server", version, about = "CRDT sync server for the Replica Obsidian plugin")]
pub struct Config {
    /// Address to listen on.
    #[arg(long, env = "REPLICA_BIND", default_value = "0.0.0.0:8787")]
    pub bind: SocketAddr,

    /// Directory for the SQLite database and generated token.
    #[arg(long, env = "REPLICA_DATA_DIR", default_value = "./data")]
    pub data_dir: PathBuf,

    /// Shared secret clients must present. Generated and persisted on first start if omitted.
    #[arg(long, env = "REPLICA_TOKEN", default_value = "")]
    pub token: String,

    /// Maximum blob size in megabytes.
    #[arg(long, env = "REPLICA_MAX_BLOB_MB", default_value_t = 256)]
    pub max_blob_mb: usize,
}

impl Config {
    /// Create the data dir and load or generate the token.
    pub fn resolve(mut self) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&self.data_dir)
            .with_context(|| format!("creating data dir {}", self.data_dir.display()))?;

        if self.token.is_empty() {
            let token_file = self.data_dir.join("token");
            if token_file.exists() {
                self.token = std::fs::read_to_string(&token_file)?.trim().to_string();
            } else {
                self.token = generate_token();
                std::fs::write(&token_file, &self.token)?;
                tracing::info!("generated new auth token at {}", token_file.display());
            }
        }
        Ok(self)
    }

    pub fn token_matches(&self, presented: Option<&str>) -> bool {
        match presented {
            Some(t) => constant_time_eq(t.as_bytes(), self.token.as_bytes()),
            None => false,
        }
    }
}

fn generate_token() -> String {
    let mut bytes = [0u8; 24];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Extract a bearer token from either the `Authorization` header or the `token` query param.
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
