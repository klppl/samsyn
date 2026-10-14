//! In-memory vault documents with persistence and fan-out broadcasting.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use tokio::sync::broadcast;
use yrs::sync::{Awareness, Message, SyncMessage};
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::{Encode, Encoder, EncoderV1};
use yrs::{Doc, ReadTxn, StateVector, Transact};

use crate::store::Store;

/// Broadcast channel capacity per vault. Slow consumers that lag get dropped and must resync.
const BROADCAST_CAPACITY: usize = 4096;

pub struct Vault {
    pub id: String,
    pub awareness: Mutex<Awareness>,
    pub tx: broadcast::Sender<Bytes>,
}

impl Vault {
    fn load(id: &str, store: Store) -> anyhow::Result<Arc<Self>> {
        let doc = Doc::new();
        let updates = store.load_updates(id)?;
        let n = updates.len();
        {
            let mut txn = doc.transact_mut();
            for u in updates {
                match yrs::Update::decode_v1(&u) {
                    Ok(update) => {
                        if let Err(e) = txn.apply_update(update) {
                            tracing::warn!(vault = id, "skipping corrupt update: {e}");
                        }
                    }
                    Err(e) => tracing::warn!(vault = id, "skipping undecodable update: {e}"),
                }
            }
        }
        if store.needs_compaction(id) {
            let full = doc.transact().encode_state_as_update_v1(&StateVector::default());
            store.compact(id, &full)?;
            tracing::info!(vault = id, "compacted {n} updates into one");
        }

        let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);

        // Persist and broadcast every committed change.
        {
            let store = store.clone();
            let tx = tx.clone();
            let id = id.to_string();
            doc.observe_update_v1("replica", move |_txn, event| {
                if let Err(e) = store.append_update(&id, &event.update) {
                    tracing::error!(vault = %id, "failed to persist update: {e}");
                }
                let msg = Message::Sync(SyncMessage::Update(event.update.clone()));
                let _ = tx.send(encode_message(&msg));
            })?;
        }

        let mut awareness = Awareness::new(doc);
        {
            let tx = tx.clone();
            awareness.on_update("replica", move |aw, event, _origin| {
                let changed = event.all_changes();
                if let Ok(update) = aw.update_with_clients(changed) {
                    let _ = tx.send(encode_message(&Message::Awareness(update)));
                }
            });
        }

        tracing::info!(vault = id, "loaded vault ({n} stored updates)");
        Ok(Arc::new(Vault {
            id: id.to_string(),
            awareness: Mutex::new(awareness),
            tx,
        }))
    }

    /// Full document state, used for compaction.
    pub fn full_state(&self) -> Vec<u8> {
        let aw = self.awareness.lock().unwrap();
        let state = aw.doc().transact().encode_state_as_update_v1(&StateVector::default());
        state
    }
}

pub fn encode_message(msg: &Message) -> Bytes {
    let mut enc = EncoderV1::new();
    msg.encode(&mut enc);
    Bytes::from(enc.to_vec())
}

pub struct VaultRegistry {
    store: Store,
    vaults: Mutex<HashMap<String, Arc<Vault>>>,
}

impl VaultRegistry {
    pub fn new(store: Store) -> Self {
        Self { store, vaults: Mutex::new(HashMap::new()) }
    }

    pub fn get_or_load(&self, id: &str) -> anyhow::Result<Arc<Vault>> {
        let mut map = self.vaults.lock().unwrap();
        if let Some(v) = map.get(id) {
            return Ok(v.clone());
        }
        let v = Vault::load(id, self.store.clone())?;
        map.insert(id.to_string(), v.clone());
        Ok(v)
    }

    pub fn loaded_count(&self) -> usize {
        self.vaults.lock().unwrap().len()
    }

    /// Compact the on-disk log of a vault if it has grown large. Safe to call any time
    /// no transaction is held (i.e. not from inside an observer callback).
    pub fn maybe_compact(&self, vault: &Vault) {
        if self.store.needs_compaction(&vault.id) {
            let full = vault.full_state();
            match self.store.compact(&vault.id, &full) {
                Ok(()) => tracing::info!(vault = %vault.id, "compacted update log"),
                Err(e) => tracing::error!(vault = %vault.id, "compaction failed: {e}"),
            }
        }
    }
}

pub fn valid_vault_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
