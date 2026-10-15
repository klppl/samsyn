//! Live documents (vaults and shares) with persistence, fan-out broadcasting, and the
//! bridges that keep shared subtrees equal between documents.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use tokio::sync::broadcast;
use yrs::sync::{Awareness, Message, SyncMessage};
use yrs::types::{EntryChange, Event, PathSegment};
use yrs::updates::encoder::{Encode, Encoder, EncoderV1};
use yrs::{Any, DeepObservable, GetString, Map, Observable, Out, ReadTxn, StateVector, Transact};

use crate::bridge::{Bridge, Change, ChangeKind, OwnedDelta, Side};
use crate::storage::{MountInfo, ShareInfo, Storage, User, VaultInfo, VaultStore};

const BROADCAST_CAPACITY: usize = 4096;
pub const BRIDGE_ORIGIN_PREFIX: &[u8] = b"bridge:";

pub enum DocKind {
    Vault(VaultInfo),
    Share(ShareInfo),
}

pub struct LiveDoc {
    pub id: String,
    pub label: String,
    pub kind: DocKind,
    pub store: VaultStore,
    pub awareness: Mutex<Awareness>,
    pub tx: broadcast::Sender<Bytes>,
    /// Changes recorded by observers, drained by `DocRegistry::propagate`.
    pub changes: Arc<Mutex<Vec<Change>>>,
}

impl LiveDoc {
    fn load(id: String, label: String, kind: DocKind, store: VaultStore) -> anyhow::Result<Arc<Self>> {
        let (doc, n) = store.load_doc()?;
        if store.needs_compaction() {
            let full = doc.transact().encode_state_as_update_v1(&StateVector::default());
            store.compact(&full)?;
            tracing::info!(doc = %label, "compacted {n} updates into one");
        }

        let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);
        {
            let store = store.clone();
            let tx = tx.clone();
            let label = label.clone();
            doc.observe_update_v1("replica", move |_txn, event| {
                if let Err(e) = store.append_update(&event.update) {
                    tracing::error!(doc = %label, "failed to persist update: {e}");
                }
                let _ = tx.send(encode_message(&Message::Sync(SyncMessage::Update(event.update.clone()))));
            })?;
        }

        let changes: Arc<Mutex<Vec<Change>>> = Arc::new(Mutex::new(Vec::new()));
        let texts = doc.get_or_insert_map("texts");
        let blobs = doc.get_or_insert_map("blobs");
        {
            let changes = changes.clone();
            texts.observe_deep("replica-bridge", move |txn, events| {
                let from = origin_source(txn.origin());
                let mut out = changes.lock().unwrap();
                let mut handled: Vec<String> = Vec::new();
                for ev in events.iter() {
                    match ev {
                        Event::Map(m) if ev.path().is_empty() => {
                            for (k, ch) in m.keys(txn) {
                                let key = k.to_string();
                                let kind = match ch {
                                    EntryChange::Inserted(Out::YText(t)) | EntryChange::Updated(_, Out::YText(t)) => {
                                        ChangeKind::TextSet(t.get_string(txn))
                                    }
                                    EntryChange::Removed(_) => ChangeKind::Delete,
                                    _ => continue,
                                };
                                handled.push(key.clone());
                                out.push(Change { key, kind, from: from.clone() });
                            }
                        }
                        Event::Text(t) => {
                            let Some(PathSegment::Key(k)) = ev.path().front().cloned() else { continue };
                            let key = k.to_string();
                            if handled.contains(&key) {
                                continue;
                            }
                            let delta: Vec<OwnedDelta> = t
                                .delta(txn)
                                .iter()
                                .filter_map(|d| match d {
                                    yrs::types::Delta::Retain(n, _) => Some(OwnedDelta::Retain(*n)),
                                    yrs::types::Delta::Deleted(n) => Some(OwnedDelta::Delete(*n)),
                                    yrs::types::Delta::Inserted(Out::Any(Any::String(s)), _) => Some(OwnedDelta::Insert(s.to_string())),
                                    _ => None,
                                })
                                .collect();
                            if !delta.is_empty() {
                                out.push(Change { key, kind: ChangeKind::TextDelta(delta), from: from.clone() });
                            }
                        }
                        _ => {}
                    }
                }
            });
        }
        {
            let changes = changes.clone();
            blobs.observe("replica-bridge", move |txn, ev| {
                let from = origin_source(txn.origin());
                let mut out = changes.lock().unwrap();
                for (k, ch) in ev.keys(txn) {
                    let kind = match ch {
                        EntryChange::Inserted(Out::Any(a)) | EntryChange::Updated(_, Out::Any(a)) => ChangeKind::BlobSet(a.clone()),
                        EntryChange::Removed(_) => ChangeKind::Delete,
                        _ => continue,
                    };
                    out.push(Change { key: k.to_string(), kind, from: from.clone() });
                }
            });
        }

        let mut awareness = Awareness::new(doc);
        {
            let tx = tx.clone();
            awareness.on_update("replica", move |aw, event, _origin| {
                if let Ok(update) = aw.update_with_clients(event.all_changes()) {
                    let _ = tx.send(encode_message(&Message::Awareness(update)));
                }
            });
        }

        tracing::info!(doc = %label, "loaded ({n} stored updates)");
        Ok(Arc::new(LiveDoc { id, label, kind, store, awareness: Mutex::new(awareness), tx, changes }))
    }

    pub fn full_state(&self) -> Vec<u8> {
        let aw = self.awareness.lock().unwrap();
        let state = aw.doc().transact().encode_state_as_update_v1(&StateVector::default());
        state
    }

    pub fn snapshot(&self, keep: usize) -> anyhow::Result<()> {
        let aw = self.awareness.lock().unwrap();
        let path = self.store.write_snapshot(aw.doc(), &self.id)?;
        let full = aw.doc().transact().encode_state_as_update_v1(&StateVector::default());
        drop(aw);
        self.store.compact(&full)?;
        let pruned = self.store.prune_snapshots(keep)?;
        tracing::info!(doc = %self.label, "snapshot {} written, {pruned} pruned", path.display());
        Ok(())
    }

    pub fn vault_info(&self) -> Option<&VaultInfo> {
        match &self.kind {
            DocKind::Vault(v) => Some(v),
            _ => None,
        }
    }

    /// All keys in `texts` and `blobs`.
    pub fn keys(&self) -> Vec<String> {
        let aw = self.awareness.lock().unwrap();
        let txn = aw.doc().transact();
        let mut out: Vec<String> = txn.get_map("texts").map(|m| m.keys(&txn).map(|k| k.to_string()).collect()).unwrap_or_default();
        if let Some(b) = txn.get_map("blobs") {
            out.extend(b.keys(&txn).map(|k| k.to_string()));
        }
        out
    }
}

fn origin_source(origin: Option<&yrs::Origin>) -> Option<String> {
    let o = origin?;
    let bytes: &[u8] = o.as_ref();
    bytes.strip_prefix(BRIDGE_ORIGIN_PREFIX).map(|b| String::from_utf8_lossy(b).to_string())
}

pub fn encode_message(msg: &Message) -> Bytes {
    let mut enc = EncoderV1::new();
    msg.encode(&mut enc);
    Bytes::from(enc.to_vec())
}

/// All loaded documents plus the bridges between them. `apply_lock` serializes every CRDT
/// mutation of any document on this server, which is what makes bridging safe.
pub struct DocRegistry {
    storage: Storage,
    pub node_id: String,
    vaults: Mutex<HashMap<String, Arc<LiveDoc>>>,
    shares: Mutex<HashMap<String, Arc<LiveDoc>>>,
    bridges: Mutex<Vec<Arc<Bridge>>>,
    pub apply_lock: Mutex<()>,
    /// Serializes share/mount creation (overlap check + registry insert + bridge attach) so
    /// two concurrent requests cannot both pass the overlap check. Taken before `apply_lock`.
    pub share_admin: Mutex<()>,
}

impl DocRegistry {
    pub fn new(storage: Storage, node_id: String) -> Self {
        Self {
            storage,
            node_id,
            vaults: Mutex::new(HashMap::new()),
            shares: Mutex::new(HashMap::new()),
            bridges: Mutex::new(Vec::new()),
            apply_lock: Mutex::new(()),
            share_admin: Mutex::new(()),
        }
    }

    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    /// Live document for `user`'s vault `name`, loading it and its bridges on first use.
    pub fn get_or_load(&self, user: &User, name: &str) -> anyhow::Result<Arc<LiveDoc>> {
        let (info, store) = self.storage.open_vault_for_user(user, name)?;
        let existing = self.vaults.lock().unwrap().get(&info.id).cloned();
        if let Some(v) = existing {
            return Ok(v);
        }
        let _g = self.apply_lock.lock().unwrap();
        // Re-check under the apply lock: another connection may have loaded it meanwhile.
        let existing = self.vaults.lock().unwrap().get(&info.id).cloned();
        if let Some(v) = existing {
            return Ok(v);
        }
        let doc = LiveDoc::load(info.id.clone(), format!("{}/{}", user.name, info.name), DocKind::Vault(info.clone()), store)?;
        self.vaults.lock().unwrap().insert(info.id.clone(), doc.clone());
        self.attach_vault_bridges(&doc, &info);
        Ok(doc)
    }

    pub fn loaded_vault(&self, vault_id: &str) -> Option<Arc<LiveDoc>> {
        self.vaults.lock().unwrap().get(vault_id).cloned()
    }

    fn load_share(&self, info: &ShareInfo) -> anyhow::Result<Arc<LiveDoc>> {
        let existing = self.shares.lock().unwrap().get(&info.id).cloned();
        if let Some(s) = existing {
            return Ok(s);
        }
        let store = self.storage.share_store(info)?;
        let doc = LiveDoc::load(info.id.clone(), format!("share:{}", info.name), DocKind::Share(info.clone()), store)?;
        self.shares.lock().unwrap().insert(info.id.clone(), doc.clone());
        Ok(doc)
    }

    fn attach_vault_bridges(&self, vault: &Arc<LiveDoc>, info: &VaultInfo) {
        let reg = self.storage.registry();
        match reg.shares_for_vault(&info.id) {
            Ok(shares) => {
                for s in shares {
                    if let Err(e) = self.attach_owner_bridge_locked(vault, &s) {
                        tracing::error!(share = %s.name, "attaching share bridge failed: {e}");
                    }
                }
            }
            Err(e) => tracing::error!("listing shares failed: {e}"),
        }
        match reg.mounts_for_vault(&info.id) {
            Ok(mounts) => {
                for m in mounts.into_iter().filter(|m| m.revoked_at.is_none()) {
                    if let Err(e) = self.attach_mount_bridge_locked(vault, &m) {
                        tracing::error!(mount = %m.share_name, "attaching mount bridge failed: {e}");
                    }
                }
            }
            Err(e) => tracing::error!("listing mounts failed: {e}"),
        }
    }

    fn attach_owner_bridge_locked(&self, vault: &Arc<LiveDoc>, share: &ShareInfo) -> anyhow::Result<()> {
        let sdoc = self.load_share(share)?;
        let bridge = Arc::new(Bridge {
            share_id: share.id.clone(),
            mount_id: None,
            vault: vault.clone(),
            share: sdoc,
            vault_prefix: share.prefix.clone(),
            only_file: share.only_file.clone(),
            access: "rw".into(),
            truth: Side::Vault,
        });
        bridge.reconcile();
        self.bridges.lock().unwrap().push(bridge.clone());
        self.propagate(&bridge.share);
        Ok(())
    }

    fn attach_mount_bridge_locked(&self, vault: &Arc<LiveDoc>, mount: &MountInfo) -> anyhow::Result<()> {
        if mount.home_node != self.node_id {
            tracing::warn!(mount = %mount.share_name, "remote shares are not supported yet; mount is inactive");
            return Ok(());
        }
        let share = self
            .storage
            .registry()
            .share_by_id(&mount.share_id)?
            .ok_or_else(|| anyhow::anyhow!("share {} no longer exists", mount.share_id))?;
        let sdoc = self.load_share(&share)?;
        let bridge = Arc::new(Bridge {
            share_id: share.id.clone(),
            mount_id: Some(mount.id.clone()),
            vault: vault.clone(),
            share: sdoc,
            vault_prefix: mount.mount_prefix.clone(),
            only_file: share.only_file.clone(),
            access: mount.access.clone(),
            truth: Side::Share,
        });
        bridge.reconcile();
        self.bridges.lock().unwrap().push(bridge.clone());
        self.propagate(&bridge.vault);
        Ok(())
    }

    /// Called by the API after creating a share. Takes the apply lock itself.
    pub fn add_owner_bridge(&self, share: &ShareInfo) -> anyhow::Result<()> {
        let _g = self.apply_lock.lock().unwrap();
        let vault = self.loaded_vault(&share.vault_id).ok_or_else(|| anyhow::anyhow!("vault not loaded"))?;
        self.attach_owner_bridge_locked(&vault, share)
    }

    pub fn add_mount_bridge(&self, mount: &MountInfo) -> anyhow::Result<()> {
        let _g = self.apply_lock.lock().unwrap();
        let vault = self.loaded_vault(&mount.vault_id).ok_or_else(|| anyhow::anyhow!("vault not loaded"))?;
        self.attach_mount_bridge_locked(&vault, mount)
    }

    pub fn remove_bridges_for_share(&self, share_id: &str) {
        let _g = self.apply_lock.lock().unwrap();
        self.bridges.lock().unwrap().retain(|b| b.share_id != share_id);
        self.shares.lock().unwrap().remove(share_id);
    }

    pub fn remove_bridges_for_mounts(&self, mount_ids: &[String]) {
        let _g = self.apply_lock.lock().unwrap();
        let mut bridges = self.bridges.lock().unwrap();
        let before = bridges.len();
        bridges.retain(|b| b.mount_id.as_ref().map(|m| !mount_ids.contains(m)).unwrap_or(true));
        tracing::info!("removed {} mount bridge(s) for {:?}", before - bridges.len(), mount_ids);
    }

    /// Load every vault that takes part in a bridge, owners first, so a share can never
    /// advance while one of its ends is not loaded (which would make reconcile lossy).
    pub fn preload_bridged(&self) {
        let reg = self.storage.registry();
        let mut vault_ids: Vec<String> = Vec::new();
        for s in reg.list_vaults(None).unwrap_or_default() {
            if reg.shares_for_vault(&s.id).map(|v| !v.is_empty()).unwrap_or(false) {
                vault_ids.push(s.id);
            }
        }
        for v in reg.list_vaults(None).unwrap_or_default() {
            if reg.mounts_for_vault(&v.id).map(|m| m.iter().any(|m| m.revoked_at.is_none())).unwrap_or(false) && !vault_ids.contains(&v.id) {
                vault_ids.push(v.id);
            }
        }
        for id in vault_ids {
            if let Err(e) = self.load_vault_by_id(&id) {
                tracing::error!(vault = %id, "preloading bridged vault failed: {e}");
            }
        }
    }

    pub fn load_vault_by_id(&self, vault_id: &str) -> anyhow::Result<Arc<LiveDoc>> {
        let reg = self.storage.registry();
        let info = reg.vault_by_id(vault_id)?.ok_or_else(|| anyhow::anyhow!("no vault {vault_id}"))?;
        let user = reg.user_by_id(&info.user_id)?.ok_or_else(|| anyhow::anyhow!("no user {}", info.user_id))?;
        self.get_or_load(&user, &info.name)
    }

    fn bridges_for(&self, doc_id: &str) -> Vec<Arc<Bridge>> {
        self.bridges
            .lock()
            .unwrap()
            .iter()
            .filter(|b| b.vault.id == doc_id || b.share.id == doc_id)
            .cloned()
            .collect()
    }

    /// Drain recorded changes of `start` and replay them across bridges until nothing is left.
    /// Must be called with `apply_lock` held.
    pub fn propagate(&self, start: &Arc<LiveDoc>) {
        let mut work: Vec<Arc<LiveDoc>> = vec![start.clone()];
        let mut rounds = 0;
        while let Some(doc) = work.pop() {
            let changes: Vec<Change> = std::mem::take(&mut *doc.changes.lock().unwrap());
            if changes.is_empty() {
                continue;
            }
            for bridge in self.bridges_for(&doc.id) {
                if let Some(target) = bridge.forward(&doc, &changes) {
                    work.push(target);
                }
            }
            rounds += 1;
            if rounds > 10_000 {
                tracing::error!("bridge propagation did not settle; aborting this round");
                break;
            }
        }
        // Drop anything recorded on docs without bridges so it does not pile up.
        for d in self.vaults.lock().unwrap().values().chain(self.shares.lock().unwrap().values()) {
            if self.bridges_for(&d.id).is_empty() {
                d.changes.lock().unwrap().clear();
            }
        }
    }

    pub fn loaded_count(&self) -> usize {
        self.vaults.lock().unwrap().len()
    }

    pub fn maybe_compact(&self, doc: &LiveDoc) {
        if doc.store.needs_compaction() {
            let full = doc.full_state();
            match doc.store.compact(&full) {
                Ok(()) => tracing::info!(doc = %doc.label, "compacted update log"),
                Err(e) => tracing::error!(doc = %doc.label, "compaction failed: {e}"),
            }
        }
    }

    pub fn snapshot_all(&self, keep: usize) {
        let docs: Vec<Arc<LiveDoc>> = {
            let v = self.vaults.lock().unwrap();
            let s = self.shares.lock().unwrap();
            v.values().chain(s.values()).cloned().collect()
        };
        for d in docs {
            if let Err(e) = d.snapshot(keep) {
                tracing::error!(doc = %d.label, "snapshot failed: {e}");
            }
        }
    }
}
