//! Prefix cache to keep conversation sessions alive between turns.
//!
//! The cache holds no weights or tensors. It holds only the identity of the
//! KV session that stays alive on each peer, the textual form used to
//! find the longest common prefix and the token count confirmed by the
//! head. A session is busy during a generation; a second request
//! never shares the same sequence by accident.
//!
//! `PrefixCache` is the in-memory cache. `PrefixDiskIndex` (Phase 3) is the optional
//! disk layer: with `slot_persist_prefix_cache` on, the gateway writes
//! one KV snapshot per prefix to `slot_save_path` and, on a miss after a
//! restart, restores it instead of redoing the prefill cold. See
//! `docs/subsystems/inference.md` §3.1.1 and `gateway::build_prefix_disk_index`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

const DEFAULT_CAPACITY: usize = 32;

/// Default TTL of an unused automatic disk snapshot, in seconds. It applies
/// to the index entry and, indirectly, to the `.bin` it references.
/// `updated_at` is renewed on every write and every restore. Adjustable by
/// `Config.slot_auto_ttl_secs` / `POM_SLOT_AUTO_TTL_SECS`.
pub const DEFAULT_SLOT_AUTO_TTL_SECS: i64 = 7 * 24 * 3600;

/// Default disk budget for the automatic prefix snapshots, summed
/// across all slices. Adjustable by `Config.slot_auto_max_bytes`.
pub const DEFAULT_SLOT_AUTO_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// A turn whose retained prefix grew by less than this since the last snapshot
/// of the same lineage does not rewrite: it only renews the ancestor's stamp. It cuts
/// write amplification in a conversation with short turns.
pub const SLOT_AUTO_MIN_GROWTH_TOKENS: u32 = 256;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PrefixRouteKey {
    pub model: String,
    pub hops: Vec<String>,
}

impl PrefixRouteKey {
    pub fn new(model: impl Into<String>, hops: impl IntoIterator<Item = String>) -> Self {
        Self {
            model: model.into(),
            hops: hops.into_iter().collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefixSnapshot {
    pub key: PrefixRouteKey,
    pub prompt: String,
    pub session_id: String,
    pub cached_tokens: u32,
    pub busy: bool,
}

#[derive(Debug)]
struct Entry {
    key: PrefixRouteKey,
    prompt: String,
    session_id: String,
    cached_tokens: u32,
    last_used: Instant,
    busy: bool,
}

#[derive(Debug)]
struct State {
    entries: HashMap<String, Entry>,
}

/// A small, global cache per gateway. Capacity is enforced by evicting
/// idle entries; busy leases are never forced out and may keep
/// the total above it until they finish. The limit is a count, not a weight
/// budget: only KV sessions already accounted for by the runtime remain.
#[derive(Debug)]
pub struct PrefixCache {
    capacity: usize,
    state: Mutex<State>,
}

impl PrefixCache {
    pub fn default_arc() -> Arc<Self> {
        Self::new(DEFAULT_CAPACITY)
    }

    pub fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            capacity: capacity.max(1),
            state: Mutex::new(State {
                entries: HashMap::new(),
            }),
        })
    }

    /// Reserves the longest compatible textual prefix. When there is no hit, the
    /// new session still comes out as a lease to be inserted at the end of the turn.
    pub fn acquire(
        self: &Arc<Self>,
        key: PrefixRouteKey,
        prompt: &str,
        new_session_id: String,
    ) -> PrefixLease {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let matched = state
            .entries
            .iter()
            .filter(|(_, entry)| {
                !entry.busy && entry.key == key && prompt.starts_with(&entry.prompt)
            })
            .max_by_key(|(_, entry)| entry.prompt.len())
            .map(|(id, entry)| (id.clone(), entry.prompt.clone(), entry.cached_tokens));
        let miss_reason =
            matched.is_none().then(|| {
                if state.entries.is_empty() {
                    "cache-empty"
                } else if state.entries.values().any(|entry| {
                    entry.busy && entry.key == key && prompt.starts_with(&entry.prompt)
                }) {
                    "matching-session-busy"
                } else if state.entries.values().any(|entry| entry.key == key) {
                    "prompt-prefix-changed"
                } else {
                    "route-changed"
                }
            });

        let (entry_id, session_id, cached_prompt, cached_tokens) = match matched {
            Some((entry_id, cached_prompt, cached_tokens)) => {
                let entry = state.entries.get_mut(&entry_id).expect("hit exists");
                entry.busy = true;
                entry.last_used = Instant::now();
                (
                    Some(entry_id),
                    entry.session_id.clone(),
                    cached_prompt,
                    cached_tokens,
                )
            }
            None => {
                // A miss debuts with its own identity. Recycling here the
                // identity of an idle session was what made a one-off request
                // adopt the `session_id` of a hot conversation: its
                // prefix went away with the new prefill and its follow-up
                // came back with "rewind refused: unknown session".
                // Who decides whether there is room for a new session is the admission
                // of the scheduler (`new_session_slots`), not the cache. Idle
                // entries stay bounded by LRU in `commit`.
                (None, new_session_id, String::new(), 0)
            }
        };

        PrefixLease {
            cache: Arc::clone(self),
            entry_id,
            key,
            session_id,
            cached_prompt,
            cached_tokens,
            miss_reason,
            active: true,
        }
    }

    /// Registers a session **restored from disk** as a busy hit. The
    /// turn proceeds as a normal prefix hit: the head reuses the
    /// retained prompt by the append-only rule, `commit` publishes the updated
    /// entry at the end, and `discard`/`Drop` release it if the prefill fails.
    /// Used only by the disk layer (`PrefixDiskIndex`).
    pub fn adopt(
        self: &Arc<Self>,
        key: PrefixRouteKey,
        prompt: &str,
        session_id: String,
        cached_tokens: u32,
    ) -> PrefixLease {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.entries.insert(
            session_id.clone(),
            Entry {
                key: key.clone(),
                prompt: prompt.to_string(),
                session_id: session_id.clone(),
                cached_tokens,
                last_used: Instant::now(),
                busy: true,
            },
        );
        PrefixLease {
            cache: Arc::clone(self),
            entry_id: Some(session_id.clone()),
            key,
            session_id,
            cached_prompt: prompt.to_string(),
            cached_tokens,
            miss_reason: None,
            active: true,
        }
    }

    fn commit(
        &self,
        entry_id: Option<String>,
        key: PrefixRouteKey,
        session_id: String,
        prompt: String,
        cached_tokens: u32,
    ) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let id = entry_id.unwrap_or_else(|| session_id.clone());
        let inserted_id = id.clone();
        state.entries.insert(
            id,
            Entry {
                key,
                prompt,
                session_id,
                cached_tokens,
                last_used: Instant::now(),
                busy: false,
            },
        );
        while state.entries.len() > self.capacity {
            let Some(evict) = state
                .entries
                .iter()
                .filter(|(candidate_id, entry)| {
                    candidate_id.as_str() != inserted_id.as_str() && !entry.busy
                })
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            state.entries.remove(&evict);
        }
    }

    fn release(&self, entry_id: &str) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = state.entries.get_mut(entry_id) {
            entry.busy = false;
            entry.last_used = Instant::now();
        }
    }

    pub fn snapshots(&self) -> Vec<PrefixSnapshot> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut snapshots = state
            .entries
            .values()
            .map(|entry| PrefixSnapshot {
                key: entry.key.clone(),
                prompt: entry.prompt.clone(),
                session_id: entry.session_id.clone(),
                cached_tokens: entry.cached_tokens,
                busy: entry.busy,
            })
            .collect::<Vec<_>>();
        snapshots.sort_by(|a, b| a.prompt.cmp(&b.prompt));
        snapshots
    }
}

pub struct PrefixLease {
    cache: Arc<PrefixCache>,
    entry_id: Option<String>,
    key: PrefixRouteKey,
    session_id: String,
    pub cached_prompt: String,
    pub cached_tokens: u32,
    miss_reason: Option<&'static str>,
    active: bool,
}

impl PrefixLease {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn is_hit(&self) -> bool {
        self.entry_id.is_some()
    }

    pub fn miss_reason(&self) -> Option<&'static str> {
        self.miss_reason
    }

    /// Publishes the session after the gateway rewound it to the end of the prompt.
    /// Consuming the lease prevents `Drop` from unmarking an already updated entry.
    pub fn commit(mut self, prompt: String, cached_tokens: u32) {
        self.cache.commit(
            self.entry_id.take(),
            self.key.clone(),
            self.session_id.clone(),
            prompt,
            cached_tokens,
        );
        self.active = false;
    }

    pub fn discard(mut self) {
        self.active = false;
        if let Some(entry_id) = self.entry_id.take() {
            self.cache.release(&entry_id);
        }
    }
}

impl Drop for PrefixLease {
    fn drop(&mut self) {
        if self.active {
            if let Some(entry_id) = &self.entry_id {
                self.cache.release(entry_id);
            }
        }
    }
}

// ── Disk layer: prefixes that survive a restart ──────────────────────────────

/// An entry of the index of automatic prefix snapshots. It points to a
/// set of `.bin` files (one per hop, `auto__<key_hash>__<node_id>.bin`) that
/// `gateway::snapshot_deployment` wrote to each peer's `slot_save_path`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskPrefixEntry {
    /// `PrefixRouteKey::model` (model + deployment + chat_template).
    pub route_model: String,
    /// `PrefixRouteKey::hops`.
    pub route_hops: Vec<String>,
    /// Identifies the file: `hash(route + prompt)`, hex. Stable for the same
    /// conversation prefix, so a re-snapshot overwrites.
    pub key_hash: String,
    /// The textual form of the prefix (the gateway's `cache_key`), for the same
    /// longest-prefix matching the in-memory cache does.
    pub prompt: String,
    /// Prompt tokens the snapshot covers (`retained_tokens`).
    pub cached_tokens: u32,
    /// Sum of the `.bin` files of all hops, for the disk budget.
    pub bytes: u64,
    /// Unix secs of the last use (write or restore).
    pub updated_at: i64,
    /// A system part many conversations open with (`gateway::SharedPrefix`):
    /// a conversation that grows from it never supersedes it. TTL and budget
    /// still apply.
    #[serde(default)]
    pub pinned: bool,
}

/// The gateway's local index of the automatic prefix snapshots. It persists in a
/// small JSON next to the `.bin` files; it is absent when `slot_save_path` is not
/// configured or `slot_persist_prefix_cache` is off (`GatewayState`
/// holds an `Option`). The index belongs to the gateway: if the gateway role moves
/// to another machine, the new gateway does not see these entries and falls back to cold prefill.
#[derive(Debug)]
pub struct PrefixDiskIndex {
    path: PathBuf,
    max_bytes: u64,
    ttl_secs: i64,
    state: Mutex<Vec<DiskPrefixEntry>>,
}

impl PrefixDiskIndex {
    /// Reads the index from disk, prunes what is past the TTL and returns the handle.
    pub fn load(path: impl Into<PathBuf>, max_bytes: u64, ttl_secs: i64) -> Arc<Self> {
        let path = path.into();
        let ttl_secs = ttl_secs.max(1);
        let mut entries: Vec<DiskPrefixEntry> = std::fs::read(&path)
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default();
        let cutoff = now_secs() - ttl_secs;
        entries.retain(|entry| entry.updated_at >= cutoff);
        Arc::new(Self {
            path,
            max_bytes: max_bytes.max(1),
            ttl_secs,
            state: Mutex::new(entries),
        })
    }

    /// The longest prefix on disk that is a **strict** prefix of the requested prompt -
    /// same as the append-only rule of `plan_prefix_reuse`, so as not to restore
    /// a snapshot the head could not reuse.
    pub fn longest_prefix_match(
        &self,
        model: &str,
        hops: &[String],
        prompt: &str,
    ) -> Option<DiskPrefixEntry> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .iter()
            .filter(|entry| {
                entry.route_model == model
                    && entry.route_hops == hops
                    && prompt.len() > entry.prompt.len()
                    && prompt.starts_with(&entry.prompt)
            })
            .max_by_key(|entry| entry.prompt.len())
            .cloned()
    }

    /// Inserts or updates the entry for a `key_hash`, drops the ancestors
    /// it supersedes and applies TTL and budget. Returns the `key_hash` values that
    /// left, so the caller can delete the matching `.bin` files on each hop.
    pub fn record(
        &self,
        model: &str,
        hops: &[String],
        key_hash: &str,
        prompt: &str,
        cached_tokens: u32,
        bytes: u64,
        pinned: bool,
    ) -> Vec<String> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let now = now_secs();
        state.retain(|entry| entry.key_hash != key_hash);
        state.push(DiskPrefixEntry {
            route_model: model.to_string(),
            route_hops: hops.to_vec(),
            key_hash: key_hash.to_string(),
            prompt: prompt.to_string(),
            cached_tokens,
            bytes,
            updated_at: now,
            pinned,
        });

        let mut dropped = Vec::new();

        // Supersession: a longer snapshot on the same route covers every strict
        // prefix of it, so a growing conversation keeps a single `.bin`
        // instead of one per turn. A conversation that branches (an earlier turn
        // edited) loses the intermediate point and falls back to whatever ancestor remains
        // or to cold prefill - acceptable for a cache.
        state.retain(|entry| {
            let superseded = entry.key_hash != key_hash
                && !entry.pinned
                && entry.route_model == model
                && entry.route_hops == hops
                && prompt.len() > entry.prompt.len()
                && prompt.starts_with(&entry.prompt);
            if superseded {
                dropped.push(entry.key_hash.clone());
            }
            !superseded
        });

        let cutoff = now - self.ttl_secs;
        state.retain(|entry| {
            let keep = entry.updated_at >= cutoff || entry.key_hash == key_hash;
            if !keep {
                dropped.push(entry.key_hash.clone());
            }
            keep
        });
        while state.iter().map(|entry| entry.bytes).sum::<u64>() > self.max_bytes && state.len() > 1
        {
            if let Some(idx) = state
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.key_hash != key_hash)
                .min_by_key(|(_, entry)| entry.updated_at)
                .map(|(idx, _)| idx)
            {
                dropped.push(state.remove(idx).key_hash);
            } else {
                break;
            }
        }
        self.persist(&state);
        dropped
    }

    /// Marks an entry as used now (a successful restore).
    pub fn touch(&self, key_hash: &str) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = state.iter_mut().find(|entry| entry.key_hash == key_hash) {
            entry.updated_at = now_secs();
            self.persist(&state);
        }
    }

    /// Forgets an entry whose `.bin` vanished or was not usable.
    pub fn forget(&self, key_hash: &str) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let before = state.len();
        state.retain(|entry| entry.key_hash != key_hash);
        if state.len() != before {
            self.persist(&state);
        }
    }

    /// Forgets every entry: the operator emptied this machine's snapshot
    /// directory. Returns how many were dropped.
    pub fn clear(&self) -> usize {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let dropped = state.len();
        state.clear();
        self.persist(&state);
        dropped
    }

    /// `(entries, bytes)` for telemetry.
    pub fn stats(&self) -> (usize, u64) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (state.len(), state.iter().map(|entry| entry.bytes).sum())
    }

    fn persist(&self, entries: &[DiskPrefixEntry]) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_vec_pretty(entries) {
            let _ = std::fs::write(&self.path, json);
        }
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> PrefixRouteKey {
        PrefixRouteKey::new("model", ["head".to_string(), "tail".to_string()])
    }

    #[test]
    fn acquire_returns_the_longest_free_prefix_and_commit_reuses_it() {
        let cache = PrefixCache::new(4);
        cache
            .acquire(key(), "system", "s1".into())
            .commit("system".into(), 2);
        let held = cache.acquire(key(), "system", "active".into());
        cache
            .acquire(key(), "system\nuser", "s2".into())
            .commit("system\nuser".into(), 4);
        held.discard();

        let lease = cache.acquire(key(), "system\nuser\nnext", "s3".into());
        assert!(lease.is_hit());
        assert_eq!(lease.session_id(), "s2");
        assert_eq!(lease.cached_prompt, "system\nuser");
        assert_eq!(lease.cached_tokens, 4);
        lease.discard();
    }

    #[test]
    fn busy_entries_are_not_shared_and_lru_evicts_only_idle_entries() {
        let cache = PrefixCache::new(1);
        let lease = cache.acquire(key(), "a", "s1".into());
        lease.commit("a".into(), 1);
        let held = cache.acquire(key(), "a-b", "s2".into());
        assert!(held.is_hit());

        cache.acquire(key(), "c", "s3".into()).commit("c".into(), 1);
        assert_eq!(
            cache.snapshots().len(),
            2,
            "a is in use and cannot be evicted"
        );
        held.discard();
        cache.acquire(key(), "d", "s4".into()).commit("d".into(), 1);
        assert_eq!(cache.snapshots().len(), 1);
        assert_eq!(cache.snapshots()[0].prompt, "d");
    }

    #[test]
    fn a_miss_keeps_the_retained_session_available_for_a_later_hit() {
        let cache = PrefixCache::new(4);
        cache
            .acquire(key(), "system", "old-session".into())
            .commit("system".into(), 2);

        let miss = cache.acquire(key(), "unrelated", "new-session".into());
        assert!(!miss.is_hit());
        assert_eq!(miss.session_id(), "new-session");
        miss.discard();

        let hit = cache.acquire(key(), "system\nnext", "follow-up".into());
        assert!(hit.is_hit());
        assert_eq!(hit.session_id(), "old-session");
        hit.discard();
    }

    #[test]
    fn miss_reports_whether_the_cache_is_empty_route_changed_or_prompt_diverged() {
        let cache = PrefixCache::new(4);
        let empty = cache.acquire(key(), "system", "empty".into());
        assert_eq!(empty.miss_reason(), Some("cache-empty"));
        empty.commit("system".into(), 2);

        let changed = cache.acquire(key(), "different", "changed".into());
        assert_eq!(changed.miss_reason(), Some("prompt-prefix-changed"));
        changed.discard();

        let other_route = PrefixRouteKey::new("model", ["other-head".to_string()]);
        let route = cache.acquire(other_route, "system", "route".into());
        assert_eq!(route.miss_reason(), Some("route-changed"));
    }

    #[test]
    fn miss_reports_when_another_request_holds_the_matching_session() {
        let cache = PrefixCache::new(4);
        cache
            .acquire(key(), "system", "s1".into())
            .commit("system".into(), 2);
        let active = cache.acquire(key(), "system\nnext", "s2".into());
        let concurrent = cache.acquire(key(), "system\nnext2", "s3".into());
        assert_eq!(concurrent.miss_reason(), Some("matching-session-busy"));
        concurrent.discard();
        active.discard();
    }

    /// The race from the incident of 2026-09-08: an agent with 62 thousand tokens
    /// hot and, in the middle of the turn, a one-off request of 35 characters
    /// (title generation). The small request debuted with the identity of the
    /// large session, and its follow-up came back with "rewind refused: session
    /// unknown" and re-ingested everything from scratch.
    #[test]
    fn a_small_concurrent_request_does_not_take_over_a_hot_sessions_identity() {
        let cache = PrefixCache::new(4);
        let agent_prompt = "system\nferramentas\nconversa longa";
        cache
            .acquire(key(), agent_prompt, "agent-session".into())
            .commit(agent_prompt.into(), 62_380);

        // One-off request: one message, zero tools, short prompt.
        let title = cache.acquire(key(), "resuma isto num título", "title-session".into());
        assert!(!title.is_hit());
        assert_eq!(title.session_id(), "title-session");
        title.discard();

        // The agent's next turn finds its own whole prefix again.
        let follow_up = cache.acquire(
            key(),
            &format!("{agent_prompt}\nmais uma pergunta"),
            "outra".into(),
        );
        assert!(follow_up.is_hit());
        assert_eq!(follow_up.session_id(), "agent-session");
        assert_eq!(follow_up.cached_tokens, 62_380);
        follow_up.discard();
    }

    #[test]
    fn adopt_registers_a_restored_session_as_a_busy_hit() {
        let cache = PrefixCache::new(4);
        let lease = cache.adopt(key(), "system", "disk-session".into(), 3);
        assert!(lease.is_hit());
        assert_eq!(lease.session_id(), "disk-session");
        // Busy: a second request does not share it.
        let other = cache.acquire(key(), "system\nmais", "outra".into());
        assert!(!other.is_hit());
        other.discard();
        lease.commit("system\nmais".into(), 5);
        let hit = cache.acquire(key(), "system\nmais\nfim", "s".into());
        assert!(hit.is_hit());
        assert_eq!(hit.session_id(), "disk-session");
        hit.discard();
    }

    fn tmp_index(tag: &str, max_bytes: u64) -> (std::path::PathBuf, Arc<PrefixDiskIndex>) {
        let dir = std::env::temp_dir().join(format!("pom-prefix-idx-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let idx = PrefixDiskIndex::load(
            dir.join("index.json"),
            max_bytes,
            DEFAULT_SLOT_AUTO_TTL_SECS,
        );
        (dir, idx)
    }

    #[test]
    fn disk_index_matches_the_longest_strict_prefix_only() {
        let (dir, idx) = tmp_index("match", DEFAULT_SLOT_AUTO_MAX_BYTES);
        let hops = vec!["h1".to_string(), "h2".to_string()];
        // The long one goes in first; the short one afterwards is **not** superseded (short
        // does not cover long), so both coexist for this match test.
        idx.record(
            "m",
            &hops,
            "h_turn2",
            "system\nuser1\nassist1\nuser2",
            8,
            1_000,
            false,
        );
        idx.record("m", &hops, "h_sys", "system", 2, 1_000, false);

        // Two match; the longest wins.
        assert_eq!(
            idx.longest_prefix_match("m", &hops, "system\nuser1\nassist1\nuser2\nuser3")
                .expect("match")
                .key_hash,
            "h_turn2"
        );
        // A branch diverges early: only the system prefix matches.
        assert_eq!(
            idx.longest_prefix_match("m", &hops, "system\noutra coisa")
                .expect("falls on the system prefix")
                .key_hash,
            "h_sys"
        );
        // Equality does not match (the head could not reuse it).
        assert!(idx.longest_prefix_match("m", &hops, "system").is_none());
        // A different route does not match.
        assert!(idx
            .longest_prefix_match("m", &["outro".to_string()], "system\nuser1\nx")
            .is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_index_supersedes_prefix_ancestors_on_the_same_route() {
        let (dir, idx) = tmp_index("sup", DEFAULT_SLOT_AUTO_MAX_BYTES);
        let r1 = vec!["h".to_string()];
        assert!(idx.record("m", &r1, "a", "sys", 1, 1_000, false).is_empty());
        // Next turn of the same lineage: drops the ancestor "a".
        assert_eq!(
            idx.record("m", &r1, "b", "sys\nu1", 3, 1_000, false),
            vec!["a"]
        );
        assert_eq!(
            idx.record("m", &r1, "c", "sys\nu1\nu2", 5, 1_000, false),
            vec!["b"]
        );
        assert_eq!(idx.stats().0, 1, "one lineage, one snapshot");

        // Another route is not touched.
        idx.record("m", &["outro".to_string()], "d", "sys\nu1", 3, 1_000, false);
        assert_eq!(idx.stats().0, 2);

        // A branch (earlier turn edited) diverges from "c" and coexists with it.
        assert!(idx
            .record("m", &r1, "e", "sys\nu1\nOUTRO", 5, 1_000, false)
            .is_empty());
        assert_eq!(idx.stats().0, 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pinned_system_part_outlives_the_conversations_that_grow_from_it() {
        let (dir, idx) = tmp_index("pinned", DEFAULT_SLOT_AUTO_MAX_BYTES);
        let r1 = vec!["h".to_string()];
        assert!(idx
            .record("m", &r1, "root", "sys", 1, 1_000, true)
            .is_empty());
        // A conversation that opens with it does not supersede it...
        assert!(idx
            .record("m", &r1, "a", "sys\nu1", 3, 1_000, false)
            .is_empty());
        // ...and its own next turn supersedes only its own ancestor.
        assert_eq!(
            idx.record("m", &r1, "a2", "sys\nu1\nu2", 5, 1_000, false),
            vec!["a"]
        );
        // So a new conversation still starts from the system part.
        assert_eq!(
            idx.longest_prefix_match("m", &r1, "sys\nother")
                .expect("the pinned root")
                .key_hash,
            "root"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_index_evicts_by_budget_and_reports_dropped_hashes() {
        let (dir, idx) = tmp_index("budget", 2_500);
        let hops = vec!["h".to_string()];
        idx.record("m", &hops, "a", "p-a", 1, 1_000, false);
        idx.record("m", &hops, "b", "p-b", 1, 1_000, false);
        // The third pushes the total to 3_000 > 2_500: the oldest ("a") leaves.
        // ("p-a"/"p-b"/"p-c" are not prefixes of each other, so supersession
        // does not fire and the test isolates the budget.)
        let dropped = idx.record("m", &hops, "c", "p-c", 1, 1_000, false);
        assert!(dropped.contains(&"a".to_string()), "dropped = {dropped:?}");
        let (count, bytes) = idx.stats();
        assert_eq!(count, 2);
        assert_eq!(bytes, 2_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_index_ttl_is_configurable_and_prunes_on_record() {
        let (dir, _idx) = tmp_index("ttl", DEFAULT_SLOT_AUTO_MAX_BYTES);
        // TTL of 1 s via load below; here we use a new index with a short ttl.
        let short = PrefixDiskIndex::load(dir.join("short.json"), DEFAULT_SLOT_AUTO_MAX_BYTES, 1);
        short.record("m", &["h".to_string()], "old", "p-old", 1, 10, false);
        // Forces the stamp into the past and rewrites another entry: "old" expires.
        {
            let mut st = short.state.lock().unwrap();
            st.iter_mut().for_each(|e| e.updated_at -= 10);
        }
        let dropped = short.record("m", &["h".to_string()], "new", "p-new", 1, 10, false);
        assert!(
            dropped.contains(&"old".to_string()),
            "dropped = {dropped:?}"
        );
        assert_eq!(short.stats().0, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
