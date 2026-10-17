//! Presence across bridges: who is editing which file inside a shared subtree.
//!
//! Awareness states are per connection and per document, so an owner's device editing
//! `Projects/X/a.md` is invisible to a friend whose vault has the same note at
//! `Shared/X/a.md`. The mirror fixes that at the server: every awareness change on a vault or
//! share document is pushed through a channel to one task, which re-publishes the state into
//! every document bridged to it under a synthetic client id, with the `file` field remapped and
//! a `_via` marker so mirrored states are never mirrored again. Only clients whose `file` lies
//! inside the shared subtree are mirrored; where you are in the rest of your vault stays private.
//!
//! Clients renew their awareness every 15 s and drop states that go 30 s without a renewal, so
//! a mirror follows its source automatically; explicit removals (disconnect, leaving the folder)
//! are mirrored as `null` states right away.

use std::collections::HashMap;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use yrs::block::ClientID;
use yrs::sync::awareness::{AwarenessUpdate, AwarenessUpdateEntry};

use crate::vault::{DocRegistry, LiveDoc};

pub type PresenceEvent = (String, AwarenessUpdate);
pub type PresenceSender = mpsc::UnboundedSender<PresenceEvent>;

const VIA: &str = "_via";

#[derive(Default)]
struct MirrorState {
    /// (source doc, source client) -> targets currently holding a mirror of it.
    mirrors: HashMap<(String, ClientID), Vec<(String, ClientID)>>,
    /// Monotonic clocks per mirrored client, so re-publishing always wins over the previous state.
    clocks: HashMap<(String, ClientID), u32>,
}

impl MirrorState {
    fn next_clock(&mut self, target: &str, id: ClientID) -> u32 {
        let c = self.clocks.entry((target.to_string(), id)).or_insert(0);
        *c += 1;
        *c
    }
}

/// Derive a stable synthetic client id for a mirror. Kept below 2^53 so JavaScript clients
/// read it exactly; never collides with a real Yjs client id in practice.
fn mirror_id(src_doc: &str, src_client: ClientID, target_doc: &str) -> ClientID {
    let h = Sha256::digest(format!("{src_doc}:{}:{target_doc}", src_client.get()));
    let n = u64::from_le_bytes(h[..8].try_into().unwrap()) & ((1u64 << 53) - 1);
    ClientID::new(n.max(1))
}

pub fn spawn(docs: Arc<DocRegistry>, mut rx: mpsc::UnboundedReceiver<PresenceEvent>) {
    tokio::spawn(async move {
        let mut st = MirrorState::default();
        while let Some((doc_id, update)) = rx.recv().await {
            mirror(&docs, &doc_id, update, &mut st);
        }
    });
}

fn mirror(docs: &DocRegistry, src_id: &str, update: AwarenessUpdate, st: &mut MirrorState) {
    for (cid, entry) in update.clients {
        let json: serde_json::Value = match serde_json::from_str(&entry.json) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if json.get(VIA).is_some() {
            continue; // a mirror; never re-mirror
        }
        let targets = if json.is_null() { Vec::new() } else { targets_for(docs, src_id, &json) };

        // Retract mirrors that no longer apply (client left, or left the shared folder).
        let previous = st.mirrors.remove(&(src_id.to_string(), cid)).unwrap_or_default();
        for (tid, mid) in previous {
            if targets.iter().any(|(t, _)| t.id == tid) {
                continue;
            }
            if let Some(doc) = docs.doc_by_id(&tid) {
                let clock = st.next_clock(&tid, mid);
                apply(&doc, mid, clock, "null");
            }
            st.clocks.remove(&(tid, mid));
        }

        let mut now: Vec<(String, ClientID)> = Vec::new();
        for (doc, mirrored) in targets {
            let mid = mirror_id(src_id, cid, &doc.id);
            let clock = st.next_clock(&doc.id, mid);
            apply(&doc, mid, clock, &mirrored.to_string());
            now.push((doc.id.clone(), mid));
        }
        if !now.is_empty() {
            st.mirrors.insert((src_id.to_string(), cid), now);
        }
    }
}

/// Every document that should see this client, with the state rewritten for it.
fn targets_for(docs: &DocRegistry, src_id: &str, json: &serde_json::Value) -> Vec<(Arc<LiveDoc>, serde_json::Value)> {
    let file = json.get("file").and_then(|f| f.as_str()).map(str::to_string);
    let mut out: Vec<(Arc<LiveDoc>, serde_json::Value)> = Vec::new();
    let rewrite = |file: String, via: &str| {
        let mut v = json.clone();
        if let serde_json::Value::Object(m) = &mut v {
            m.insert("file".into(), serde_json::Value::String(file));
            m.insert(VIA.into(), serde_json::Value::String(via.to_string()));
        }
        v
    };
    for b in docs.bridges_for(src_id) {
        if b.vault.id == src_id {
            // Source is a vault: only states inside the shared subtree cross over.
            let Some(key) = file.as_deref().and_then(|f| b.vault_to_share_key(f)) else { continue };
            out.push((b.share.clone(), rewrite(key.clone(), &b.share_id)));
            for b2 in docs.bridges_for(&b.share.id) {
                if b2.vault.id == src_id {
                    continue;
                }
                if let Some(p) = b2.share_to_vault_key(&key) {
                    out.push((b2.vault.clone(), rewrite(p, &b.share_id)));
                }
            }
        } else {
            // Source is a share document (guests, or the far end of a server link): keys are
            // share-relative; map them into every vault mounting it.
            let Some(key) = file.clone() else { continue };
            if let Some(p) = b.share_to_vault_key(&key) {
                out.push((b.vault.clone(), rewrite(p, &b.share_id)));
            }
        }
    }
    out.dedup_by(|a, b| a.0.id == b.0.id);
    out
}

fn apply(doc: &LiveDoc, id: ClientID, clock: u32, json: &str) {
    let mut clients = HashMap::new();
    clients.insert(id, AwarenessUpdateEntry { clock, json: json.into() });
    let mut aw = doc.awareness.lock();
    if let Err(e) = aw.apply_update(AwarenessUpdate { clients }) {
        tracing::debug!(doc = %doc.label, "mirroring presence failed: {e}");
    }
}
