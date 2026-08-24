//! Cache de prefixos para manter sessões de conversa entre turnos.
//!
//! O cache não guarda pesos nem tensors. Ele guarda apenas a identidade da
//! sessão de KV que continua viva em cada peer, a forma textual usada para
//! encontrar o maior prefixo comum e o tamanho de tokens confirmado pela
//! cabeça. A sessão fica ocupada durante uma geração; uma segunda requisição
//! nunca compartilha a mesma sequência por acidente.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

const DEFAULT_CAPACITY: usize = 32;

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

/// Um cache pequeno e global por gateway. A capacidade é aplicada despejando
/// entradas ociosas; leases ocupados nunca são forçados a sair e podem manter
/// o total acima dela até terminarem. O limite é de quantidade, não orçamento
/// de pesos: somente sessões de KV já contabilizadas pelo runtime permanecem.
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

    /// Reserva o maior prefixo textual compatível. Quando não há hit, a
    /// sessão nova ainda sai como lease para ser inserida ao final do turno.
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
                // A cache miss must not consume a second KV slot when the
                // runtime has `max_sessions = 1`. Recycle the least-recently
                // used idle session; the next prefill resets that sequence
                // before decoding the new prompt. Busy entries stay intact.
                let recycled_id = state
                    .entries
                    .iter()
                    .filter(|(_, entry)| !entry.busy)
                    .min_by_key(|(_, entry)| entry.last_used)
                    .map(|(id, _)| id.clone());
                let session_id = recycled_id
                    .and_then(|id| state.entries.remove(&id).map(|entry| entry.session_id))
                    .unwrap_or(new_session_id);
                (None, session_id, String::new(), 0)
            }
        };

        PrefixLease {
            cache: Arc::clone(self),
            entry_id,
            key,
            session_id,
            cached_prompt,
            cached_tokens,
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
    active: bool,
}

impl PrefixLease {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn is_hit(&self) -> bool {
        self.entry_id.is_some()
    }

    /// Publica a sessão depois que o gateway a rewindeu para o fim do prompt.
    /// Consumir o lease impede que `Drop` desmarque uma entrada já atualizada.
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
            "a está em uso e não pode ser despejado"
        );
        held.discard();
        cache.acquire(key(), "d", "s4".into()).commit("d".into(), 1);
        assert_eq!(cache.snapshots().len(), 1);
        assert_eq!(cache.snapshots()[0].prompt, "d");
    }

    #[test]
    fn a_miss_recycles_an_idle_session_instead_of_needing_a_second_slot() {
        let cache = PrefixCache::new(4);
        cache
            .acquire(key(), "system", "old-session".into())
            .commit("system".into(), 2);

        let lease = cache.acquire(key(), "unrelated", "new-session".into());
        assert!(!lease.is_hit());
        assert_eq!(lease.session_id(), "old-session");
        lease.discard();
    }
}
