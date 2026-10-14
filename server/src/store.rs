//! SQLite persistence: append-only Yjs updates per vault, compacted on load, plus blobs.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use rusqlite::{params, Connection, OptionalExtension};

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

/// Compact a vault's update log once it has this many rows.
const COMPACT_THRESHOLD: i64 = 500;

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS updates (
                seq   INTEGER PRIMARY KEY AUTOINCREMENT,
                vault TEXT NOT NULL,
                data  BLOB NOT NULL
            );
            CREATE INDEX IF NOT EXISTS updates_vault ON updates(vault, seq);
            CREATE TABLE IF NOT EXISTS blobs (
                vault TEXT NOT NULL,
                hash  TEXT NOT NULL,
                data  BLOB NOT NULL,
                created_at INTEGER NOT NULL,
                PRIMARY KEY (vault, hash)
            );
            "#,
        )?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    pub fn load_updates(&self, vault: &str) -> anyhow::Result<Vec<Vec<u8>>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT data FROM updates WHERE vault = ?1 ORDER BY seq")?;
        let rows = stmt.query_map(params![vault], |r| r.get::<_, Vec<u8>>(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn append_update(&self, vault: &str, data: &[u8]) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO updates (vault, data) VALUES (?1, ?2)",
            params![vault, data],
        )?;
        Ok(())
    }

    pub fn update_count(&self, vault: &str) -> anyhow::Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM updates WHERE vault = ?1",
            params![vault],
            |r| r.get(0),
        )?)
    }

    /// Replace the whole update log of a vault with a single full-state update.
    pub fn compact(&self, vault: &str, full_state: &[u8]) -> anyhow::Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM updates WHERE vault = ?1", params![vault])?;
        tx.execute(
            "INSERT INTO updates (vault, data) VALUES (?1, ?2)",
            params![vault, full_state],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn needs_compaction(&self, vault: &str) -> bool {
        self.update_count(vault).map(|n| n > COMPACT_THRESHOLD).unwrap_or(false)
    }

    pub fn put_blob(&self, vault: &str, hash: &str, data: &[u8]) -> anyhow::Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO blobs (vault, hash, data, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![vault, hash, data, now],
        )?;
        Ok(())
    }

    pub fn get_blob(&self, vault: &str, hash: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT data FROM blobs WHERE vault = ?1 AND hash = ?2",
                params![vault, hash],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .optional()?)
    }
}
