//! Host-memory KV cache, the counterpart of llama-server's `--cache-ram`.
//!
//! A peer has `max_sessions` live KV sequences. When a retained session loses
//! its sequence - the scheduler reclaims it for another conversation, or the
//! idle collector takes it - its KV used to be wiped, and the conversation's
//! next turn paid a cold prefill. With this cache the serialized KV of that
//! session is parked in RAM instead, keyed by `session_id`, and copied back
//! into a free sequence when the same session returns. The parked entries are
//! the "extra slots" of llama-server: bounded by bytes, evicted least recently
//! used first.
//!
//! The memory is the user's before it is the cache's: half of the host RAM
//! always stays available to whoever uses the machine, and a machine whose
//! half is below 6 GiB never parks anything (`user_ram_reserve`).
//!
//! This file only holds the bookkeeping. Reading and writing the engine state
//! lives in `InferState::park_session` / `unpark_session` (`server.rs`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The user of the machine always keeps at least this much host RAM.
pub(super) const USER_RAM_FLOOR_BYTES: u64 = 6 * 1024 * 1024 * 1024;

/// Host RAM the cache must leave to the user: half of the machine. When that
/// half is below `USER_RAM_FLOOR_BYTES` the machine is too small and nothing
/// is ever parked.
pub(super) fn user_ram_reserve(total_bytes: u64) -> Option<u64> {
    let half = total_bytes / 2;
    (half >= USER_RAM_FLOOR_BYTES).then_some(half)
}

/// What the operator asked for (`Config::cache_ram_mib`, `--cache-ram`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CacheRamSetting {
    Off,
    /// As much as the user reserve allows.
    Auto,
    /// A byte ceiling, still below what the user reserve allows.
    Limit(u64),
}

impl CacheRamSetting {
    /// llama-server values: absent or negative is no explicit limit, zero is
    /// off, a positive number is MiB.
    pub(super) fn from_mib(mib: Option<i64>) -> Self {
        match mib {
            Some(0) => Self::Off,
            Some(mib) if mib > 0 => Self::Limit((mib as u64).saturating_mul(1024 * 1024)),
            _ => Self::Auto,
        }
    }
}

/// Last sample of the host memory, refreshed in the background so the parking
/// path never waits on the operating system. Zero total means "not sampled",
/// and an unsampled host parks nothing.
#[derive(Debug, Default)]
pub(super) struct HostMemory {
    total: AtomicU64,
    available: AtomicU64,
}

impl HostMemory {
    pub(super) fn set(&self, total_bytes: u64, available_bytes: u64) {
        self.total.store(total_bytes, Ordering::Relaxed);
        self.available.store(available_bytes, Ordering::Relaxed);
    }

    fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    fn available(&self) -> u64 {
        self.available.load(Ordering::Relaxed)
    }

    /// Samples now and keeps sampling while someone holds the value. Outside a
    /// tokio runtime there is no sampler and the cache stays inert.
    pub(super) fn spawn_sampler(self: &Arc<Self>) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let host = Arc::downgrade(self);
        runtime.spawn(async move {
            loop {
                let memory = crate::sysmon::sample_memory().await;
                let Some(host) = host.upgrade() else {
                    return;
                };
                let bytes = |mb: f64| (mb.max(0.0) * 1_048_576.0) as u64;
                host.set(bytes(memory.total_mb), bytes(memory.available_mb));
                drop(host);
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
    }
}

/// A retained session whose KV left the context and now lives in host RAM.
pub(super) struct ParkedSession {
    /// `llama_state_seq_get_data` of the session's `prompt_seq`.
    pub(super) data: Vec<u8>,
    /// `Session::prompt_len` at parking time: the retained boundary.
    pub(super) prompt_len: u32,
    /// `Session::stable_len` at parking time.
    pub(super) stable_len: u32,
    /// The retained prompt tokens, when this peer knows them (the head).
    pub(super) prompt_tokens: Vec<i32>,
    last_used: Instant,
}

impl ParkedSession {
    pub(super) fn new(
        data: Vec<u8>,
        prompt_len: u32,
        stable_len: u32,
        prompt_tokens: Vec<i32>,
    ) -> Self {
        Self {
            data,
            prompt_len,
            stable_len,
            prompt_tokens,
            last_used: Instant::now(),
        }
    }

    fn bytes(&self) -> u64 {
        self.data.len() as u64
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct RamCacheStats {
    pub(super) entries: usize,
    pub(super) bytes: u64,
    pub(super) tokens: u64,
    /// Sessions parked since the model loaded.
    pub(super) parked: u64,
    /// Parked sessions copied back into a sequence.
    pub(super) restored: u64,
    /// Parked sessions dropped to make room, or refused for being too large.
    pub(super) evicted: u64,
}

pub(super) struct KvRamCache {
    setting: CacheRamSetting,
    host: Arc<HostMemory>,
    entries: HashMap<String, ParkedSession>,
    bytes: u64,
    parked: u64,
    restored: u64,
    evicted: u64,
}

impl KvRamCache {
    pub(super) fn new(setting: CacheRamSetting, host: Arc<HostMemory>) -> Self {
        Self {
            setting,
            host,
            entries: HashMap::new(),
            bytes: 0,
            parked: 0,
            restored: 0,
            evicted: 0,
        }
    }

    /// Turned on by the operator; whether this host can actually keep
    /// anything is `limit_bytes`.
    pub(super) fn enabled(&self) -> bool {
        self.setting != CacheRamSetting::Off
    }

    /// Host RAM that stays with the user, or `None` when the machine is too
    /// small (or not sampled yet) for the cache to exist.
    pub(super) fn reserve_bytes(&self) -> Option<u64> {
        user_ram_reserve(self.host.total())
    }

    /// The most this cache may hold: never more than what is left after the
    /// user reserve, and never more than the operator's own ceiling.
    pub(super) fn limit_bytes(&self) -> Option<u64> {
        let reserve = self.reserve_bytes()?;
        let ceiling = self.host.total().saturating_sub(reserve);
        match self.setting {
            CacheRamSetting::Off => None,
            CacheRamSetting::Auto => Some(ceiling),
            CacheRamSetting::Limit(limit) => Some(limit.min(ceiling)),
        }
    }

    /// Bytes that must leave the cache before `size` more can be parked, or
    /// `None` when it cannot be parked at all. Two conditions: the cache stays
    /// under its ceiling, and the host keeps the user reserve **available**
    /// after the copy - whatever else is using the machine's memory.
    fn room_needed(&self, size: u64) -> Option<u64> {
        let limit = self.limit_bytes()?;
        let reserve = self.reserve_bytes()?;
        if size > limit {
            return None;
        }
        let over_limit = self.bytes.saturating_add(size).saturating_sub(limit);
        let over_host = size
            .saturating_add(reserve)
            .saturating_sub(self.host.available());
        let needed = over_limit.max(over_host);
        (needed <= self.bytes).then_some(needed)
    }

    /// Whether a state of `bytes` can be kept. Asked before copying the KV
    /// out of the context, so a state that will not fit costs nothing.
    pub(super) fn accepts(&self, bytes: u64) -> bool {
        self.room_needed(bytes).is_some()
    }

    pub(super) fn contains(&self, sid: &str) -> bool {
        self.entries.contains_key(sid)
    }

    /// Keeps `entry` under `sid`, dropping least recently used entries until
    /// both conditions of `room_needed` hold. Returns `false` when they cannot.
    pub(super) fn insert(&mut self, sid: &str, entry: ParkedSession) -> bool {
        self.remove(sid);
        let size = entry.bytes();
        let Some(needed) = self.room_needed(size) else {
            self.evicted += 1;
            return false;
        };
        let mut freed = 0u64;
        while freed < needed {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, parked)| parked.last_used)
                .map(|(sid, _)| sid.clone())
            else {
                break;
            };
            freed += self.remove(&oldest).map_or(0, |old| old.bytes());
            self.evicted += 1;
        }
        self.bytes += size;
        self.entries.insert(sid.to_string(), entry);
        self.parked += 1;
        true
    }

    /// Takes the entry out without counting it as a restore.
    pub(super) fn remove(&mut self, sid: &str) -> Option<ParkedSession> {
        let entry = self.entries.remove(sid)?;
        self.bytes = self.bytes.saturating_sub(entry.bytes());
        Some(entry)
    }

    pub(super) fn note_restored(&mut self) {
        self.restored += 1;
    }

    /// Drops every parked session. Returns `(entries, bytes)` released.
    pub(super) fn clear(&mut self) -> (usize, u64) {
        let released = (self.entries.len(), self.bytes);
        self.entries.clear();
        self.bytes = 0;
        released
    }

    pub(super) fn stats(&self) -> RamCacheStats {
        RamCacheStats {
            entries: self.entries.len(),
            bytes: self.bytes,
            tokens: self
                .entries
                .values()
                .map(|entry| u64::from(entry.prompt_len))
                .sum(),
            parked: self.parked,
            restored: self.restored,
            evicted: self.evicted,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    fn entry(bytes: u64, tokens: u32) -> ParkedSession {
        // The tests only count bytes: a short buffer with a declared length
        // would lie, so sizes here are kept small and the host is scaled down
        // through `cache_on`.
        ParkedSession::new(vec![0u8; bytes as usize], tokens, tokens, Vec::new())
    }

    /// A 64 GiB host with everything but `used` available.
    fn host(used: u64) -> Arc<HostMemory> {
        let host = Arc::new(HostMemory::default());
        host.set(64 * GIB, 64 * GIB - used);
        host
    }

    #[test]
    fn half_of_the_host_stays_with_the_user_and_small_hosts_are_excluded() {
        assert_eq!(user_ram_reserve(64 * GIB), Some(32 * GIB));
        assert_eq!(user_ram_reserve(12 * GIB), Some(6 * GIB));
        assert_eq!(user_ram_reserve(12 * GIB - 2), None);
        assert_eq!(user_ram_reserve(0), None);
    }

    #[test]
    fn the_setting_follows_the_llama_server_values() {
        assert_eq!(CacheRamSetting::from_mib(None), CacheRamSetting::Auto);
        assert_eq!(CacheRamSetting::from_mib(Some(-1)), CacheRamSetting::Auto);
        assert_eq!(CacheRamSetting::from_mib(Some(0)), CacheRamSetting::Off);
        assert_eq!(
            CacheRamSetting::from_mib(Some(8192)),
            CacheRamSetting::Limit(8 * GIB)
        );
    }

    #[test]
    fn the_ceiling_is_what_the_user_reserve_leaves() {
        let auto = KvRamCache::new(CacheRamSetting::Auto, host(0));
        assert_eq!(auto.limit_bytes(), Some(32 * GIB));
        let small = KvRamCache::new(CacheRamSetting::Limit(8 * GIB), host(0));
        assert_eq!(small.limit_bytes(), Some(8 * GIB));
        let greedy = KvRamCache::new(CacheRamSetting::Limit(60 * GIB), host(0));
        assert_eq!(greedy.limit_bytes(), Some(32 * GIB));
        let off = KvRamCache::new(CacheRamSetting::Off, host(0));
        assert!(!off.enabled());
        assert_eq!(off.limit_bytes(), None);
    }

    #[test]
    fn a_host_below_twelve_gigabytes_or_not_sampled_parks_nothing() {
        let small = Arc::new(HostMemory::default());
        small.set(8 * GIB, 8 * GIB);
        for host in [small, Arc::new(HostMemory::default())] {
            let mut cache = KvRamCache::new(CacheRamSetting::Auto, host);
            assert!(cache.enabled());
            assert_eq!(cache.limit_bytes(), None);
            assert!(!cache.accepts(1));
            assert!(!cache.insert("a", entry(1, 1)));
            assert_eq!(cache.stats().entries, 0);
        }
    }

    #[test]
    fn nothing_is_parked_when_it_would_eat_into_the_user_reserve() {
        // 64 GiB host, 32 GiB reserved. With 31 GiB already used by the model,
        // 1 GiB is left above the reserve.
        let host = host(31 * GIB);
        let cache = KvRamCache::new(CacheRamSetting::Auto, host.clone());
        assert!(cache.accepts(GIB));
        assert!(!cache.accepts(GIB + 1));
        // Someone else takes the rest of the memory: nothing fits any more.
        host.set(64 * GIB, 32 * GIB);
        assert!(!cache.accepts(1));
    }

    #[test]
    fn the_budget_evicts_the_least_recently_parked_session() {
        let mut cache = KvRamCache::new(CacheRamSetting::Limit(100), host(0));
        assert!(cache.insert("a", entry(40, 4)));
        std::thread::sleep(Duration::from_millis(2));
        assert!(cache.insert("b", entry(40, 4)));
        std::thread::sleep(Duration::from_millis(2));
        assert!(cache.insert("c", entry(40, 4)));

        assert!(!cache.contains("a"));
        assert!(cache.contains("b") && cache.contains("c"));
        let stats = cache.stats();
        assert_eq!((stats.entries, stats.bytes, stats.tokens), (2, 80, 8));
        assert_eq!((stats.parked, stats.evicted), (3, 1));
    }

    #[test]
    fn host_pressure_evicts_parked_sessions_before_refusing() {
        let host = host(0);
        let mut cache = KvRamCache::new(CacheRamSetting::Limit(1000), host.clone());
        assert!(cache.insert("a", entry(60, 6)));
        // The host now has only 50 bytes above the reserve: parking 80 more
        // needs 30 back, which dropping "a" provides.
        host.set(64 * GIB, 32 * GIB + 50);
        assert!(cache.insert("b", entry(80, 8)));
        assert!(!cache.contains("a") && cache.contains("b"));
        // And 200 cannot be freed by a cache that holds 80.
        assert!(!cache.insert("c", entry(200, 9)));
        assert!(cache.contains("b"));
    }

    #[test]
    fn an_entry_larger_than_the_whole_budget_is_refused_and_keeps_the_rest() {
        let mut cache = KvRamCache::new(CacheRamSetting::Limit(100), host(0));
        assert!(cache.insert("a", entry(60, 6)));
        assert!(!cache.accepts(101));
        assert!(!cache.insert("big", entry(101, 9)));
        assert!(cache.contains("a"));
        assert_eq!(cache.stats().bytes, 60);
    }

    #[test]
    fn parking_the_same_session_again_replaces_its_entry() {
        let mut cache = KvRamCache::new(CacheRamSetting::Limit(100), host(0));
        assert!(cache.insert("a", entry(60, 6)));
        assert!(cache.insert("a", entry(70, 7)));
        let stats = cache.stats();
        assert_eq!((stats.entries, stats.bytes, stats.evicted), (1, 70, 0));
    }

    #[test]
    fn remove_and_clear_give_the_bytes_back() {
        let mut cache = KvRamCache::new(CacheRamSetting::Auto, host(0));
        cache.insert("a", entry(10, 1));
        cache.insert("b", entry(20, 2));
        assert_eq!(cache.remove("a").map(|entry| entry.prompt_len), Some(1));
        assert!(cache.remove("a").is_none());
        assert_eq!(cache.stats().bytes, 20);
        assert_eq!(cache.clear(), (1, 20));
        assert_eq!(cache.stats().bytes, 0);
    }
}
