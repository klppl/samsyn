//! Per-directory sync bookkeeping in `<dir>/.samsyn/`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use yrs::updates::decoder::Decode;
use yrs::{Doc, ReadTxn, StateVector, Transact};

pub const META_DIR: &str = ".samsyn";

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct LocalState {
    #[serde(default)]
    pub install_id: String,
    /// path -> content hash the last time disk and shared document agreed
    #[serde(default)]
    pub synced: HashMap<String, String>,
}

pub struct StateStore {
    dir: PathBuf,
}

impl StateStore {
    pub fn new(root: &Path) -> anyhow::Result<Self> {
        let dir = root.join(META_DIR);
        std::fs::create_dir_all(dir.join("trash"))?;
        Ok(Self { dir })
    }

    fn state_path(&self) -> PathBuf {
        self.dir.join("state.json")
    }
    fn doc_path(&self) -> PathBuf {
        self.dir.join("doc.bin")
    }
    pub fn trash_dir(&self) -> PathBuf {
        self.dir.join("trash")
    }

    /// Bound to machine + absolute directory so a copied folder starts with fresh bookkeeping.
    pub fn install_id(root: &Path) -> String {
        let host = gethostname::gethostname().to_string_lossy().to_string();
        let abs = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let h = Sha256::digest(format!("{host}|{}", abs.display()).as_bytes());
        hex::encode(&h[..8])
    }

    pub fn load_state(&self, install_id: &str) -> LocalState {
        let fresh = || LocalState { install_id: install_id.to_string(), synced: HashMap::new() };
        match std::fs::read_to_string(self.state_path()) {
            Ok(s) => match serde_json::from_str::<LocalState>(&s) {
                Ok(st) if st.install_id == install_id => st,
                Ok(st) => {
                    tracing::warn!(
                        "sync state belongs to another install ({} vs {install_id}); starting fresh",
                        st.install_id
                    );
                    fresh()
                }
                Err(e) => {
                    tracing::warn!("unreadable state.json ({e}); starting fresh");
                    fresh()
                }
            },
            Err(_) => fresh(),
        }
    }

    pub fn save_state(&self, st: &LocalState) -> anyhow::Result<()> {
        write_atomic(&self.state_path(), serde_json::to_string_pretty(st)?.as_bytes())
    }

    /// Returns true if a cached document existed.
    pub fn load_doc(&self, doc: &Doc) -> bool {
        match std::fs::read(self.doc_path()) {
            Ok(bytes) => match yrs::Update::decode_v1(&bytes) {
                Ok(u) => {
                    let mut txn = doc.transact_mut_with("persistence");
                    if let Err(e) = txn.apply_update(u) {
                        tracing::warn!("cached doc.bin could not be applied: {e}");
                        return false;
                    }
                    true
                }
                Err(e) => {
                    tracing::warn!("cached doc.bin is corrupt ({e}); ignoring");
                    false
                }
            },
            Err(_) => false,
        }
    }

    pub fn save_doc(&self, doc: &Doc) -> anyhow::Result<()> {
        let bytes = doc.transact().encode_state_as_update_v1(&StateVector::default());
        write_atomic(&self.doc_path(), &bytes)
    }
}

pub fn write_atomic(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("{}.samsyn-tmp", path.extension().and_then(|e| e.to_str()).unwrap_or("")));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn hash_bytes(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}
