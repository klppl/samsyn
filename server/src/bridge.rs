//! A bridge keeps the subtree `vault_prefix/…` of a vault document equal to the whole of a
//! share document. Changes are replayed as plain edits with a `bridge:<source-doc-id>` origin
//! so observers can tell where they came from and never echo them back.

use std::sync::Arc;

use yrs::{Any, GetString, Map, Origin, Out, ReadTxn, Text, TextPrelim, Transact, WriteTxn};

use crate::client::diff::diff_text;
use crate::vault::{LiveDoc, BRIDGE_ORIGIN_PREFIX};

#[derive(Debug, Clone)]
pub enum OwnedDelta {
    Retain(u32),
    Delete(u32),
    Insert(String),
}

#[derive(Debug, Clone)]
pub enum ChangeKind {
    TextDelta(Vec<OwnedDelta>),
    TextSet(String),
    BlobSet(Any),
    Delete,
}

#[derive(Debug, Clone)]
pub struct Change {
    pub key: String,
    pub kind: ChangeKind,
    /// Doc id this change was bridged from, if it did not come from a client.
    pub from: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Vault,
    Share,
}

pub struct Bridge {
    pub share_id: String,
    pub mount_id: Option<String>,
    pub vault: Arc<LiveDoc>,
    pub share: Arc<LiveDoc>,
    pub vault_prefix: String,
    pub only_file: Option<String>,
    pub access: String,
    pub truth: Side,
}

fn bridge_origin(src: &str) -> Origin {
    let mut v = BRIDGE_ORIGIN_PREFIX.to_vec();
    v.extend_from_slice(src.as_bytes());
    Origin::from(v.as_slice())
}

impl Bridge {
    pub fn vault_to_share_key(&self, key: &str) -> Option<String> {
        let rest = key.strip_prefix(self.vault_prefix.as_str())?;
        if rest.is_empty() {
            return None;
        }
        match &self.only_file {
            Some(f) if f != rest => None,
            _ => Some(rest.to_string()),
        }
    }

    pub fn share_to_vault_key(&self, key: &str) -> Option<String> {
        match &self.only_file {
            Some(f) if f != key => None,
            _ => Some(format!("{}{}", self.vault_prefix, key)),
        }
    }

    /// Replay `changes` recorded on `source` into the other side. Returns the target doc if
    /// anything was applied there (so the caller continues propagation from it).
    pub fn forward(&self, source: &Arc<LiveDoc>, changes: &[Change]) -> Option<Arc<LiveDoc>> {
        let (target, to_share) = if source.id == self.vault.id {
            (&self.share, true)
        } else if source.id == self.share.id {
            (&self.vault, false)
        } else {
            return None;
        };
        let mapped: Vec<(String, &Change)> = changes
            .iter()
            .filter(|c| c.from.as_deref() != Some(target.id.as_str()))
            .filter_map(|c| {
                let k = if to_share { self.vault_to_share_key(&c.key) } else { self.share_to_vault_key(&c.key) };
                k.map(|k| (k, c))
            })
            .collect();
        if mapped.is_empty() {
            return None;
        }

        if to_share && self.access == "ro" {
            // Read-only mount: undo the vault-side edits by re-asserting the share's content.
            tracing::info!(mount = ?self.mount_id, "reverting edits on read-only mount");
            let keys: Vec<String> = mapped.iter().map(|(k, _)| k.clone()).collect();
            self.assert_keys(&self.share, &self.vault, &keys, false);
            return Some(self.vault.clone());
        }

        apply_changes(target, &source.id, &mapped);
        for (k, c) in &mapped {
            if let ChangeKind::BlobSet(a) = &c.kind {
                copy_blob(source, target, a, k);
            }
        }
        Some(target.clone())
    }

    /// Make the non-truth side equal to the truth side for the whole bridged subtree.
    pub fn reconcile(&self) {
        let (truth, other, to_share) = match self.truth {
            Side::Vault => (&self.vault, &self.share, true),
            Side::Share => (&self.share, &self.vault, false),
        };
        let truth_keys: Vec<(String, String)> = truth
            .keys()
            .into_iter()
            .filter_map(|k| {
                let mapped = if to_share { self.vault_to_share_key(&k) } else { self.share_to_vault_key(&k) };
                mapped.map(|m| (k, m))
            })
            .collect();
        let other_keys: Vec<String> = other
            .keys()
            .into_iter()
            .filter(|k| if to_share { self.share_to_vault_key(k).is_some() } else { self.vault_to_share_key(k).is_some() })
            .collect();

        // Keys only on the other side: delete.
        let truth_mapped: Vec<&String> = truth_keys.iter().map(|(_, m)| m).collect();
        let stale: Vec<String> = other_keys.into_iter().filter(|k| !truth_mapped.contains(&k)).collect();
        if !stale.is_empty() {
            let aw = other.awareness.lock().unwrap();
            let doc = aw.doc().clone();
            let mut txn = doc.transact_mut_with(bridge_origin(&truth.id));
            let texts = txn.get_or_insert_map("texts");
            let blobs = txn.get_or_insert_map("blobs");
            for k in &stale {
                texts.remove(&mut txn, k);
                blobs.remove(&mut txn, k);
            }
        }
        let keys: Vec<String> = truth_keys.iter().map(|(k, _)| k.clone()).collect();
        self.assert_keys(truth, other, &keys, to_share);
        tracing::info!(
            share = %self.share.label,
            "bridge reconciled: {} keys, {} stale removed",
            truth_keys.len(),
            stale.len()
        );
    }

    /// Copy the given keys (named in `src` terms) from `src` to `dst`, making them equal.
    fn assert_keys(&self, src: &Arc<LiveDoc>, dst: &Arc<LiveDoc>, keys: &[String], src_is_vault: bool) {
        // Read everything from src first so we never hold two awareness locks at once.
        let mut items: Vec<(String, Option<String>, Option<Any>)> = Vec::new();
        {
            let aw = src.awareness.lock().unwrap();
            let txn = aw.doc().transact();
            let texts = txn.get_map("texts");
            let blobs = txn.get_map("blobs");
            for k in keys {
                let mapped = if src_is_vault { self.vault_to_share_key(k) } else { self.share_to_vault_key(k) };
                let Some(mapped) = mapped else { continue };
                let text = texts.as_ref().and_then(|m| match m.get(&txn, k) {
                    Some(Out::YText(t)) => Some(t.get_string(&txn)),
                    _ => None,
                });
                let blob = blobs.as_ref().and_then(|m| match m.get(&txn, k) {
                    Some(Out::Any(a)) => Some(a),
                    _ => None,
                });
                items.push((mapped, text, blob));
            }
        }
        let changes: Vec<(String, Change)> = items
            .into_iter()
            .map(|(k, text, blob)| {
                let kind = match (text, blob) {
                    (Some(t), _) => ChangeKind::TextSet(t),
                    (None, Some(b)) => ChangeKind::BlobSet(b),
                    (None, None) => ChangeKind::Delete,
                };
                (k.clone(), Change { key: k, kind, from: None })
            })
            .collect();
        let refs: Vec<(String, &Change)> = changes.iter().map(|(k, c)| (k.clone(), c)).collect();
        apply_changes(dst, &src.id, &refs);
        for (k, c) in &refs {
            if let ChangeKind::BlobSet(a) = &c.kind {
                copy_blob(src, dst, a, k);
            }
        }
    }
}

/// Apply mapped changes to `target` in one transaction with origin `bridge:<src>`.
fn apply_changes(target: &Arc<LiveDoc>, src_id: &str, changes: &[(String, &Change)]) {
    let aw = target.awareness.lock().unwrap();
    let doc = aw.doc().clone();
    let mut txn = doc.transact_mut_with(bridge_origin(src_id));
    let texts = txn.get_or_insert_map("texts");
    let blobs = txn.get_or_insert_map("blobs");
    for (key, change) in changes {
        match &change.kind {
            ChangeKind::TextDelta(delta) => {
                let t = match texts.get(&txn, key) {
                    Some(Out::YText(t)) => t,
                    _ => texts.insert(&mut txn, key.as_str(), TextPrelim::new("")),
                };
                let mut pos: u32 = 0;
                for d in delta {
                    match d {
                        OwnedDelta::Retain(n) => pos += n,
                        OwnedDelta::Delete(n) => t.remove_range(&mut txn, pos, *n),
                        OwnedDelta::Insert(s) => {
                            t.insert(&mut txn, pos, s);
                            pos += s.len() as u32;
                        }
                    }
                }
            }
            ChangeKind::TextSet(content) => match texts.get(&txn, key) {
                Some(Out::YText(t)) => {
                    let cur = t.get_string(&txn);
                    if let Some(d) = diff_text(&cur, content) {
                        if d.remove > 0 {
                            t.remove_range(&mut txn, d.index as u32, d.remove as u32);
                        }
                        if !d.insert.is_empty() {
                            t.insert(&mut txn, d.index as u32, &d.insert);
                        }
                    }
                }
                _ => {
                    texts.insert(&mut txn, key.as_str(), TextPrelim::new(content.as_str()));
                }
            },
            ChangeKind::BlobSet(a) => {
                let same = matches!(blobs.get(&txn, key), Some(Out::Any(cur)) if &cur == a);
                if !same {
                    blobs.insert(&mut txn, key.as_str(), a.clone());
                }
            }
            ChangeKind::Delete => {
                texts.remove(&mut txn, key);
                blobs.remove(&mut txn, key);
            }
        }
    }
}

fn copy_blob(src: &LiveDoc, dst: &LiveDoc, meta: &Any, key: &str) {
    let Any::Map(m) = meta else { return };
    let Some(Any::String(hash)) = m.get("hash") else { return };
    if dst.store.has_blob(hash) {
        return;
    }
    match src.store.get_blob(hash) {
        Ok(Some(bytes)) => {
            if let Err(e) = dst.store.put_blob(hash, &bytes) {
                tracing::warn!("copying blob for {key} failed: {e}");
            }
        }
        Ok(None) => tracing::debug!("blob for {key} not present in source yet"),
        Err(e) => tracing::warn!("reading blob for {key} failed: {e}"),
    }
}
