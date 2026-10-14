//! In-memory vault documents with persistence and fan-out broadcasting.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use tokio::sync::broadcast;
use yrs::sync::{Awareness, Message, SyncMessage};
use yrs::updates::encoder::{Encode, Encoder, EncoderV1};
use yrs::{ReadTxn, StateVector, Transact};

use crate::storage::{Storage, VaultInfo, VaultStore};

/// Broadcast channel capacity per vault. Slow consumers that lag get a full resync.
const BROADCAST_CAPACITY: usize = 4096;

pub struct Vault {
    pub info: VaultInfo,
    pub store: VaultStore,
    pub awareness: Mutex<Awareness>,
    pub tx: broadcast::Sender<Bytes>,
}

impl Vault {
    fn load(info: VaultInfo, store: VaultStore) -> anyhow::Result<Arc<Self>> {
        let (doc, n) = store.load_doc()?;
        if store.needs_compaction() {
            let full = doc.transact().encode_state_as_update_v1(&StateVector::default());
            store.compact(&full)?;
            tracing::info!(vault = %info.name, "compacted {n} updates into one");
        }

        let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);

        {
            let store = store.clone();
            let tx = tx.clone();
            let name = info.name.clone();
            doc.observe_update_v1("replica", move |_txn, event| {
                if let Err(e) = store.append_update(&event.update) {
                    tracing::error!(vault = %name, "failed to persist update: {e}");
                }
                let msg = Message::Sync(SyncMessage::Update(event.update.clone()));
                let _ = tx.send(encode_message(&msg));
            })?;
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

        tracing::info!(vault = %info.name, user = %info.user_id, "loaded vault ({n} stored updates)");
        Ok(Arc::new(Vault { info, store, awareness: Mutex::new(awareness), tx }))
    }

    pub fn full_state(&self) -> Vec<u8> {
        let aw = self.awareness.lock().unwrap();
        let state = aw.doc().transact().encode_state_as_update_v1(&StateVector::default());
        state
    }

    /// Snapshot + compact + prune. Must not be called from inside an observer callback.
    pub fn snapshot(&self, keep: usize) -> anyhow::Result<()> {
        let aw = self.awareness.lock().unwrap();
        let path = self.store.write_snapshot(aw.doc(), &self.info.id)?;
        let full = aw.doc().transact().encode_state_as_update_v1(&StateVector::default());
        drop(aw);
        self.store.compact(&full)?;
        let pruned = self.store.prune_snapshots(keep)?;
        tracing::info!(vault = %self.info.name, "snapshot {} written, {pruned} pruned", path.display());
        Ok(())
    }
}

pub fn encode_message(msg: &Message) -> Bytes {
    let mut enc = EncoderV1::new();
    msg.encode(&mut enc);
    Bytes::from(enc.to_vec())
}

pub struct VaultRegistry {
    storage: Storage,
    vaults: Mutex<HashMap<String, Arc<Vault>>>,
}

impl VaultRegistry {
    pub fn new(storage: Storage) -> Self {
        Self { storage, vaults: Mutex::new(HashMap::new()) }
    }

    /// Get the live document for `user`'s vault `name`, loading or creating it as needed.
    pub fn get_or_load(&self, user: &crate::storage::User, name: &str) -> anyhow::Result<Arc<Vault>> {
        let (info, store) = self.storage.open_vault_for_user(user, name)?;
        let mut map = self.vaults.lock().unwrap();
        if let Some(v) = map.get(&info.id) {
            return Ok(v.clone());
        }
        let v = Vault::load(info.clone(), store)?;
        map.insert(info.id, v.clone());
        Ok(v)
    }

    pub fn loaded_count(&self) -> usize {
        self.vaults.lock().unwrap().len()
    }

    pub fn maybe_compact(&self, vault: &Vault) {
        if vault.store.needs_compaction() {
            let full = vault.full_state();
            match vault.store.compact(&full) {
                Ok(()) => tracing::info!(vault = %vault.info.name, "compacted update log"),
                Err(e) => tracing::error!(vault = %vault.info.name, "compaction failed: {e}"),
            }
        }
    }

    pub fn snapshot_all(&self, keep: usize) {
        let vaults: Vec<Arc<Vault>> = self.vaults.lock().unwrap().values().cloned().collect();
        for v in vaults {
            if let Err(e) = v.snapshot(keep) {
                tracing::error!(vault = %v.info.name, "snapshot failed: {e}");
            }
        }
    }
}
