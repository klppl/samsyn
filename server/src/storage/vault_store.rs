//! One vault on disk: Yjs update log in SQLite, blobs as files, snapshots as files.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use yrs::updates::decoder::Decode;
use yrs::{Doc, ReadTxn, StateVector, Transact};

use super::registry::VaultInfo;

/// Compact the update log once it has grown past this many rows.
pub const COMPACT_THRESHOLD: i64 = 500;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultMeta {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotManifest {
    pub vault_id: String,
    pub created_at: String,
    pub state_bytes: usize,
    pub text_files: usize,
    pub blob_files: usize,
    pub blob_hashes: Vec<String>,
}

#[derive(Clone)]
pub struct VaultStore {
    dir: PathBuf,
    conn: Arc<Mutex<Connection>>,
}

impl VaultStore {
    pub fn open(dir: &Path, info: &VaultInfo) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir.join("blobs"))?;
        std::fs::create_dir_all(dir.join("snapshots"))?;
        let meta_path = dir.join("vault.json");
        if !meta_path.exists() {
            let meta = VaultMeta {
                id: info.id.clone(),
                user_id: info.user_id.clone(),
                name: info.name.clone(),
                created_at: info.created_at.clone(),
            };
            std::fs::write(&meta_path, serde_json::to_string_pretty(&meta)?)?;
        }
        let conn = Connection::open(dir.join("doc.db"))
            .with_context(|| format!("opening {}", dir.join("doc.db").display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS updates (
                seq        INTEGER PRIMARY KEY AUTOINCREMENT,
                data       BLOB NOT NULL,
                created_at TEXT NOT NULL
            );
            "#,
        )?;
        Ok(Self { dir: dir.to_path_buf(), conn: Arc::new(Mutex::new(conn)) })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn read_meta(dir: &Path) -> anyhow::Result<VaultMeta> {
        Ok(serde_json::from_str(&std::fs::read_to_string(dir.join("vault.json"))?)?)
    }

    // -------------------------------------------------------------- update log

    pub fn load_updates(&self) -> anyhow::Result<Vec<Vec<u8>>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT data FROM updates ORDER BY seq")?;
        let rows = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn append_update(&self, data: &[u8]) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO updates (data, created_at) VALUES (?1, ?2)",
            params![data, chrono::Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn update_count(&self) -> anyhow::Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row("SELECT COUNT(*) FROM updates", [], |r| r.get(0))?)
    }

    pub fn needs_compaction(&self) -> bool {
        self.update_count().map(|n| n > COMPACT_THRESHOLD).unwrap_or(false)
    }

    /// Replace the whole log with one full-state update.
    pub fn compact(&self, full_state: &[u8]) -> anyhow::Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM updates", [])?;
        tx.execute(
            "INSERT INTO updates (data, created_at) VALUES (?1, ?2)",
            params![full_state, chrono::Utc::now().to_rfc3339()],
        )?;
        tx.commit()?;
        let _ = conn.execute_batch("VACUUM");
        Ok(())
    }

    /// Build a fresh in-memory document from the log.
    pub fn load_doc(&self) -> anyhow::Result<(Doc, usize)> {
        let doc = Doc::new();
        let updates = self.load_updates()?;
        let n = updates.len();
        let mut txn = doc.transact_mut();
        for u in updates {
            match yrs::Update::decode_v1(&u) {
                Ok(update) => {
                    if let Err(e) = txn.apply_update(update) {
                        tracing::warn!("skipping corrupt update: {e}");
                    }
                }
                Err(e) => tracing::warn!("skipping undecodable update: {e}"),
            }
        }
        drop(txn);
        Ok((doc, n))
    }

    /// Consistent copy of doc.db for backups.
    pub fn backup_db_to(&self, dest: &Path) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        let mut dst = Connection::open(dest)?;
        let backup = rusqlite::backup::Backup::new(&conn, &mut dst)?;
        backup.run_to_completion(256, std::time::Duration::from_millis(5), None)?;
        Ok(())
    }

    // ------------------------------------------------------------------ blobs

    fn blob_path(&self, hash: &str) -> PathBuf {
        self.dir.join("blobs").join(&hash[..2]).join(hash)
    }

    pub fn has_blob(&self, hash: &str) -> bool {
        self.blob_path(hash).exists()
    }

    pub fn put_blob(&self, hash: &str, data: &[u8]) -> anyhow::Result<()> {
        let path = self.blob_path(hash);
        if path.exists() {
            return Ok(());
        }
        std::fs::create_dir_all(path.parent().unwrap())?;
        // Write to a temp file then rename so readers never see a partial blob.
        let tmp = path.with_extension(format!("tmp-{}", crate::ids::new_id()));
        std::fs::write(&tmp, data)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn get_blob(&self, hash: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let path = self.blob_path(hash);
        match std::fs::read(&path) {
            Ok(d) => Ok(Some(d)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn list_blob_hashes(&self) -> anyhow::Result<Vec<String>> {
        let mut out = Vec::new();
        let root = self.dir.join("blobs");
        if !root.exists() {
            return Ok(out);
        }
        for shard in std::fs::read_dir(&root)? {
            let shard = shard?;
            if !shard.file_type()?.is_dir() {
                continue;
            }
            for f in std::fs::read_dir(shard.path())? {
                let f = f?;
                if let Some(name) = f.file_name().to_str() {
                    if name.len() == 64 {
                        out.push(name.to_string());
                    }
                }
            }
        }
        out.sort();
        Ok(out)
    }

    /// Delete blobs no longer referenced by the document. Call after compaction.
    pub fn gc_blobs(&self, referenced: &std::collections::HashSet<String>) -> anyhow::Result<usize> {
        let mut removed = 0;
        for h in self.list_blob_hashes()? {
            if !referenced.contains(&h) {
                std::fs::remove_file(self.blob_path(&h))?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    // -------------------------------------------------------------- snapshots

    pub fn snapshots_dir(&self) -> PathBuf {
        self.dir.join("snapshots")
    }

    /// Write a full-state snapshot of `doc` plus a manifest. Returns the .ybin path.
    pub fn write_snapshot(&self, doc: &Doc, vault_id: &str) -> anyhow::Result<PathBuf> {
        let txn = doc.transact();
        let state = txn.encode_state_as_update_v1(&StateVector::default());
        let (text_files, blob_hashes) = doc_summary(&txn);
        drop(txn);

        let stamp = chrono::Utc::now().format("%Y-%m-%dT%H-%M-%SZ").to_string();
        let base = self.snapshots_dir().join(&stamp);
        let ybin = base.with_extension("ybin");
        let manifest = SnapshotManifest {
            vault_id: vault_id.to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            state_bytes: state.len(),
            text_files,
            blob_files: blob_hashes.len(),
            blob_hashes,
        };
        let tmp = base.with_extension("ybin.tmp");
        std::fs::write(&tmp, &state)?;
        std::fs::rename(&tmp, &ybin)?;
        std::fs::write(base.with_extension("json"), serde_json::to_string_pretty(&manifest)?)?;
        Ok(ybin)
    }

    pub fn list_snapshots(&self) -> anyhow::Result<Vec<PathBuf>> {
        let mut out: Vec<PathBuf> = std::fs::read_dir(self.snapshots_dir())?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().map(|x| x == "ybin").unwrap_or(false))
            .collect();
        out.sort();
        Ok(out)
    }

    pub fn prune_snapshots(&self, keep: usize) -> anyhow::Result<usize> {
        let snaps = self.list_snapshots()?;
        let mut removed = 0;
        if snaps.len() > keep {
            for p in &snaps[..snaps.len() - keep] {
                let _ = std::fs::remove_file(p.with_extension("json"));
                std::fs::remove_file(p)?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Replace the log with the contents of a snapshot file. Destructive; caller confirms.
    pub fn restore_snapshot(&self, ybin: &Path) -> anyhow::Result<()> {
        let state = std::fs::read(ybin)?;
        yrs::Update::decode_v1(&state).context("snapshot is not a valid Yjs update")?;
        self.compact(&state)
    }
}

/// Count text files and collect blob hashes referenced by the document.
pub fn doc_summary<T: ReadTxn>(txn: &T) -> (usize, Vec<String>) {
    use yrs::{Any, Map, Out};
    let texts = txn.get_map("texts").map(|m| m.len(txn) as usize).unwrap_or(0);
    let mut hashes = Vec::new();
    if let Some(blobs) = txn.get_map("blobs") {
        for (_, v) in blobs.iter(txn) {
            if let Out::Any(Any::Map(m)) = v {
                if let Some(Any::String(h)) = m.get("hash") {
                    hashes.push(h.to_string());
                }
            }
        }
    }
    hashes.sort();
    hashes.dedup();
    (texts, hashes)
}
