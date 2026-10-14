//! The sync engine: owns the document, reconciles with disk, pushes local edits, writes remote ones.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::Context;
use notify_debouncer_mini::{new_debouncer, notify::RecursiveMode, DebounceEventResult, Debouncer};
use tokio::sync::mpsc;
use yrs::sync::{Awareness, DefaultProtocol, Message, MessageReader, Protocol, SyncMessage};
use yrs::types::{Event, PathSegment};
use yrs::updates::decoder::DecoderV1;
use yrs::updates::encoder::{Encode, Encoder, EncoderV1};
use yrs::{Any, DeepObservable, Doc, GetString, Map, MapRef, Observable, Origin, Out, Text, TextPrelim, Transact};

use super::diff::diff_text;
use super::state::{hash_bytes, write_atomic, LocalState, StateStore, META_DIR};
use super::SyncArgs;

const LOCAL: &str = "local";
const MASS_DELETE_MIN: usize = 5;
const MASS_DELETE_RATIO: f64 = 0.5;

pub struct Engine {
    args: SyncArgs,
    root: PathBuf,
    awareness: Awareness,
    texts: MapRef,
    blobs: MapRef,
    store: StateStore,
    state: LocalState,
    device: String,
    text_exts: HashSet<String>,
    excludes: Vec<String>,
    http: reqwest::Client,
    conn: Arc<Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>>,
    /// Paths changed by remote transactions, drained after every incoming message.
    dirty_remote: Arc<Mutex<HashSet<String>>>,
    doc_dirty: Arc<Mutex<bool>>,
    /// Text content each file had when disk and document last agreed. Used as the common
    /// ancestor for a three-way merge when a remote change lands while a local edit is pending.
    base: HashMap<String, String>,
    state_dirty: bool,
    root_missing: bool,
    /// (when, how many) shared files we removed recently; feeds the mass-delete guard.
    recent_deletes: VecDeque<(Instant, usize)>,
    pub synced: bool,
    pub had_cached_doc: bool,
    pub pending_deletions: Vec<String>,
}

impl Engine {
    pub fn new(args: &SyncArgs) -> anyhow::Result<Self> {
        let root = std::fs::canonicalize(&args.dir)?;
        let store = StateStore::new(&root)?;
        let install_id = StateStore::install_id(&root);
        let state = store.load_state(&install_id);

        let doc = Doc::new();
        let had_cached_doc = store.load_doc(&doc);
        let texts = doc.get_or_insert_map("texts");
        let blobs = doc.get_or_insert_map("blobs");

        let conn: Arc<Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>> = Arc::new(Mutex::new(None));
        let dirty_remote: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let doc_dirty = Arc::new(Mutex::new(false));

        // Local commits go to the server; every commit marks the cache dirty.
        {
            let conn = conn.clone();
            let doc_dirty = doc_dirty.clone();
            doc.observe_update_v1("replica-client", move |txn, ev| {
                *doc_dirty.lock().unwrap() = true;
                if is_local(txn.origin()) {
                    if let Some(tx) = conn.lock().unwrap().as_ref() {
                        let _ = tx.send(encode(&Message::Sync(SyncMessage::Update(ev.update.clone()))));
                    }
                }
            })?;
        }
        // Remote changes are collected by path and written to disk after the message is handled.
        {
            let dirty = dirty_remote.clone();
            texts.observe_deep("replica-client", move |txn, events| {
                if is_local(txn.origin()) {
                    return;
                }
                let mut d = dirty.lock().unwrap();
                for ev in events.iter() {
                    match ev {
                        Event::Map(m) if ev.path().is_empty() => {
                            for (k, _) in m.keys(txn) {
                                d.insert(k.to_string());
                            }
                        }
                        _ => {
                            if let Some(PathSegment::Key(k)) = ev.path().front() {
                                d.insert(k.to_string());
                            }
                        }
                    }
                }
            });
        }
        {
            let dirty = dirty_remote.clone();
            blobs.observe("replica-client", move |txn, ev| {
                if is_local(txn.origin()) {
                    return;
                }
                let mut d = dirty.lock().unwrap();
                for (k, _) in ev.keys(txn) {
                    d.insert(k.to_string());
                }
            });
        }

        let device = args
            .device
            .clone()
            .unwrap_or_else(|| gethostname::gethostname().to_string_lossy().to_string());
        let mut awareness = Awareness::new(doc);
        let _ = awareness.set_local_state(serde_json::json!({ "user": { "name": device } }));

        let text_exts = args
            .text_ext
            .split(',')
            .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();

        Ok(Self {
            args: args.clone(),
            root,
            awareness,
            texts,
            blobs,
            store,
            state,
            device,
            text_exts,
            excludes: args.exclude.clone(),
            http: reqwest::Client::new(),
            conn,
            dirty_remote,
            doc_dirty,
            base: HashMap::new(),
            state_dirty: false,
            root_missing: false,
            recent_deletes: VecDeque::new(),
            synced: false,
            had_cached_doc,
            pending_deletions: Vec::new(),
        })
    }

    // ------------------------------------------------------------------ helpers

    fn doc(&self) -> &Doc {
        self.awareness.doc()
    }

    fn rel(&self, abs: &Path) -> Option<String> {
        let r = abs.strip_prefix(&self.root).ok()?;
        let s = r.to_string_lossy().replace('\\', "/");
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }

    fn abs(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn is_text(&self, rel: &str) -> bool {
        rel.rsplit_once('.')
            .map(|(_, ext)| self.text_exts.contains(&ext.to_ascii_lowercase()))
            .unwrap_or(false)
    }

    fn should_sync(&self, rel: &str) -> bool {
        if rel.split('/').any(|seg| seg.starts_with('.')) {
            return false;
        }
        if rel.starts_with(META_DIR) {
            return false;
        }
        if self.excludes.iter().any(|e| rel == e.trim_end_matches('/') || rel.starts_with(&format!("{}/", e.trim_end_matches('/')))) {
            return false;
        }
        self.is_text(rel) || !self.args.no_binary
    }

    fn mark_synced(&mut self, rel: &str, hash: String) {
        if self.state.synced.get(rel) != Some(&hash) {
            self.state.synced.insert(rel.to_string(), hash);
            self.state_dirty = true;
        }
    }

    fn forget_synced(&mut self, rel: &str) {
        if self.state.synced.remove(rel).is_some() {
            self.state_dirty = true;
        }
    }

    fn text_of(&self, rel: &str) -> Option<String> {
        let txn = self.doc().transact();
        match self.texts.get(&txn, rel) {
            Some(Out::YText(t)) => Some(t.get_string(&txn)),
            _ => None,
        }
    }

    fn blob_of(&self, rel: &str) -> Option<BlobMeta> {
        let txn = self.doc().transact();
        match self.blobs.get(&txn, rel) {
            Some(Out::Any(Any::Map(m))) => BlobMeta::from_any(&m),
            _ => None,
        }
    }

    fn doc_keys(&self) -> (Vec<String>, Vec<String>) {
        let txn = self.doc().transact();
        let t: Vec<String> = self.texts.keys(&txn).map(|k| k.to_string()).collect();
        let b: Vec<String> = self.blobs.keys(&txn).map(|k| k.to_string()).collect();
        (t, b)
    }

    fn conflict_path(&self, rel: &str) -> String {
        let (base, ext) = match rel.rsplit_once('.') {
            Some((b, e)) if !b.contains('/') || b.rfind('/') < rel.rfind('.') => (b.to_string(), format!(".{e}")),
            _ => (rel.to_string(), String::new()),
        };
        let stamp = chrono::Local::now().format("%Y-%m-%d %H%M");
        format!("{base} (conflict {} {stamp}){ext}", self.device)
    }

    // ------------------------------------------------------------------ connection

    pub fn on_connected(&mut self, tx: mpsc::UnboundedSender<Vec<u8>>) {
        let mut enc = EncoderV1::new();
        if let Err(e) = DefaultProtocol.start(&self.awareness, &mut enc) {
            tracing::warn!("protocol start failed: {e}");
        }
        let _ = tx.send(enc.to_vec());
        *self.conn.lock().unwrap() = Some(tx);
        tracing::info!("connected");
    }

    pub fn on_disconnected(&mut self) {
        *self.conn.lock().unwrap() = None;
        self.synced = false;
    }

    pub async fn on_message(&mut self, data: &[u8]) {
        let mut decoder = DecoderV1::from(data);
        let reader = MessageReader::new(&mut decoder);
        let mut replies = Vec::new();
        let mut got_step2 = false;
        for msg in reader {
            let msg = match msg {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!("bad message from server: {e}");
                    break;
                }
            };
            if matches!(msg, Message::Sync(SyncMessage::SyncStep2(_))) {
                got_step2 = true;
            }
            match DefaultProtocol.handle_message(&mut self.awareness, msg) {
                Ok(Some(reply)) => replies.push(encode(&reply)),
                Ok(None) => {}
                Err(e) => tracing::warn!("protocol error: {e}"),
            }
        }
        if let Some(tx) = self.conn.lock().unwrap().as_ref() {
            for r in replies {
                let _ = tx.send(r);
            }
        }
        if got_step2 && !self.synced {
            self.synced = true;
            tracing::info!("initial sync received");
        }
        self.flush_remote().await;
    }

    // ------------------------------------------------------------------ remote -> disk

    async fn flush_remote(&mut self) {
        let paths: Vec<String> = {
            let mut d = self.dirty_remote.lock().unwrap();
            d.drain().collect()
        };
        for rel in paths {
            if !self.should_sync(&rel) {
                continue;
            }
            if let Err(e) = self.apply_remote(&rel).await {
                tracing::warn!("applying remote change to {rel} failed: {e}");
            }
        }
    }

    async fn apply_remote(&mut self, rel: &str) -> anyhow::Result<()> {
        if let Some(content) = self.text_of(rel) {
            let abs = self.abs(rel);
            if let Ok(local) = std::fs::read_to_string(&abs) {
                if local == content {
                    self.mark_synced(rel, hash_bytes(content.as_bytes()));
                    self.base.insert(rel.to_string(), content);
                    return Ok(());
                }
                match self.base.get(rel).cloned() {
                    Some(base) if base != local => {
                        // Local has an unpushed edit. Merge it against the remote change.
                        return self.merge_local_edit(rel, &base, &local, &content);
                    }
                    Some(_) => {}
                    None => {
                        let local_hash = hash_bytes(local.as_bytes());
                        if self.state.synced.get(rel).map(|h| h != &local_hash).unwrap_or(true) {
                            self.write_conflict_copy(rel, local.as_bytes())?;
                        }
                    }
                }
            }
            self.write_text(rel, &content)?;
            return Ok(());
        }
        if let Some(meta) = self.blob_of(rel) {
            return self.download_blob(rel, &meta).await;
        }
        // Gone from the document: move the local file to .replica/trash.
        self.trash_local(rel)?;
        Ok(())
    }

    /// Three-way merge of a pending local edit (base -> local) with a remote change
    /// (base -> remote) that has already been applied to the document. Non-overlapping spans
    /// are merged by replaying the local span into the CRDT; overlapping ones keep a copy.
    fn merge_local_edit(&mut self, rel: &str, base: &str, local: &str, remote: &str) -> anyhow::Result<()> {
        let Some(dl) = diff_text(base, local) else {
            return self.write_text(rel, remote);
        };
        let Some(dr) = diff_text(base, remote) else {
            // Remote did not change the text after all; just push ours.
            self.push_text(rel, local);
            return Ok(());
        };
        let local_end = dl.index + dl.remove;
        let remote_end = dr.index + dr.remove;
        let disjoint = local_end <= dr.index || remote_end <= dl.index;
        // Pure insertions at the same point are not a conflict either; order remote first.
        let same_point_inserts = dl.remove == 0 && dr.remove == 0 && dl.index == dr.index;
        if !disjoint && !same_point_inserts {
            tracing::warn!("overlapping concurrent edits on {rel}; keeping local copy");
            self.write_conflict_copy(rel, local.as_bytes())?;
            return self.write_text(rel, remote);
        }
        let shift: isize = if dr.index <= dl.index {
            dr.insert.len() as isize - dr.remove as isize
        } else {
            0
        };
        let index = (dl.index as isize + shift) as usize;
        let merged = {
            let doc = self.doc().clone();
            let mut txn = doc.transact_mut_with(LOCAL);
            if let Some(Out::YText(t)) = self.texts.get(&txn, rel) {
                if dl.remove > 0 {
                    t.remove_range(&mut txn, index as u32, dl.remove as u32);
                }
                if !dl.insert.is_empty() {
                    t.insert(&mut txn, index as u32, &dl.insert);
                }
                t.get_string(&txn)
            } else {
                remote.to_string()
            }
        };
        tracing::info!("merged concurrent edits on {rel}");
        self.write_text(rel, &merged)
    }

    fn write_text(&mut self, rel: &str, content: &str) -> anyhow::Result<()> {
        self.mark_synced(rel, hash_bytes(content.as_bytes()));
        self.base.insert(rel.to_string(), content.to_string());
        write_atomic(&self.abs(rel), content.as_bytes())?;
        tracing::info!("wrote {rel}");
        Ok(())
    }

    fn write_conflict_copy(&self, rel: &str, data: &[u8]) -> anyhow::Result<()> {
        let cp = self.conflict_path(rel);
        write_atomic(&self.abs(&cp), data)?;
        tracing::warn!("conflict on {rel}; local version kept as {cp}");
        Ok(())
    }

    fn trash_local(&mut self, rel: &str) -> anyhow::Result<()> {
        self.forget_synced(rel);
        self.base.remove(rel);
        let abs = self.abs(rel);
        if !abs.exists() {
            return Ok(());
        }
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let dest = self.store.trash_dir().join(stamp.to_string()).join(rel);
        std::fs::create_dir_all(dest.parent().unwrap())?;
        std::fs::rename(&abs, &dest)?;
        tracing::info!("removed {rel} (kept in {})", dest.display());
        Ok(())
    }

    async fn download_blob(&mut self, rel: &str, meta: &BlobMeta) -> anyhow::Result<()> {
        let abs = self.abs(rel);
        if let Ok(local) = std::fs::read(&abs) {
            let local_hash = hash_bytes(&local);
            if local_hash == meta.hash {
                self.mark_synced(rel, local_hash);
                return Ok(());
            }
            if self.state.synced.get(rel).map(|h| h != &local_hash).unwrap_or(false) {
                self.write_conflict_copy(rel, &local)?;
            }
        }
        let url = format!("{}/blobs/{}/{}", self.args.http_base(), self.args.vault, meta.hash);
        let res = self.http.get(&url).bearer_auth(&self.args.token).send().await?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            tracing::warn!("blob for {rel} not on server yet");
            return Ok(());
        }
        let bytes = res.error_for_status()?.bytes().await?;
        self.mark_synced(rel, meta.hash.clone());
        write_atomic(&abs, &bytes)?;
        tracing::info!("downloaded {rel} ({} bytes)", bytes.len());
        Ok(())
    }

    // ------------------------------------------------------------------ disk -> doc

    pub fn start_watcher(&self, tx: mpsc::UnboundedSender<Vec<PathBuf>>) -> anyhow::Result<Debouncer<notify_debouncer_mini::notify::RecommendedWatcher>> {
        let mut debouncer = new_debouncer(Duration::from_millis(self.args.debounce_ms), move |res: DebounceEventResult| {
            if let Ok(events) = res {
                let paths: Vec<PathBuf> = events.into_iter().map(|e| e.path).collect();
                if !paths.is_empty() {
                    let _ = tx.send(paths);
                }
            }
        })?;
        debouncer.watcher().watch(&self.root, RecursiveMode::Recursive).context("starting file watcher")?;
        Ok(debouncer)
    }

    /// Handle one debounced batch of filesystem events. Removals are counted across the whole
    /// batch (and a rolling window) so deleting a folder cannot sneak past the mass-delete guard
    /// as many single-file deletions. Returns true if anything syncable was touched.
    pub async fn on_fs_batch(&mut self, paths: &[PathBuf]) -> bool {
        if !self.root.is_dir() {
            if !self.root_missing {
                self.root_missing = true;
                tracing::error!(
                    "sync directory {} disappeared; ignoring all local events until it is back",
                    self.root.display()
                );
            }
            return false;
        }
        self.root_missing = false;

        let mut touched = false;
        let mut removed: Vec<String> = Vec::new();
        let mut changed: Vec<String> = Vec::new();
        let mut seen = HashSet::new();
        for abs in paths {
            let Some(rel) = self.rel(abs) else { continue };
            if !self.should_sync(&rel) || !seen.insert(rel.clone()) {
                continue;
            }
            touched = true;
            if abs.exists() {
                changed.push(rel);
            } else {
                removed.push(rel);
            }
        }

        if !removed.is_empty() {
            let (t, b) = self.doc_keys();
            let total = t.len() + b.len();
            let mut gone: Vec<String> = Vec::new();
            for k in t.into_iter().chain(b) {
                if removed.iter().any(|r| &k == r || k.starts_with(&format!("{r}/"))) {
                    gone.push(k);
                }
            }
            if !gone.is_empty() {
                let now = Instant::now();
                while self.recent_deletes.front().map(|(t, _)| now.duration_since(*t) > Duration::from_secs(30)).unwrap_or(false) {
                    self.recent_deletes.pop_front();
                }
                let recent: usize = self.recent_deletes.iter().map(|(_, n)| n).sum();
                let n = gone.len() + recent;
                let mass = n >= MASS_DELETE_MIN && (n as f64) > ((total + recent) as f64) * MASS_DELETE_RATIO;
                if mass && !self.args.allow_mass_delete {
                    tracing::warn!(
                        "{} files removed locally would delete {n} of {} shared files; held back.                          Restart with --allow-mass-delete if intended; otherwise they are restored on next reconcile.",
                        gone.len(),
                        total + recent
                    );
                    self.pending_deletions.extend(gone);
                } else {
                    self.recent_deletes.push_back((now, gone.len()));
                    for k in gone {
                        self.remove_from_doc(&k);
                    }
                }
            }
        }

        for rel in changed {
            if let Err(e) = self.push_local(&rel).await {
                tracing::warn!("pushing {rel} failed: {e}");
            }
        }
        touched
    }

    /// Make the document reflect what is on disk at `rel` (file changed, created or removed).
    async fn push_local(&mut self, rel: &str) -> anyhow::Result<()> {
        let abs = self.abs(rel);
        let meta = match std::fs::metadata(&abs) {
            Ok(m) => m,
            Err(_) => {
                // Removed: drop the key and anything below it if it was a directory.
                let (t, b) = self.doc_keys();
                let total = t.len() + b.len();
                let prefix = format!("{rel}/");
                let gone: Vec<String> = t.into_iter().chain(b).filter(|k| k == rel || k.starts_with(&prefix)).collect();
                let mass = gone.len() >= MASS_DELETE_MIN && (gone.len() as f64) > (total as f64) * MASS_DELETE_RATIO;
                if mass && !self.args.allow_mass_delete {
                    tracing::warn!(
                        "{rel} removal would delete {} of {total} shared files; held back.                          Restart with --allow-mass-delete if intended; otherwise the files are restored on next reconcile.",
                        gone.len()
                    );
                    self.pending_deletions = gone;
                    return Ok(());
                }
                for k in gone {
                    self.remove_from_doc(&k);
                }
                return Ok(());
            }
        };
        if meta.is_dir() {
            // New directory (e.g. moved in): push its contents.
            for entry in walkdir::WalkDir::new(&abs).into_iter().filter_map(|e| e.ok()) {
                if entry.file_type().is_file() {
                    if let Some(r) = self.rel(entry.path()) {
                        if self.should_sync(&r) {
                            Box::pin(self.push_local(&r)).await?;
                        }
                    }
                }
            }
            return Ok(());
        }
        if self.is_text(rel) {
            let bytes = std::fs::read(&abs)?;
            let content = match String::from_utf8(bytes) {
                Ok(s) => s,
                Err(_) => {
                    tracing::warn!("{rel} is not valid UTF-8; skipping");
                    return Ok(());
                }
            };
            self.push_text(rel, &content);
        } else {
            self.push_blob(rel).await?;
        }
        Ok(())
    }

    fn push_text(&mut self, rel: &str, content: &str) {
        let doc = self.doc().clone();
        let mut txn = doc.transact_mut_with(LOCAL);
        match self.texts.get(&txn, rel) {
            Some(Out::YText(t)) => {
                let current = t.get_string(&txn);
                if let Some(d) = diff_text(&current, content) {
                    if d.remove > 0 {
                        t.remove_range(&mut txn, d.index as u32, d.remove as u32);
                    }
                    if !d.insert.is_empty() {
                        t.insert(&mut txn, d.index as u32, &d.insert);
                    }
                    tracing::info!("pushed {rel} (-{} +{} bytes)", d.remove, d.insert.len());
                }
            }
            _ => {
                self.texts.insert(&mut txn, rel, TextPrelim::new(content));
                tracing::info!("added {rel}");
            }
        }
        drop(txn);
        self.mark_synced(rel, hash_bytes(content.as_bytes()));
        self.base.insert(rel.to_string(), content.to_string());
    }

    async fn push_blob(&mut self, rel: &str) -> anyhow::Result<()> {
        let abs = self.abs(rel);
        let data = std::fs::read(&abs)?;
        let hash = hash_bytes(&data);
        if self.blob_of(rel).map(|m| m.hash == hash).unwrap_or(false) {
            self.mark_synced(rel, hash);
            return Ok(());
        }
        let url = format!("{}/blobs/{}/{}", self.args.http_base(), self.args.vault, hash);
        let size = data.len();
        let res = self.http.put(&url).bearer_auth(&self.args.token).body(data).send().await?;
        if !res.status().is_success() {
            anyhow::bail!("upload failed: HTTP {}", res.status());
        }
        let mtime = std::fs::metadata(&abs)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as f64)
            .unwrap_or(0.0);
        let meta = BlobMeta { hash: hash.clone(), size, mtime };
        {
            let doc = self.doc().clone();
            let mut txn = doc.transact_mut_with(LOCAL);
            self.blobs.insert(&mut txn, rel, meta.to_any());
        }
        self.mark_synced(rel, hash);
        tracing::info!("uploaded {rel} ({size} bytes)");
        Ok(())
    }

    fn remove_from_doc(&mut self, rel: &str) {
        let doc = self.doc().clone();
        let mut txn = doc.transact_mut_with(LOCAL);
        let a = self.texts.remove(&mut txn, rel).is_some();
        let b = self.blobs.remove(&mut txn, rel).is_some();
        drop(txn);
        if a || b {
            tracing::info!("deleted {rel} from shared vault");
        }
        self.forget_synced(rel);
        self.base.remove(rel);
    }

    // ------------------------------------------------------------------ reconcile

    pub async fn reconcile(&mut self) -> anyhow::Result<()> {
        let mut local: HashMap<String, PathBuf> = HashMap::new();
        for entry in walkdir::WalkDir::new(&self.root)
            .into_iter()
            .filter_entry(|e| e.depth() == 0 || !e.file_name().to_string_lossy().starts_with('.'))
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            if let Some(rel) = self.rel(entry.path()) {
                if self.should_sync(&rel) {
                    local.insert(rel, entry.path().to_path_buf());
                }
            }
        }

        for rel in local.keys().cloned().collect::<Vec<_>>() {
            let r = if self.is_text(&rel) { self.reconcile_text(&rel).await } else { self.reconcile_blob(&rel).await };
            if let Err(e) = r {
                tracing::error!("reconcile failed for {rel}: {e}");
            }
        }

        let (t_keys, b_keys) = self.doc_keys();
        let mut to_delete = Vec::new();
        for rel in t_keys.iter().chain(b_keys.iter()) {
            if local.contains_key(rel) || !self.should_sync(rel) {
                continue;
            }
            if self.state.synced.contains_key(rel) {
                to_delete.push(rel.clone());
            } else if let Some(c) = self.text_of(rel) {
                self.write_text(rel, &c)?;
            } else if let Some(m) = self.blob_of(rel) {
                self.download_blob(rel, &m).await?;
            }
        }

        let total = t_keys.len() + b_keys.len();
        let mass = to_delete.len() >= MASS_DELETE_MIN && (to_delete.len() as f64) > (total as f64) * MASS_DELETE_RATIO;
        if mass && !self.args.allow_mass_delete {
            tracing::warn!(
                "refusing to delete {} of {total} shared files from this directory; re-downloading them instead. \
                 Re-run with --allow-mass-delete if the removal was intended.",
                to_delete.len()
            );
            for rel in &to_delete {
                self.forget_synced(rel);
                if let Some(c) = self.text_of(rel) {
                    self.write_text(rel, &c)?;
                } else if let Some(m) = self.blob_of(rel) {
                    self.download_blob(rel, &m).await?;
                }
            }
            self.pending_deletions = to_delete;
        } else {
            for rel in to_delete {
                self.remove_from_doc(&rel);
            }
        }
        self.store.save_state(&self.state)?;
        self.state_dirty = false;
        tracing::info!("reconcile done: {} local files, {} shared", local.len(), total);
        Ok(())
    }

    async fn reconcile_text(&mut self, rel: &str) -> anyhow::Result<()> {
        let bytes = std::fs::read(self.abs(rel))?;
        let local = match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(_) => return Ok(()),
        };
        let local_hash = hash_bytes(local.as_bytes());
        let last = self.state.synced.get(rel).cloned();
        let Some(remote) = self.text_of(rel) else {
            if last.is_some() {
                self.trash_local(rel)?;
            } else {
                self.push_text(rel, &local);
            }
            return Ok(());
        };
        if remote == local {
            self.mark_synced(rel, local_hash);
            self.base.insert(rel.to_string(), local);
        } else if last.as_deref() == Some(local_hash.as_str()) {
            self.write_text(rel, &remote)?;
        } else if last.as_deref() == Some(hash_bytes(remote.as_bytes()).as_str()) {
            self.push_text(rel, &local);
        } else {
            self.write_conflict_copy(rel, local.as_bytes())?;
            self.write_text(rel, &remote)?;
        }
        Ok(())
    }

    async fn reconcile_blob(&mut self, rel: &str) -> anyhow::Result<()> {
        let abs = self.abs(rel);
        let last = self.state.synced.get(rel).cloned();
        let Some(meta) = self.blob_of(rel) else {
            if last.is_some() {
                self.trash_local(rel)?;
            } else {
                self.push_blob(rel).await?;
            }
            return Ok(());
        };
        let data = std::fs::read(&abs)?;
        let local_hash = hash_bytes(&data);
        if local_hash == meta.hash {
            self.mark_synced(rel, local_hash);
        } else if last.as_deref() == Some(local_hash.as_str()) {
            self.download_blob(rel, &meta).await?;
        } else if last.as_deref() == Some(meta.hash.as_str()) {
            self.push_blob(rel).await?;
        } else {
            let mtime = std::fs::metadata(&abs)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as f64)
                .unwrap_or(0.0);
            if mtime > meta.mtime {
                self.push_blob(rel).await?;
            } else {
                self.write_conflict_copy(rel, &data)?;
                self.download_blob(rel, &meta).await?;
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------ persistence

    pub fn flush_persistence(&mut self) {
        let dirty = std::mem::replace(&mut *self.doc_dirty.lock().unwrap(), false);
        if dirty {
            if let Err(e) = self.store.save_doc(self.doc()) {
                tracing::warn!("saving doc cache failed: {e}");
            }
        }
        if self.state_dirty {
            match self.store.save_state(&self.state) {
                Ok(()) => self.state_dirty = false,
                Err(e) => tracing::warn!("saving state failed: {e}"),
            }
        }
    }

    pub fn shutdown(&mut self) {
        *self.doc_dirty.lock().unwrap() = true;
        self.flush_persistence();
    }
}

fn is_local(origin: Option<&Origin>) -> bool {
    origin.map(|o| o == &Origin::from(LOCAL)).unwrap_or(false)
}

fn encode(msg: &Message) -> Vec<u8> {
    let mut enc = EncoderV1::new();
    msg.encode(&mut enc);
    enc.to_vec()
}

#[derive(Debug, Clone)]
struct BlobMeta {
    hash: String,
    size: usize,
    mtime: f64,
}

impl BlobMeta {
    fn from_any(m: &HashMap<String, Any>) -> Option<Self> {
        let hash = match m.get("hash") {
            Some(Any::String(s)) => s.to_string(),
            _ => return None,
        };
        let num = |k: &str| match m.get(k) {
            Some(Any::Number(n)) => n.as_f64().unwrap_or(0.0),
            _ => 0.0,
        };
        let size = num("size") as usize;
        let mtime = num("mtime");
        Some(Self { hash, size, mtime })
    }

    fn to_any(&self) -> Any {
        let mut m = HashMap::new();
        m.insert("hash".to_string(), Any::String(self.hash.clone().into()));
        m.insert("size".to_string(), Any::Number(yrs::Number::from(self.size as f64)));
        m.insert("mtime".to_string(), Any::Number(yrs::Number::from(self.mtime)));
        Any::Map(Arc::new(m))
    }
}
