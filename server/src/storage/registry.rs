//! Users, tokens and the vault index. One small SQLite file.

use std::path::Path;
use std::sync::Arc;
use parking_lot::Mutex;

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

#[derive(Debug, Clone, serde::Serialize)]
pub struct ShareInfo {
    pub id: String,
    pub user_id: String,
    pub vault_id: String,
    pub name: String,
    /// Folder prefix with trailing slash, or "" for the vault root.
    pub prefix: String,
    /// Set when the share is a single note inside `prefix`.
    pub only_file: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GrantInfo {
    pub label: String,
    pub access: String,
    pub created_at: String,
    pub revoked_at: Option<String>,
    /// Last time a guest client authenticated with this ticket.
    pub last_seen_at: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MountInfo {
    pub id: String,
    pub user_id: String,
    pub vault_id: String,
    pub share_id: String,
    pub share_name: String,
    pub mount_prefix: String,
    pub access: String,
    pub home_node: String,
    /// Owner server's relay/direct addresses as printed in the ticket (remote mounts).
    pub home_addrs: Vec<String>,
    /// Set when the share is a single note (remote mounts; local ones read it from the share).
    pub only_file: Option<String>,
    #[serde(skip_serializing)]
    pub secret: String,
    pub created_at: String,
    pub revoked_at: Option<String>,
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
            CREATE TABLE IF NOT EXISTS shares (
                id         TEXT PRIMARY KEY,
                user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                vault_id   TEXT NOT NULL REFERENCES vaults(id) ON DELETE CASCADE,
                name       TEXT NOT NULL,
                prefix     TEXT NOT NULL,
                only_file  TEXT,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS share_grants (
                share_id    TEXT NOT NULL REFERENCES shares(id) ON DELETE CASCADE,
                secret_hash TEXT NOT NULL,
                label       TEXT NOT NULL,
                access      TEXT NOT NULL,
                created_at  TEXT NOT NULL,
                revoked_at  TEXT,
                PRIMARY KEY (share_id, label)
            );
            CREATE TABLE IF NOT EXISTS mounts (
                id           TEXT PRIMARY KEY,
                user_id      TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                vault_id     TEXT NOT NULL REFERENCES vaults(id) ON DELETE CASCADE,
                share_id     TEXT NOT NULL,
                share_name   TEXT NOT NULL,
                mount_prefix TEXT NOT NULL,
                access       TEXT NOT NULL,
                home_node    TEXT NOT NULL,
                secret       TEXT NOT NULL,
                created_at   TEXT NOT NULL,
                revoked_at   TEXT
            );
            CREATE INDEX IF NOT EXISTS mounts_vault ON mounts(vault_id);
            CREATE INDEX IF NOT EXISTS mounts_share ON mounts(share_id);
            "#,
        )?;
        migrate(&conn)?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    fn now() -> String {
        chrono::Utc::now().to_rfc3339()
    }

    // ------------------------------------------------------------------ users

    pub fn create_user(&self, name: &str) -> anyhow::Result<User> {
        anyhow::ensure!(valid_name(name), "invalid user name (use letters, digits, - _ .)");
        let user = User { id: new_id(), name: name.to_string(), created_at: Self::now() };
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO users (id, name, created_at) VALUES (?1, ?2, ?3)",
            params![user.id, user.name, user.created_at],
        )
        .with_context(|| format!("creating user {name} (already exists?)"))?;
        Ok(user)
    }

    pub fn user_by_name(&self, name: &str) -> anyhow::Result<Option<User>> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row(
                "SELECT id, name, created_at FROM users WHERE name = ?1",
                params![name],
                |r| Ok(User { id: r.get(0)?, name: r.get(1)?, created_at: r.get(2)? }),
            )
            .optional()?)
    }

    pub fn user_by_id(&self, id: &str) -> anyhow::Result<Option<User>> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row(
                "SELECT id, name, created_at FROM users WHERE id = ?1",
                params![id],
                |r| Ok(User { id: r.get(0)?, name: r.get(1)?, created_at: r.get(2)? }),
            )
            .optional()?)
    }

    pub fn list_users(&self) -> anyhow::Result<Vec<User>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT id, name, created_at FROM users ORDER BY name")?;
        let rows = stmt.query_map([], |r| Ok(User { id: r.get(0)?, name: r.get(1)?, created_at: r.get(2)? }))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn user_count(&self) -> anyhow::Result<i64> {
        let conn = self.conn.lock();
        Ok(conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?)
    }

    pub fn delete_user(&self, id: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        Ok(conn.execute("DELETE FROM users WHERE id = ?1", params![id])? > 0)
    }

    // ----------------------------------------------------------------- tokens

    /// Issue a new token for a user. The plaintext is returned once and never stored.
    pub fn create_token(&self, user_id: &str, label: &str) -> anyhow::Result<String> {
        let token = new_token();
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO tokens (hash, user_id, label, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![token_hash(&token), user_id, label, Self::now()],
        )?;
        Ok(token)
    }

    /// Resolve a presented token to its user, updating `last_used_at`.
    pub fn authenticate(&self, token: &str) -> anyhow::Result<Option<User>> {
        let hash = token_hash(token);
        let conn = self.conn.lock();
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
        let conn = self.conn.lock();
        let mut stmt =
            conn.prepare("SELECT label, created_at, last_used_at FROM tokens WHERE user_id = ?1 ORDER BY created_at")?;
        let rows = stmt.query_map(params![user_id], |r| {
            Ok(TokenInfo { label: r.get(0)?, created_at: r.get(1)?, last_used_at: r.get(2)? })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn revoke_tokens(&self, user_id: &str, label: Option<&str>) -> anyhow::Result<usize> {
        let conn = self.conn.lock();
        Ok(match label {
            Some(l) => conn.execute("DELETE FROM tokens WHERE user_id = ?1 AND label = ?2", params![user_id, l])?,
            None => conn.execute("DELETE FROM tokens WHERE user_id = ?1", params![user_id])?,
        })
    }

    // ----------------------------------------------------------------- vaults

    pub fn get_or_create_vault(&self, user_id: &str, name: &str) -> anyhow::Result<VaultInfo> {
        anyhow::ensure!(valid_name(name), "invalid vault name");
        let conn = self.conn.lock();
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

    /// Look a vault up without creating it.
    pub fn vault_by_name(&self, user_id: &str, name: &str) -> anyhow::Result<Option<VaultInfo>> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row("SELECT id, user_id, name, created_at FROM vaults WHERE user_id = ?1 AND name = ?2", params![user_id, name], row_to_vault)
            .optional()?)
    }

    pub fn vault_count(&self, user_id: &str) -> anyhow::Result<usize> {
        let conn = self.conn.lock();
        Ok(conn.query_row("SELECT count(*) FROM vaults WHERE user_id = ?1", params![user_id], |r| r.get::<_, i64>(0))? as usize)
    }

    pub fn vault_by_id(&self, id: &str) -> anyhow::Result<Option<VaultInfo>> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row("SELECT id, user_id, name, created_at FROM vaults WHERE id = ?1", params![id], row_to_vault)
            .optional()?)
    }

    pub fn list_vaults(&self, user_id: Option<&str>) -> anyhow::Result<Vec<VaultInfo>> {
        let conn = self.conn.lock();
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
        let conn = self.conn.lock();
        Ok(conn.execute("DELETE FROM vaults WHERE id = ?1", params![id])? > 0)
    }

    /// Re-insert a vault row (used by restore when the registry lacks a vault found on disk).
    pub fn upsert_vault(&self, v: &VaultInfo) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT OR IGNORE INTO vaults (id, user_id, name, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![v.id, v.user_id, v.name, v.created_at],
        )?;
        Ok(())
    }

    pub fn upsert_user(&self, u: &User) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT OR IGNORE INTO users (id, name, created_at) VALUES (?1, ?2, ?3)",
            params![u.id, u.name, u.created_at],
        )?;
        Ok(())
    }

    /// Consistent copy of the registry for backups (SQLite online backup API).
    pub fn backup_to(&self, dest: &Path) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        let mut dst = Connection::open(dest)?;
        let backup = rusqlite::backup::Backup::new(&conn, &mut dst)?;
        backup.run_to_completion(256, std::time::Duration::from_millis(5), None)?;
        Ok(())
    }
}

impl Registry {
    // ----------------------------------------------------------------- shares

    pub fn create_share(&self, user_id: &str, vault_id: &str, name: &str, prefix: &str, only_file: Option<&str>) -> anyhow::Result<ShareInfo> {
        let s = ShareInfo {
            id: new_id(),
            user_id: user_id.into(),
            vault_id: vault_id.into(),
            name: name.into(),
            prefix: prefix.into(),
            only_file: only_file.map(|s| s.to_string()),
            created_at: Self::now(),
        };
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO shares (id, user_id, vault_id, name, prefix, only_file, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![s.id, s.user_id, s.vault_id, s.name, s.prefix, s.only_file, s.created_at],
        )?;
        Ok(s)
    }

    pub fn share_by_id(&self, id: &str) -> anyhow::Result<Option<ShareInfo>> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row("SELECT id, user_id, vault_id, name, prefix, only_file, created_at FROM shares WHERE id = ?1", params![id], row_to_share)
            .optional()?)
    }

    pub fn shares_for_vault(&self, vault_id: &str) -> anyhow::Result<Vec<ShareInfo>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT id, user_id, vault_id, name, prefix, only_file, created_at FROM shares WHERE vault_id = ?1 ORDER BY prefix")?;
        let rows = stmt.query_map(params![vault_id], row_to_share)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Move a share to another folder or note in the same vault (the share itself is unchanged).
    pub fn update_share_path(&self, id: &str, prefix: &str, only_file: Option<&str>) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        Ok(conn.execute("UPDATE shares SET prefix = ?1, only_file = ?2 WHERE id = ?3", params![prefix, only_file, id])? > 0)
    }

    pub fn delete_share(&self, id: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        Ok(conn.execute("DELETE FROM shares WHERE id = ?1", params![id])? > 0)
    }

    pub fn create_grant(&self, share_id: &str, label: &str, access: &str) -> anyhow::Result<String> {
        let secret = hex::encode({
            let mut b = [0u8; 32];
            rand::Rng::fill_bytes(&mut rand::rng(), &mut b);
            b
        });
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO share_grants (share_id, secret_hash, label, access, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![share_id, token_hash(&secret), label, access, Self::now()],
        )
        .with_context(|| format!("creating ticket '{label}' (label already used?)"))?;
        Ok(secret)
    }

    pub fn grants_for_share(&self, share_id: &str) -> anyhow::Result<Vec<GrantInfo>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT label, access, created_at, revoked_at, last_seen_at FROM share_grants WHERE share_id = ?1 ORDER BY created_at")?;
        let rows = stmt.query_map(params![share_id], |r| {
            Ok(GrantInfo { label: r.get(0)?, access: r.get(1)?, created_at: r.get(2)?, revoked_at: r.get(3)?, last_seen_at: r.get(4)? })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Validate a presented share secret. Returns the grant's access level if valid and not revoked.
    pub fn check_grant(&self, share_id: &str, secret: &str) -> anyhow::Result<Option<(String, String)>> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row(
                "SELECT label, access FROM share_grants WHERE share_id = ?1 AND secret_hash = ?2 AND revoked_at IS NULL",
                params![share_id, token_hash(secret)],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?)
    }

    /// Record that a guest just authenticated with this secret.
    pub fn touch_grant(&self, share_id: &str, secret: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE share_grants SET last_seen_at = ?1 WHERE share_id = ?2 AND secret_hash = ?3",
            params![Self::now(), share_id, token_hash(secret)],
        )?;
        Ok(())
    }

    pub fn grant_secret_hash(&self, share_id: &str, label: &str) -> anyhow::Result<Option<String>> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row("SELECT secret_hash FROM share_grants WHERE share_id = ?1 AND label = ?2", params![share_id, label], |r| r.get(0))
            .optional()?)
    }

    /// Make a revoked ticket valid again. Returns its secret hash so mounts can be restored too.
    pub fn restore_grant(&self, share_id: &str, label: &str) -> anyhow::Result<Option<String>> {
        let conn = self.conn.lock();
        let hash: Option<String> = conn
            .query_row(
                "SELECT secret_hash FROM share_grants WHERE share_id = ?1 AND label = ?2 AND revoked_at IS NOT NULL",
                params![share_id, label],
                |r| r.get(0),
            )
            .optional()?;
        if hash.is_some() {
            conn.execute("UPDATE share_grants SET revoked_at = NULL WHERE share_id = ?1 AND label = ?2", params![share_id, label])?;
        }
        Ok(hash)
    }

    /// Un-revoke the local mounts made with a secret. Returns their ids so bridges can be re-attached.
    pub fn restore_mounts_with_secret(&self, share_id: &str, secret_hash: &str) -> anyhow::Result<Vec<String>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT id, secret FROM mounts WHERE share_id = ?1 AND revoked_at IS NOT NULL")?;
        let ids: Vec<String> = stmt
            .query_map(params![share_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .filter_map(|r| r.ok())
            .filter(|(_, s)| token_hash(s) == secret_hash)
            .map(|(id, _)| id)
            .collect();
        for id in &ids {
            conn.execute("UPDATE mounts SET revoked_at = NULL WHERE id = ?1", params![id])?;
        }
        Ok(ids)
    }

    /// Remove a revoked ticket for good (it was dead already; this only tidies the list).
    pub fn delete_revoked_grant(&self, share_id: &str, label: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        let n = conn.execute(
            "DELETE FROM share_grants WHERE share_id = ?1 AND label = ?2 AND revoked_at IS NOT NULL",
            params![share_id, label],
        )?;
        Ok(n > 0)
    }

    pub fn revoke_grant(&self, share_id: &str, label: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        let n = conn.execute(
            "UPDATE share_grants SET revoked_at = ?1 WHERE share_id = ?2 AND label = ?3 AND revoked_at IS NULL",
            params![Self::now(), share_id, label],
        )?;
        Ok(n > 0)
    }

    // ----------------------------------------------------------------- mounts

    #[allow(clippy::too_many_arguments)]
    pub fn create_mount(
        &self,
        user_id: &str,
        vault_id: &str,
        share_id: &str,
        share_name: &str,
        mount_prefix: &str,
        access: &str,
        home_node: &str,
        home_addrs: &[String],
        only_file: Option<&str>,
        secret: &str,
    ) -> anyhow::Result<MountInfo> {
        let m = MountInfo {
            id: new_id(),
            user_id: user_id.into(),
            vault_id: vault_id.into(),
            share_id: share_id.into(),
            share_name: share_name.into(),
            mount_prefix: mount_prefix.into(),
            access: access.into(),
            home_node: home_node.into(),
            home_addrs: home_addrs.to_vec(),
            only_file: only_file.map(str::to_string),
            secret: secret.into(),
            created_at: Self::now(),
            revoked_at: None,
        };
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO mounts (id, user_id, vault_id, share_id, share_name, mount_prefix, access, home_node, home_addrs, only_file, secret, created_at)              VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![m.id, m.user_id, m.vault_id, m.share_id, m.share_name, m.mount_prefix, m.access, m.home_node, m.home_addrs.join(" "), m.only_file, m.secret, m.created_at],
        )?;
        Ok(m)
    }

    /// Mark one mount revoked (used when the owner server denies our secret).
    pub fn revoke_mount(&self, id: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        Ok(conn.execute("UPDATE mounts SET revoked_at = ?1 WHERE id = ?2 AND revoked_at IS NULL", params![Self::now(), id])? > 0)
    }

    pub fn all_mounts(&self) -> anyhow::Result<Vec<MountInfo>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&format!("{MOUNT_COLS} ORDER BY created_at"))?;
        let rows = stmt.query_map([], row_to_mount)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn all_shares(&self) -> anyhow::Result<Vec<ShareInfo>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&format!("{SHARE_COLS} ORDER BY created_at"))?;
        let rows = stmt.query_map([], row_to_share)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn mount_by_id(&self, id: &str) -> anyhow::Result<Option<MountInfo>> {
        let conn = self.conn.lock();
        Ok(conn.query_row(&format!("{MOUNT_COLS} WHERE id = ?1"), params![id], row_to_mount).optional()?)
    }

    pub fn mounts_for_vault(&self, vault_id: &str) -> anyhow::Result<Vec<MountInfo>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&format!("{MOUNT_COLS} WHERE vault_id = ?1 ORDER BY mount_prefix"))?;
        let rows = stmt.query_map(params![vault_id], row_to_mount)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn mounts_for_share(&self, share_id: &str) -> anyhow::Result<Vec<MountInfo>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&format!("{MOUNT_COLS} WHERE share_id = ?1"))?;
        let rows = stmt.query_map(params![share_id], row_to_mount)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn update_mount_prefix(&self, id: &str, prefix: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        Ok(conn.execute("UPDATE mounts SET mount_prefix = ?1 WHERE id = ?2", params![prefix, id])? > 0)
    }

    pub fn delete_mount(&self, id: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        Ok(conn.execute("DELETE FROM mounts WHERE id = ?1", params![id])? > 0)
    }

    pub fn revoke_mounts_with_secret(&self, share_id: &str, secret_hash: &str) -> anyhow::Result<Vec<String>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT id, secret FROM mounts WHERE share_id = ?1 AND revoked_at IS NULL")?;
        let ids: Vec<String> = stmt
            .query_map(params![share_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .filter_map(|r| r.ok())
            .filter(|(_, s)| token_hash(s) == secret_hash)
            .map(|(id, _)| id)
            .collect();
        for id in &ids {
            conn.execute("UPDATE mounts SET revoked_at = ?1 WHERE id = ?2", params![Self::now(), id])?;
        }
        Ok(ids)
    }
}

// ---------------------------------------------------------------------- schema migrations
//
// The CREATE TABLE IF NOT EXISTS block above describes the *current* shape and only ever
// creates missing tables. Anything that changes an existing table lives here as a numbered
// step, tracked in SQLite's `PRAGMA user_version`. Rules: steps are additive and idempotent,
// never drop a table that could hold user data (rename it `<name>_legacy_vN` instead), and
// bump REGISTRY_SCHEMA_VERSION together with the new step. See docs/STORAGE.md.

/// Current registry schema version (`PRAGMA user_version` after all steps ran).
pub const REGISTRY_SCHEMA_VERSION: u32 = 3;

fn migrate(conn: &Connection) -> anyhow::Result<()> {
    let mut version: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let start = version;

    if version < 1 {
        // v1: the sharing model. Databases from the first scaffold carry an empty, reserved
        // `mounts` table with a different shape (vault_id, mount_path, share_id, remote_server).
        if !has_column(conn, "mounts", "user_id")? {
            let rows: i64 = conn.query_row("SELECT count(*) FROM mounts", [], |r| r.get(0))?;
            if rows > 0 {
                tracing::warn!("registry: keeping {rows} rows of the pre-sharing mounts table as mounts_legacy_v0");
                conn.execute_batch("ALTER TABLE mounts RENAME TO mounts_legacy_v0")?;
            } else {
                conn.execute_batch("DROP TABLE mounts")?;
            }
            conn.execute_batch(
                r#"
                CREATE TABLE mounts (
                    id           TEXT PRIMARY KEY,
                    user_id      TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                    vault_id     TEXT NOT NULL REFERENCES vaults(id) ON DELETE CASCADE,
                    share_id     TEXT NOT NULL,
                    share_name   TEXT NOT NULL,
                    mount_prefix TEXT NOT NULL,
                    access       TEXT NOT NULL,
                    home_node    TEXT NOT NULL,
                    secret       TEXT NOT NULL,
                    created_at   TEXT NOT NULL,
                    revoked_at   TEXT
                );
                CREATE INDEX IF NOT EXISTS mounts_vault ON mounts(vault_id);
                CREATE INDEX IF NOT EXISTS mounts_share ON mounts(share_id);
                "#,
            )?;
        }
        version = 1;
    }

    if version < 2 {
        // v2: remote mounts (server-to-server) need the owner's addresses and the single-note flag.
        if !has_column(conn, "mounts", "home_addrs")? {
            conn.execute_batch("ALTER TABLE mounts ADD COLUMN home_addrs TEXT NOT NULL DEFAULT ''")?;
        }
        if !has_column(conn, "mounts", "only_file")? {
            conn.execute_batch("ALTER TABLE mounts ADD COLUMN only_file TEXT")?;
        }
        version = 2;
    }

    if version < 3 {
        // v3: guest mode shows the owner when a ticket was last used.
        if !has_column(conn, "share_grants", "last_seen_at")? {
            conn.execute_batch("ALTER TABLE share_grants ADD COLUMN last_seen_at TEXT")?;
        }
        version = 3;
    }

    if version > REGISTRY_SCHEMA_VERSION {
        anyhow::bail!(
            "registry.db schema is v{version} but this server only knows v{REGISTRY_SCHEMA_VERSION}; upgrade the server"
        );
    }
    if version != start {
        conn.pragma_update(None, "user_version", version)?;
        tracing::info!("registry: migrated schema v{start} -> v{version}");
    } else if start == 0 {
        conn.pragma_update(None, "user_version", REGISTRY_SCHEMA_VERSION)?;
    }
    Ok(())
}

fn has_column(conn: &Connection, table: &str, column: &str) -> anyhow::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let cols: Vec<String> = stmt.query_map([], |r| r.get::<_, String>(1))?.collect::<Result<_, _>>()?;
    Ok(cols.iter().any(|c| c == column))
}

const SHARE_COLS: &str = "SELECT id, user_id, vault_id, name, prefix, only_file, created_at FROM shares";
const MOUNT_COLS: &str = "SELECT id, user_id, vault_id, share_id, share_name, mount_prefix, access, home_node, secret, created_at, revoked_at, home_addrs, only_file FROM mounts";

fn row_to_share(r: &rusqlite::Row) -> rusqlite::Result<ShareInfo> {
    Ok(ShareInfo {
        id: r.get(0)?,
        user_id: r.get(1)?,
        vault_id: r.get(2)?,
        name: r.get(3)?,
        prefix: r.get(4)?,
        only_file: r.get(5)?,
        created_at: r.get(6)?,
    })
}

fn row_to_mount(r: &rusqlite::Row) -> rusqlite::Result<MountInfo> {
    Ok(MountInfo {
        id: r.get(0)?,
        user_id: r.get(1)?,
        vault_id: r.get(2)?,
        share_id: r.get(3)?,
        share_name: r.get(4)?,
        mount_prefix: r.get(5)?,
        access: r.get(6)?,
        home_node: r.get(7)?,
        secret: r.get(8)?,
        created_at: r.get(9)?,
        revoked_at: r.get(10)?,
        home_addrs: r.get::<_, String>(11)?.split_whitespace().map(str::to_string).collect(),
        only_file: r.get(12)?,
    })
}

fn row_to_vault(r: &rusqlite::Row) -> rusqlite::Result<VaultInfo> {
    Ok(VaultInfo { id: r.get(0)?, user_id: r.get(1)?, name: r.get(2)?, created_at: r.get(3)? })
}

#[cfg(test)]
mod migration_tests {
    use super::*;

    fn open_with(sql: &str) -> (tempfile::TempDir, Registry) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.db");
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch(sql).unwrap();
        }
        let reg = Registry::open(&path).unwrap();
        (dir, reg)
    }

    fn user_version(reg: &Registry) -> u32 {
        reg.conn.lock().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn fresh_db_is_current() {
        let (_d, reg) = open_with("");
        assert_eq!(user_version(&reg), REGISTRY_SCHEMA_VERSION);
        assert!(has_column(&reg.conn.lock(), "mounts", "home_addrs").unwrap());
    }

    #[test]
    fn scaffold_v0_reserved_mounts_table_is_replaced() {
        let (_d, reg) = open_with(
            "CREATE TABLE users (id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, created_at TEXT NOT NULL);
             CREATE TABLE vaults (id TEXT PRIMARY KEY, user_id TEXT NOT NULL, name TEXT NOT NULL, created_at TEXT NOT NULL);
             CREATE TABLE mounts (id TEXT PRIMARY KEY, vault_id TEXT NOT NULL, mount_path TEXT NOT NULL, share_id TEXT NOT NULL, remote_server TEXT, created_at TEXT NOT NULL);",
        );
        assert_eq!(user_version(&reg), REGISTRY_SCHEMA_VERSION);
        let c = reg.conn.lock();
        assert!(has_column(&c, "mounts", "user_id").unwrap());
        assert!(has_column(&c, "mounts", "only_file").unwrap());
        drop(c);
        // The query that failed in the field now works.
        assert!(reg.mounts_for_vault("nope").unwrap().is_empty());
    }

    #[test]
    fn phase1_db_gets_v2_columns() {
        let (_d, reg) = open_with(
            "CREATE TABLE users (id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, created_at TEXT NOT NULL);
             CREATE TABLE vaults (id TEXT PRIMARY KEY, user_id TEXT NOT NULL, name TEXT NOT NULL, created_at TEXT NOT NULL);
             CREATE TABLE mounts (id TEXT PRIMARY KEY, user_id TEXT NOT NULL, vault_id TEXT NOT NULL, share_id TEXT NOT NULL, share_name TEXT NOT NULL, mount_prefix TEXT NOT NULL, access TEXT NOT NULL, home_node TEXT NOT NULL, secret TEXT NOT NULL, created_at TEXT NOT NULL, revoked_at TEXT);
             INSERT INTO mounts VALUES ('m1','u','v','s','S','Shared/S/','rw','node','sec','t',NULL);",
        );
        assert_eq!(user_version(&reg), REGISTRY_SCHEMA_VERSION);
        let m = reg.mount_by_id("m1").unwrap().unwrap();
        assert!(m.home_addrs.is_empty());
        assert_eq!(m.only_file, None);
    }

    #[test]
    fn newer_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.db");
        {
            let c = Connection::open(&path).unwrap();
            c.pragma_update(None, "user_version", 99u32).unwrap();
        }
        assert!(Registry::open(&path).is_err());
    }
}
