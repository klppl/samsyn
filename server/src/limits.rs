//! Small, shared rate and concurrency limits. Every limit here is bounded in memory and keyed
//! by something an attacker cannot choose freely (client address, node id, credential hash).

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// Failure budget: more than `max` failures from one key within `window` → blocked for the
/// rest of the window. Maps are hard-capped so a flood from many keys cannot grow them.
pub struct FailureBudget<K: Hash + Eq + Clone> {
    max: u32,
    window: Duration,
    cap: usize,
    fails: HashMap<K, (u32, Instant)>,
}

impl<K: Hash + Eq + Clone> FailureBudget<K> {
    pub fn new(max: u32, window: Duration) -> Self {
        Self { max, window, cap: 10_000, fails: HashMap::new() }
    }

    pub fn blocked(&mut self, key: &K) -> bool {
        match self.fails.get(key) {
            Some((n, since)) if since.elapsed() < self.window => *n >= self.max,
            Some(_) => {
                self.fails.remove(key);
                false
            }
            None => false,
        }
    }

    pub fn record_failure(&mut self, key: K) {
        if self.fails.len() >= self.cap {
            let w = self.window;
            self.fails.retain(|_, (_, since)| since.elapsed() < w);
            if self.fails.len() >= self.cap {
                self.fails.clear(); // flooded from many keys: start over rather than grow
            }
        }
        let e = self.fails.entry(key).or_insert((0, Instant::now()));
        if e.1.elapsed() >= self.window {
            *e = (0, Instant::now());
        }
        e.0 += 1;
    }
}

/// Concurrent connections per key (token hash, ticket, peer node id). `acquire` hands out a
/// guard that releases the slot when dropped, so a slot can never leak past the connection.
#[derive(Default)]
pub struct ConnCounter {
    counts: Mutex<HashMap<String, usize>>,
}

pub struct ConnSlot {
    counter: Arc<ConnCounter>,
    key: String,
}

impl Drop for ConnSlot {
    fn drop(&mut self) {
        let mut c = self.counter.counts.lock();
        if let Some(n) = c.get_mut(&self.key) {
            *n -= 1;
            if *n == 0 {
                c.remove(&self.key);
            }
        }
    }
}

impl ConnCounter {
    /// `None` when `key` already holds `max` connections (0 = unlimited).
    pub fn acquire(self: &Arc<Self>, key: &str, max: usize) -> Option<ConnSlot> {
        let mut c = self.counts.lock();
        let n = c.entry(key.to_string()).or_insert(0);
        if max > 0 && *n >= max {
            return None;
        }
        *n += 1;
        Some(ConnSlot { counter: self.clone(), key: key.to_string() })
    }

    pub fn count(&self, key: &str) -> usize {
        self.counts.lock().get(key).copied().unwrap_or(0)
    }
}

/// All server-wide limiters, shared by HTTP, WebSocket and p2p handlers.
pub struct Limits {
    /// Failed guest (ticket) authentications per client address: 10 per 10 min.
    pub guest_fails: Mutex<FailureBudget<std::net::IpAddr>>,
    /// Failed user-token authentications per client address: 30 per 10 min (tokens are
    /// 128-bit random, so this only stops log floods and cheap probing).
    pub auth_fails: Mutex<FailureBudget<std::net::IpAddr>>,
    /// Failed p2p hellos per peer node id: 10 per 10 min.
    pub p2p_fails: Mutex<FailureBudget<String>>,
    /// Concurrent connections per token hash / ticket hash / peer node id.
    pub conns: Arc<ConnCounter>,
    pub usage: UsageCache,
}

impl Default for Limits {
    fn default() -> Self {
        let ten_min = Duration::from_secs(600);
        Self {
            guest_fails: Mutex::new(FailureBudget::new(10, ten_min)),
            auth_fails: Mutex::new(FailureBudget::new(30, ten_min)),
            p2p_fails: Mutex::new(FailureBudget::new(10, ten_min)),
            conns: Arc::new(ConnCounter::default()),
            usage: UsageCache::default(),
        }
    }
}

/// Per-user storage figures, recomputed at most once a minute and bumped on every upload in
/// between, so quota checks do not walk the disk on each request.
#[derive(Default)]
pub struct UsageCache {
    entries: Mutex<HashMap<String, (u64, Instant)>>,
}

impl UsageCache {
    /// Current usage for `user_id`, using `compute` when the cached figure is stale.
    pub fn get(&self, user_id: &str, compute: impl FnOnce() -> anyhow::Result<u64>) -> anyhow::Result<u64> {
        if let Some((bytes, at)) = self.entries.lock().get(user_id) {
            if at.elapsed() < Duration::from_secs(60) {
                return Ok(*bytes);
            }
        }
        let bytes = compute()?;
        let mut e = self.entries.lock();
        if e.len() > 10_000 {
            e.clear();
        }
        e.insert(user_id.to_string(), (bytes, Instant::now()));
        Ok(bytes)
    }

    pub fn add(&self, user_id: &str, bytes: u64) {
        if let Some((b, _)) = self.entries.lock().get_mut(user_id) {
            *b += bytes;
        }
    }
}

/// Per-socket budget for expensive requests (a full-state `SyncStep1`): `max` within `window`.
pub struct RequestBudget {
    max: usize,
    window: Duration,
    stamps: Vec<Instant>,
}

impl RequestBudget {
    pub fn new(max: usize, window: Duration) -> Self {
        Self { max, window, stamps: Vec::with_capacity(max) }
    }

    /// True when the request may proceed.
    pub fn allow(&mut self) -> bool {
        let now = Instant::now();
        let w = self.window;
        self.stamps.retain(|t| now.duration_since(*t) < w);
        if self.stamps.len() >= self.max {
            return false;
        }
        self.stamps.push(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_budget_blocks_after_max_within_window() {
        let mut b: FailureBudget<u8> = FailureBudget::new(3, Duration::from_secs(60));
        for _ in 0..3 {
            assert!(!b.blocked(&1));
            b.record_failure(1);
        }
        assert!(b.blocked(&1));
        assert!(!b.blocked(&2));
    }

    #[test]
    fn conn_counter_releases_on_drop() {
        let c = Arc::new(ConnCounter::default());
        let a = c.acquire("k", 2).unwrap();
        let _b = c.acquire("k", 2).unwrap();
        assert!(c.acquire("k", 2).is_none());
        drop(a);
        assert!(c.acquire("k", 2).is_some());
        assert!(c.acquire("other", 0).is_some(), "0 means unlimited");
    }

    #[test]
    fn request_budget() {
        let mut r = RequestBudget::new(2, Duration::from_secs(60));
        assert!(r.allow());
        assert!(r.allow());
        assert!(!r.allow());
    }
}
