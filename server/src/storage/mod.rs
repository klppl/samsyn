//! On-disk storage model.
//!
//! ```text
//! <data-dir>/
//!   server.json                 instance id + schema version
//!   registry.db                 users, tokens (hashed), vault index
//!   users/<user_id>/vaults/<vault_id>/
//!     vault.json                owner, name, created_at (human-readable, for recovery)
//!     doc.db                    append-only Yjs update log for this vault
//!     blobs/<aa>/<hash>         immutable content-addressed binary files
//!     snapshots/<ts>.ybin       full document state at a point in time
//!     snapshots/<ts>.json       snapshot manifest
//! ```
//!
//! Every vault is a self-contained directory: copy it, encrypt it, or restore it on its own.
//! Nothing in one user's tree references another user's tree. Future shared folders between
//! users (server-to-server) are modelled as separate "share" documents that a vault mounts;
//! they live under their owner's tree and are referenced by id, see docs/STORAGE.md.

pub mod backup;
pub mod registry;
pub mod vault_store;

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

pub use registry::{MountInfo, Registry, ShareInfo, User, VaultInfo};
pub use vault_store::VaultStore;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerMeta {
    pub instance_id: String,
    pub schema_version: u32,
    pub created_at: String,
}

#[derive(Clone)]
pub struct Storage {
    root: PathBuf,
    registry: Registry,
}

impl Storage {
    pub fn open(root: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
        let meta_path = root.join("server.json");
        if !meta_path.exists() {
            let meta = ServerMeta {
                instance_id: crate::ids::new_id(),
                schema_version: SCHEMA_VERSION,
                created_at: chrono::Utc::now().to_rfc3339(),
            };
            std::fs::write(&meta_path, serde_json::to_string_pretty(&meta)?)?;
        } else {
            let meta: ServerMeta = serde_json::from_str(&std::fs::read_to_string(&meta_path)?)?;
            anyhow::ensure!(
                meta.schema_version <= SCHEMA_VERSION,
                "data dir schema {} is newer than this server supports ({})",
                meta.schema_version,
                SCHEMA_VERSION
            );
        }
        let registry = Registry::open(&root.join("registry.db"))?;
        Ok(Self { root: root.to_path_buf(), registry })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    pub fn meta(&self) -> anyhow::Result<ServerMeta> {
        Ok(serde_json::from_str(&std::fs::read_to_string(self.root.join("server.json"))?)?)
    }

    pub fn vault_dir(&self, user_id: &str, vault_id: &str) -> PathBuf {
        self.root.join("users").join(user_id).join("vaults").join(vault_id)
    }

    pub fn share_dir(&self, user_id: &str, share_id: &str) -> PathBuf {
        self.root.join("users").join(user_id).join("shares").join(share_id)
    }

    /// Store for a share this server owns.
    pub fn share_store(&self, info: &ShareInfo) -> anyhow::Result<VaultStore> {
        let dir = self.share_dir(&info.user_id, &info.id);
        let as_vault = VaultInfo {
            id: info.id.clone(),
            user_id: info.user_id.clone(),
            name: format!("share:{}", info.name),
            created_at: info.created_at.clone(),
        };
        VaultStore::open(&dir, &as_vault)
    }

    /// This server's long-lived identity (ed25519). Public half is the node id in tickets.
    pub fn identity(&self) -> anyhow::Result<iroh_base::SecretKey> {
        let path = self.root.join("identity.key");
        if path.exists() {
            let hex_key = std::fs::read_to_string(&path)?;
            let bytes = hex::decode(hex_key.trim()).context("identity.key is not hex")?;
            let arr: [u8; 32] = bytes.as_slice().try_into().context("identity.key has wrong length")?;
            return Ok(iroh_base::SecretKey::from_bytes(&arr));
        }
        let key = iroh_base::SecretKey::generate();
        std::fs::write(&path, hex::encode(key.to_bytes()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        tracing::info!("generated server identity {}", key.public());
        Ok(key)
    }

    /// Open (creating on disk if needed) the store for a vault already present in the registry.
    pub fn vault_store(&self, info: &VaultInfo) -> anyhow::Result<VaultStore> {
        let dir = self.vault_dir(&info.user_id, &info.id);
        VaultStore::open(&dir, info)
    }

    /// Resolve `user + vault name` to a store, creating the vault on first use.
    pub fn open_vault_for_user(&self, user: &User, vault_name: &str) -> anyhow::Result<(VaultInfo, VaultStore)> {
        let info = self.registry.get_or_create_vault(&user.id, vault_name)?;
        let store = self.vault_store(&info)?;
        Ok((info, store))
    }
}
