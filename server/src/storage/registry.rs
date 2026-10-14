//! Users, tokens and the vault index. One small SQLite file.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use rusqlite::{params, Connection, OptionalExtension};

use crate::ids::{new_id, new_token, token_hash, valid_name};

#[derive(Debug, Clone)]
pub struct User {
    pub id: String,
    pub name: String,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct VaultInfo {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct TokenInfo {
    pub label: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
}

#[derive(Clone)]
pub struct Registry {
    conn: Arc<Mutex<Connection>>,
}

impl Registry {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS users (
                id         TEXT PRIMARY KEY,
                name       TEXT NOT NULL UNIQUE,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS tokens (
                hash         TEXT PRIMARY KEY,
                user_id      TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                label        TEXT NOT NULL,
                created_at   TEXT NOT NULL,
                last_used_at TEXT
            );
            CREATE INDEX IF NOT EXISTS tokens_user ON tokens(user_id);
            CREATE TABLE IF NOT EXISTS vaults (
                id         TEXT PRIMARY KEY,
                user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                name       TEXT NOT NULL,
                created_at TEXT NOT NULL,
                UNIQUE (user_id, name)
            );
            -- Reserved for shared folders (server-to-server). Not used yet; see docs/STORAGE.md.
            CREATE TABLE IF NOT EXISTS mounts (
                id            TEXT PRIMARY KEY,
                vault_id      TEXT NOT NULL REFERENCES vaults(id) ON DELETE CASCADE,
                mount_path    TEXT NOT NULL,
                share_id      TEXT NOT NULL,
                remote_server TEXT,
                created_at    TEXT NOT NULL
            );
            "#,
        )?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    fn now() -> String {
        chrono::Utc::now().to_rfc3339()
    }

    // ------------------------------------------------------------------ users

    pub fn create_user(&self, name: &str) -> anyhow::Result<User> {
        anyhow::ensure!(valid_name(name), "invalid user name (use letters, digits, - _ .)");
        let user = User { id: new_id(), name: name.to_string(), created_at: Self::now() };
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO users (id, name, created_at) VALUES (?1, ?2, ?3)",
            params![user.id, user.name, user.created_at],
        )
        .with_context(|| format!("creating user {name} (already exists?)"))?;
        Ok(user)
    }

    pub fn user_by_name(&self, name: &str) -> anyhow::Result<Option<User>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT id, name, created_at FROM users WHERE name = ?1",
                params![name],
                |r| Ok(User { id: r.get(0)?, name: r.get(1)?, created_at: r.get(2)? }),
            )
            .optional()?)
    }

    pub fn user_by_id(&self, id: &str) -> anyhow::Result<Option<User>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT id, name, created_at FROM users WHERE id = ?1",
                params![id],
                |r| Ok(User { id: r.get(0)?, name: r.get(1)?, created_at: r.get(2)? }),
            )
            .optional()?)
    }

    pub fn list_users(&self) -> anyhow::Result<Vec<User>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id, name, created_at FROM users ORDER BY name")?;
        let rows = stmt.query_map([], |r| Ok(User { id: r.get(0)?, name: r.get(1)?, created_at: r.get(2)? }))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn user_count(&self) -> anyhow::Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?)
    }

    pub fn delete_user(&self, id: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute("DELETE FROM users WHERE id = ?1", params![id])? > 0)
    }

    // ----------------------------------------------------------------- tokens

    /// Issue a new token for a user. The plaintext is returned once and never stored.
    pub fn create_token(&self, user_id: &str, label: &str) -> anyhow::Result<String> {
        let token = new_token();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO tokens (hash, user_id, label, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![token_hash(&token), user_id, label, Self::now()],
        )?;
        Ok(token)
    }

    /// Resolve a presented token to its user, updating `last_used_at`.
    pub fn authenticate(&self, token: &str) -> anyhow::Result<Option<User>> {
        let hash = token_hash(token);
        let conn = self.conn.lock().unwrap();
        let user = conn
            .query_row(
                "SELECT u.id, u.name, u.created_at FROM tokens t JOIN users u ON u.id = t.user_id WHERE t.hash = ?1",
                params![hash],
                |r| Ok(User { id: r.get(0)?, name: r.get(1)?, created_at: r.get(2)? }),
            )
            .optional()?;
        if user.is_some() {
            let _ = conn.execute(
                "UPDATE tokens SET last_used_at = ?1 WHERE hash = ?2",
                params![Self::now(), hash],
            );
        }
        Ok(user)
    }

    pub fn list_tokens(&self, user_id: &str) -> anyhow::Result<Vec<TokenInfo>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT label, created_at, last_used_at FROM tokens WHERE user_id = ?1 ORDER BY created_at")?;
        let rows = stmt.query_map(params![user_id], |r| {
            Ok(TokenInfo { label: r.get(0)?, created_at: r.get(1)?, last_used_at: r.get(2)? })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn revoke_tokens(&self, user_id: &str, label: Option<&str>) -> anyhow::Result<usize> {
        let conn = self.conn.lock().unwrap();
        Ok(match label {
            Some(l) => conn.execute("DELETE FROM tokens WHERE user_id = ?1 AND label = ?2", params![user_id, l])?,
            None => conn.execute("DELETE FROM tokens WHERE user_id = ?1", params![user_id])?,
        })
    }

    // ----------------------------------------------------------------- vaults

    pub fn get_or_create_vault(&self, user_id: &str, name: &str) -> anyhow::Result<VaultInfo> {
        anyhow::ensure!(valid_name(name), "invalid vault name");
        let conn = self.conn.lock().unwrap();
        if let Some(v) = conn
            .query_row(
                "SELECT id, user_id, name, created_at FROM vaults WHERE user_id = ?1 AND name = ?2",
                params![user_id, name],
                row_to_vault,
            )
            .optional()?
        {
            return Ok(v);
        }
        let v = VaultInfo { id: new_id(), user_id: user_id.to_string(), name: name.to_string(), created_at: Self::now() };
        conn.execute(
            "INSERT INTO vaults (id, user_id, name, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![v.id, v.user_id, v.name, v.created_at],
        )?;
        Ok(v)
    }

    pub fn vault_by_id(&self, id: &str) -> anyhow::Result<Option<VaultInfo>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row("SELECT id, user_id, name, created_at FROM vaults WHERE id = ?1", params![id], row_to_vault)
            .optional()?)
    }

    pub fn list_vaults(&self, user_id: Option<&str>) -> anyhow::Result<Vec<VaultInfo>> {
        let conn = self.conn.lock().unwrap();
        let mut out = Vec::new();
        match user_id {
            Some(u) => {
                let mut stmt =
                    conn.prepare("SELECT id, user_id, name, created_at FROM vaults WHERE user_id = ?1 ORDER BY name")?;
                for r in stmt.query_map(params![u], row_to_vault)? {
                    out.push(r?);
                }
            }
            None => {
                let mut stmt = conn.prepare("SELECT id, user_id, name, created_at FROM vaults ORDER BY user_id, name")?;
                for r in stmt.query_map([], row_to_vault)? {
                    out.push(r?);
                }
            }
        }
        Ok(out)
    }

    pub fn delete_vault(&self, id: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute("DELETE FROM vaults WHERE id = ?1", params![id])? > 0)
    }

    /// Re-insert a vault row (used by restore when the registry lacks a vault found on disk).
    pub fn upsert_vault(&self, v: &VaultInfo) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO vaults (id, user_id, name, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![v.id, v.user_id, v.name, v.created_at],
        )?;
        Ok(())
    }

    pub fn upsert_user(&self, u: &User) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO users (id, name, created_at) VALUES (?1, ?2, ?3)",
            params![u.id, u.name, u.created_at],
        )?;
        Ok(())
    }

    /// Consistent copy of the registry for backups (SQLite online backup API).
    pub fn backup_to(&self, dest: &Path) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        let mut dst = Connection::open(dest)?;
        let backup = rusqlite::backup::Backup::new(&conn, &mut dst)?;
        backup.run_to_completion(256, std::time::Duration::from_millis(5), None)?;
        Ok(())
    }
}

fn row_to_vault(r: &rusqlite::Row) -> rusqlite::Result<VaultInfo> {
    Ok(VaultInfo { id: r.get(0)?, user_id: r.get(1)?, name: r.get(2)?, created_at: r.get(3)? })
}
