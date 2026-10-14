//! Administrative subcommands.

use std::io::Write;

use anyhow::Context;

use crate::config::{UserCmd, VaultCmd};
use crate::storage::backup::{default_backup_name, restore_backup, write_backup, BackupFilter};
use crate::storage::{Storage, VaultInfo};

pub fn user(storage: &Storage, cmd: UserCmd) -> anyhow::Result<()> {
    let reg = storage.registry();
    match cmd {
        UserCmd::Add { name } => {
            let u = reg.create_user(&name)?;
            let token = reg.create_token(&u.id, "initial")?;
            println!("created user {}", u.name);
            println!("token: {token}");
            println!("(store it now; tokens are only shown once)");
        }
        UserCmd::List => {
            for u in reg.list_users()? {
                let vaults = reg.list_vaults(Some(&u.id))?.len();
                let tokens = reg.list_tokens(&u.id)?.len();
                println!("{:<24} id={} vaults={} tokens={} created={}", u.name, u.id, vaults, tokens, u.created_at);
            }
        }
        UserCmd::Token { name, label } => {
            let u = reg.user_by_name(&name)?.with_context(|| format!("no such user {name}"))?;
            let token = reg.create_token(&u.id, &label)?;
            println!("token for {} ({label}): {token}", u.name);
        }
        UserCmd::Revoke { name, label } => {
            let u = reg.user_by_name(&name)?.with_context(|| format!("no such user {name}"))?;
            let n = reg.revoke_tokens(&u.id, label.as_deref())?;
            println!("revoked {n} token(s)");
        }
        UserCmd::Remove { name } => {
            let u = reg.user_by_name(&name)?.with_context(|| format!("no such user {name}"))?;
            reg.delete_user(&u.id)?;
            println!("removed user {} from registry; data left at {}", u.name, storage.root().join("users").join(&u.id).display());
        }
    }
    Ok(())
}

pub fn vault(storage: &Storage, cmd: VaultCmd) -> anyhow::Result<()> {
    let reg = storage.registry();
    match cmd {
        VaultCmd::List { user } => {
            let uid = match user {
                Some(n) => Some(reg.user_by_name(&n)?.with_context(|| format!("no such user {n}"))?.id),
                None => None,
            };
            for v in reg.list_vaults(uid.as_deref())? {
                let owner = reg.user_by_id(&v.user_id)?.map(|u| u.name).unwrap_or_else(|| "?".into());
                let store = storage.vault_store(&v)?;
                let updates = store.update_count().unwrap_or(0);
                let blobs = store.list_blob_hashes().map(|b| b.len()).unwrap_or(0);
                let snaps = store.list_snapshots().map(|s| s.len()).unwrap_or(0);
                println!(
                    "{owner}/{:<20} id={} updates={updates} blobs={blobs} snapshots={snaps} dir={}",
                    v.name,
                    v.id,
                    store.dir().display()
                );
            }
        }
    }
    Ok(())
}

pub fn resolve_target(storage: &Storage, target: &str) -> anyhow::Result<VaultInfo> {
    let (user, vault) = target.split_once('/').context("target must be user/vault")?;
    let u = storage.registry().user_by_name(user)?.with_context(|| format!("no such user {user}"))?;
    storage
        .registry()
        .list_vaults(Some(&u.id))?
        .into_iter()
        .find(|v| v.name == vault)
        .with_context(|| format!("no vault {vault} for user {user}"))
}

pub fn snapshot(storage: &Storage, target: Option<String>, keep: usize) -> anyhow::Result<()> {
    let vaults = match target {
        Some(t) => vec![resolve_target(storage, &t)?],
        None => storage.registry().list_vaults(None)?,
    };
    for v in vaults {
        let store = storage.vault_store(&v)?;
        let (doc, n) = store.load_doc()?;
        let path = store.write_snapshot(&doc, &v.id)?;
        let full = {
            use yrs::{ReadTxn, Transact};
            doc.transact().encode_state_as_update_v1(&yrs::StateVector::default())
        };
        store.compact(&full)?;
        let pruned = store.prune_snapshots(keep)?;
        println!("{}: snapshot {} ({} updates compacted, {} old snapshots pruned)", v.name, path.display(), n, pruned);
    }
    Ok(())
}

pub fn backup(storage: &Storage, out: Option<String>, user: Option<String>, vault: Option<String>) -> anyhow::Result<()> {
    let filter = BackupFilter { user, vault };
    let stats = match out.as_deref() {
        Some("-") => write_backup(storage, &filter, std::io::stdout().lock())?,
        other => {
            let path = other.map(std::path::PathBuf::from).unwrap_or_else(default_backup_name);
            let f = std::fs::File::create(&path).with_context(|| format!("creating {}", path.display()))?;
            let stats = write_backup(storage, &filter, std::io::BufWriter::new(f))?;
            eprintln!("wrote {}", path.display());
            stats
        }
    };
    eprintln!("{} vault(s), {} blob(s), {} snapshot(s)", stats.vaults, stats.blobs, stats.snapshots);
    Ok(())
}

pub fn restore(data_dir: &std::path::Path, from: &str, merge: bool) -> anyhow::Result<()> {
    if from == "-" {
        restore_backup(std::io::stdin().lock(), data_dir, merge)?;
    } else {
        let f = std::fs::File::open(from).with_context(|| format!("opening {from}"))?;
        restore_backup(std::io::BufReader::new(f), data_dir, merge)?;
    }
    println!("restored into {}", data_dir.display());
    Ok(())
}

pub fn restore_snapshot(storage: &Storage, target: &str, file: &std::path::Path, yes: bool) -> anyhow::Result<()> {
    anyhow::ensure!(yes, "this replaces the vault's current history; re-run with --yes to confirm");
    let v = resolve_target(storage, target)?;
    let store = storage.vault_store(&v)?;
    // Keep a safety snapshot of the current state first.
    let (doc, _) = store.load_doc()?;
    let safety = store.write_snapshot(&doc, &v.id)?;
    store.restore_snapshot(file)?;
    println!("rolled {target} back to {} (previous state saved as {})", file.display(), safety.display());
    std::io::stdout().flush()?;
    Ok(())
}
