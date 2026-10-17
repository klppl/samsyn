//! Whole-instance (or per-user / per-vault) backup to a tar.gz stream, and restore.
//!
//! The archive mirrors the data directory layout exactly, so a backup can also be
//! inspected or restored by hand with `tar`. SQLite files are copied through the online
//! backup API so they are consistent even while the server is running.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;

use super::{Storage, VaultStore};

pub struct BackupFilter {
    pub user: Option<String>,
    pub vault: Option<String>,
}

pub fn write_backup<W: Write>(storage: &Storage, filter: &BackupFilter, out: W) -> anyhow::Result<BackupStats> {
    let tmp = tempfile::tempdir()?;
    let enc = GzEncoder::new(out, Compression::default());
    let mut tar = tar::Builder::new(enc);
    tar.follow_symlinks(false);
    let mut stats = BackupStats::default();

    tar.append_path_with_name(storage.root().join("server.json"), "server.json")?;

    let reg_copy = tmp.path().join("registry.db");
    storage.registry().backup_to(&reg_copy)?;
    tar.append_path_with_name(&reg_copy, "registry.db")?;

    let user_filter = match &filter.user {
        Some(name) => Some(
            storage
                .registry()
                .user_by_name(name)?
                .with_context(|| format!("no such user {name}"))?,
        ),
        None => None,
    };

    for v in storage.registry().list_vaults(user_filter.as_ref().map(|u| u.id.as_str()))? {
        if let Some(vn) = &filter.vault {
            if &v.name != vn {
                continue;
            }
        }
        let store = storage.vault_store(&v)?;
        let rel = format!("users/{}/vaults/{}", v.user_id, v.id);
        let dir = store.dir();

        tar.append_path_with_name(dir.join("vault.json"), format!("{rel}/vault.json"))?;

        let db_copy = tmp.path().join(format!("{}.db", v.id));
        store.backup_db_to(&db_copy)?;
        tar.append_path_with_name(&db_copy, format!("{rel}/doc.db"))?;

        for h in store.list_blob_hashes()? {
            let p = dir.join("blobs").join(&h[..2]).join(&h);
            tar.append_path_with_name(&p, format!("{rel}/blobs/{}/{}", &h[..2], h))?;
            stats.blobs += 1;
        }
        for s in store.list_snapshots()? {
            let name = s.file_name().unwrap().to_string_lossy().to_string();
            tar.append_path_with_name(&s, format!("{rel}/snapshots/{name}"))?;
            let man = s.with_extension("json");
            if man.exists() {
                let mname = man.file_name().unwrap().to_string_lossy().to_string();
                tar.append_path_with_name(&man, format!("{rel}/snapshots/{mname}"))?;
            }
            stats.snapshots += 1;
        }
        stats.vaults += 1;
    }

    // Share documents and remote-mount replicas, laid out next to the vaults. Only in full
    // backups: they are keyed by user id and the merge path does not know how to merge them.
    if filter.user.is_none() && filter.vault.is_none() {
        for sh in storage.registry().all_shares()? {
            let store = storage.share_store(&sh)?;
            append_store(&mut tar, tmp.path(), &store, &format!("users/{}/shares/{}", sh.user_id, sh.id), &sh.id, &mut stats)?;
        }
        for m in storage.registry().all_mounts()? {
            if !storage.mount_dir(&m.user_id, &m.id).join("doc.db").exists() {
                continue;
            }
            let store = storage.mount_store(&m)?;
            append_store(&mut tar, tmp.path(), &store, &format!("users/{}/mounts/{}", m.user_id, m.id), &m.id, &mut stats)?;
        }
    }

    let enc = tar.into_inner()?;
    enc.finish()?;
    Ok(stats)
}

fn append_store<W: Write>(
    tar: &mut tar::Builder<W>,
    tmp: &Path,
    store: &VaultStore,
    rel: &str,
    id: &str,
    stats: &mut BackupStats,
) -> anyhow::Result<()> {
    let dir = store.dir();
    tar.append_path_with_name(dir.join("vault.json"), format!("{rel}/vault.json"))?;
    let db_copy = tmp.join(format!("{id}.db"));
    store.backup_db_to(&db_copy)?;
    tar.append_path_with_name(&db_copy, format!("{rel}/doc.db"))?;
    for h in store.list_blob_hashes()? {
        let p = dir.join("blobs").join(&h[..2]).join(&h);
        tar.append_path_with_name(&p, format!("{rel}/blobs/{}/{}", &h[..2], h))?;
        stats.blobs += 1;
    }
    for s in store.list_snapshots()? {
        let name = s.file_name().unwrap().to_string_lossy().to_string();
        tar.append_path_with_name(&s, format!("{rel}/snapshots/{name}"))?;
        let man = s.with_extension("json");
        if man.exists() {
            let mname = man.file_name().unwrap().to_string_lossy().to_string();
            tar.append_path_with_name(&man, format!("{rel}/snapshots/{mname}"))?;
        }
        stats.snapshots += 1;
    }
    Ok(())
}

#[derive(Default, Debug)]
pub struct BackupStats {
    pub vaults: usize,
    pub blobs: usize,
    pub snapshots: usize,
}

/// Extract an archive into `data_dir`. The directory must be empty unless `merge` is set,
/// in which case vault directories are added/overwritten and registry rows are merged.
pub fn restore_backup<R: Read>(input: R, data_dir: &Path, merge: bool) -> anyhow::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let is_empty = std::fs::read_dir(data_dir)?.next().is_none();
    anyhow::ensure!(is_empty || merge, "{} is not empty; pass --merge to add to it", data_dir.display());

    let staging = tempfile::tempdir_in(data_dir.parent().unwrap_or(Path::new(".")))?;
    let mut archive = tar::Archive::new(GzDecoder::new(input));
    archive.unpack(staging.path())?;

    if !merge {
        // Fresh restore: move everything into place.
        for entry in std::fs::read_dir(staging.path())? {
            let entry = entry?;
            let dest = data_dir.join(entry.file_name());
            move_path(&entry.path(), &dest)?;
        }
        return Ok(());
    }

    // Merge: copy vault dirs, then register anything the live registry does not know.
    let live = Storage::open(data_dir)?;
    let staged_reg = super::Registry::open(&staging.path().join("registry.db"))?;
    let mut seen_users: HashSet<String> = HashSet::new();
    for v in staged_reg.list_vaults(None)? {
        // Ids from a foreign registry become directory names: accept only our own id shape.
        anyhow::ensure!(is_hex_id(&v.id) && is_hex_id(&v.user_id), "backup contains a malformed vault or user id ({}/{})", v.user_id, v.id);
        if !seen_users.contains(&v.user_id) {
            if let Some(u) = staged_reg.user_by_id(&v.user_id)? {
                live.registry().upsert_user(&u)?;
            }
            seen_users.insert(v.user_id.clone());
        }
        live.registry().upsert_vault(&v)?;
        let src = staging.path().join("users").join(&v.user_id).join("vaults").join(&v.id);
        let dst = live.vault_dir(&v.user_id, &v.id);
        if dst.exists() {
            std::fs::remove_dir_all(&dst)?;
        }
        std::fs::create_dir_all(dst.parent().unwrap())?;
        move_path(&src, &dst)?;
        // Validate it opens.
        let _ = VaultStore::open(&dst, &v)?;
    }
    Ok(())
}

fn is_hex_id(s: &str) -> bool {
    s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn move_path(src: &Path, dst: &Path) -> anyhow::Result<()> {
    if std::fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    // Cross-device: copy recursively.
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            move_path(&e.path(), &dst.join(e.file_name()))?;
        }
        std::fs::remove_dir_all(src)?;
    } else {
        std::fs::copy(src, dst)?;
        std::fs::remove_file(src)?;
    }
    Ok(())
}

pub fn default_backup_name() -> PathBuf {
    PathBuf::from(format!("replica-backup-{}.tar.gz", chrono::Utc::now().format("%Y%m%d-%H%M%S")))
}
