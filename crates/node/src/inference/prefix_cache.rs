//! Cache de prefixos para manter sessões de conversa entre turnos.
//!
//! O cache não guarda pesos nem tensors. Ele guarda apenas a identidade da
//! sessão de KV que continua viva em cada peer, a forma textual usada para
//! encontrar o maior prefixo comum e o tamanho de tokens confirmado pela
//! cabeça. A sessão fica ocupada durante uma geração; uma segunda requisição
//! nunca compartilha a mesma sequência por acidente.
//!
//! `PrefixCache` é o cache em memória. `PrefixDiskIndex` (Phase 3) é a camada
//! de disco opcional: com `slot_persist_prefix_cache` ligado, o gateway grava
//! um snapshot de KV por prefixo em `slot_save_path` e, num miss após
//! reinício, restaura-o em vez de refazer o prefill a frio. Ver
//! `docs/subsystems/inference.md` §3.1.1 e `gateway::build_prefix_disk_index`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

const DEFAULT_CAPACITY: usize = 32;

/// TTL padrão de um snapshot automático em disco sem uso, em segundos. Vale
/// para a entrada do índice e, indiretamente, para o `.bin` que ela referencia.
/// `updated_at` é renovado a cada gravação e a cada restauração. Ajustável por
/// `Config.slot_auto_ttl_secs` / `POM_SLOT_AUTO_TTL_SECS`.
pub const DEFAULT_SLOT_AUTO_TTL_SECS: i64 = 7 * 24 * 3600;

/// Orçamento padrão de disco para os snapshots automáticos de prefixo, somado
/// entre todas as fatias. Ajustável por `Config.slot_auto_max_bytes`.
pub const DEFAULT_SLOT_AUTO_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Um turno cujo prefixo retido cresceu menos que isto desde o último snapshot
/// da mesma linhagem não regrava: só renova o carimbo do ancestral. Corta a
/// amplificação de escrita numa conversa com turnos curtos.
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
                // Um miss estreia com identidade própria. Reciclar aqui a
                // identidade de uma sessão ociosa era o que fazia um pedido
                // avulso adotar o `session_id` de uma conversa quente: o
                // prefixo dela ia embora com o prefill novo e o follow-up
                // dela voltava com "rewind recusado: sessão desconhecida".
                // Quem decide se há lugar para uma sessão nova é a admissão
                // do escalonador (`new_session_slots`), não o cache. As
                // entradas ociosas continuam limitadas por LRU no `commit`.
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

    /// Registra uma sessão **restaurada do disco** como um acerto ocupado. O
    /// turno segue como um hit de prefixo normal: a cabeça reaproveita o
    /// prompt retido pela regra só-acrescentar, `commit` publica a entrada
    /// atualizada no fim, e `discard`/`Drop` a soltam se o prefill falhar.
    /// Usado só pela camada de disco (`PrefixDiskIndex`).
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

// ── Camada de disco: prefixos que sobrevivem ao reinício ─────────────────────

/// Uma entrada do índice de snapshots automáticos de prefixo. Aponta para um
/// conjunto de `.bin` (um por hop, `auto__<key_hash>__<node_id>.bin`) que o
/// `gateway::snapshot_deployment` gravou no `slot_save_path` de cada peer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskPrefixEntry {
    /// `PrefixRouteKey::model` (modelo + deployment + chat_template).
    pub route_model: String,
    /// `PrefixRouteKey::hops`.
    pub route_hops: Vec<String>,
    /// Identifica o arquivo: `hash(route + prompt)`, hex. Estável para o mesmo
    /// prefixo de conversa, então um re-snapshot sobrescreve.
    pub key_hash: String,
    /// A forma textual do prefixo (o `cache_key` do gateway), para o mesmo
    /// casamento de maior prefixo que o cache em memória faz.
    pub prompt: String,
    /// Tokens de prompt que o snapshot cobre (`retained_tokens`).
    pub cached_tokens: u32,
    /// Soma dos `.bin` de todos os hops, para o orçamento de disco.
    pub bytes: u64,
    /// Unix secs do último uso (gravação ou restauração).
    pub updated_at: i64,
}

/// Índice local do gateway dos snapshots automáticos de prefixo. Persiste num
/// JSON pequeno ao lado dos `.bin`; some quando o `slot_save_path` não está
/// configurado ou `slot_persist_prefix_cache` está desligado (o `GatewayState`
/// guarda um `Option`). O índice é do gateway: se o papel de gateway mudar de
/// máquina, o novo gateway não enxerga estas entradas e cai no prefill frio.
#[derive(Debug)]
pub struct PrefixDiskIndex {
    path: PathBuf,
    max_bytes: u64,
    ttl_secs: i64,
    state: Mutex<Vec<DiskPrefixEntry>>,
}

impl PrefixDiskIndex {
    /// Lê o índice do disco, poda o que passou do TTL e devolve o handle.
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

    /// O maior prefixo em disco que é prefixo **estrito** do prompt pedido -
    /// igual à regra só-acrescentar de `plan_prefix_reuse`, para não restaurar
    /// um snapshot que a cabeça não conseguiria reaproveitar.
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

    /// Insere ou atualiza a entrada de um `key_hash`, solta os ancestrais que
    /// ele supersede e aplica TTL e orçamento. Devolve os `key_hash` que
    /// saíram, para o chamador apagar os `.bin` correspondentes em cada hop.
    pub fn record(
        &self,
        model: &str,
        hops: &[String],
        key_hash: &str,
        prompt: &str,
        cached_tokens: u32,
        bytes: u64,
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
        });

        let mut dropped = Vec::new();

        // Supersessão: um snapshot mais longo na mesma rota cobre todo prefixo
        // estrito dele, então uma conversa que cresce mantém um só `.bin` em
        // vez de um por turno. Uma conversa que se ramifica (um turno anterior
        // editado) perde o ponto intermediário e cai no ancestral que sobrar
        // ou no prefill frio - aceitável para um cache.
        state.retain(|entry| {
            let superseded = entry.key_hash != key_hash
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

    /// Marca uma entrada como usada agora (uma restauração bem-sucedida).
    pub fn touch(&self, key_hash: &str) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = state.iter_mut().find(|entry| entry.key_hash == key_hash) {
            entry.updated_at = now_secs();
            self.persist(&state);
        }
    }

    /// Esquece uma entrada cujo `.bin` sumiu ou não serviu.
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

    /// `(entradas, bytes)` para telemetria.
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

    /// A corrida do incidente de 2026-09-08: um agente com 62 mil tokens
    /// quentes e, no meio do turno, um pedido avulso de 35 caracteres
    /// (geração de título). O pedido pequeno estreava com a identidade da
    /// sessão grande, e o follow-up dela voltava com "rewind recusado: sessão
    /// desconhecida" e reingeria tudo do zero.
    #[test]
    fn a_small_concurrent_request_does_not_take_over_a_hot_sessions_identity() {
        let cache = PrefixCache::new(4);
        let agent_prompt = "system\nferramentas\nconversa longa";
        cache
            .acquire(key(), agent_prompt, "agent-session".into())
            .commit(agent_prompt.into(), 62_380);

        // Pedido avulso: uma mensagem, zero ferramentas, prompt curto.
        let title = cache.acquire(key(), "resuma isto num título", "title-session".into());
        assert!(!title.is_hit());
        assert_eq!(title.session_id(), "title-session");
        title.discard();

        // O turno seguinte do agente reencontra o próprio prefixo inteiro.
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
        // Ocupada: uma segunda requisição não a compartilha.
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
        // O longo entra primeiro; o curto depois **não** é supersedido (curto
        // não cobre longo), então os dois coexistem para este teste de match.
        idx.record(
            "m",
            &hops,
            "h_turn2",
            "system\nuser1\nassist1\nuser2",
            8,
            1_000,
        );
        idx.record("m", &hops, "h_sys", "system", 2, 1_000);

        // Dois casam; o mais longo vence.
        assert_eq!(
            idx.longest_prefix_match("m", &hops, "system\nuser1\nassist1\nuser2\nuser3")
                .expect("match")
                .key_hash,
            "h_turn2"
        );
        // Um ramo diverge cedo: só o prefixo do sistema casa.
        assert_eq!(
            idx.longest_prefix_match("m", &hops, "system\noutra coisa")
                .expect("falls on the system prefix")
                .key_hash,
            "h_sys"
        );
        // Igualdade não casa (a cabeça não conseguiria reaproveitar).
        assert!(idx.longest_prefix_match("m", &hops, "system").is_none());
        // Rota diferente não casa.
        assert!(idx
            .longest_prefix_match("m", &["outro".to_string()], "system\nuser1\nx")
            .is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_index_supersedes_prefix_ancestors_on_the_same_route() {
        let (dir, idx) = tmp_index("sup", DEFAULT_SLOT_AUTO_MAX_BYTES);
        let r1 = vec!["h".to_string()];
        assert!(idx.record("m", &r1, "a", "sys", 1, 1_000).is_empty());
        // Turno seguinte da mesma linhagem: solta o ancestral "a".
        assert_eq!(idx.record("m", &r1, "b", "sys\nu1", 3, 1_000), vec!["a"]);
        assert_eq!(
            idx.record("m", &r1, "c", "sys\nu1\nu2", 5, 1_000),
            vec!["b"]
        );
        assert_eq!(idx.stats().0, 1, "one lineage, one snapshot");

        // Outra rota não é tocada.
        idx.record("m", &["outro".to_string()], "d", "sys\nu1", 3, 1_000);
        assert_eq!(idx.stats().0, 2);

        // Um ramo (turno anterior editado) diverge de "c" e coexiste com ele.
        assert!(idx
            .record("m", &r1, "e", "sys\nu1\nOUTRO", 5, 1_000)
            .is_empty());
        assert_eq!(idx.stats().0, 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_index_evicts_by_budget_and_reports_dropped_hashes() {
        let (dir, idx) = tmp_index("budget", 2_500);
        let hops = vec!["h".to_string()];
        idx.record("m", &hops, "a", "p-a", 1, 1_000);
        idx.record("m", &hops, "b", "p-b", 1, 1_000);
        // Terceiro empurra o total a 3_000 > 2_500: o mais antigo ("a") sai.
        // ("p-a"/"p-b"/"p-c" não são prefixos entre si, então a supersessão
        // não dispara e o teste isola o orçamento.)
        let dropped = idx.record("m", &hops, "c", "p-c", 1, 1_000);
        assert!(dropped.contains(&"a".to_string()), "dropped = {dropped:?}");
        let (count, bytes) = idx.stats();
        assert_eq!(count, 2);
        assert_eq!(bytes, 2_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_index_ttl_is_configurable_and_prunes_on_record() {
        let (dir, _idx) = tmp_index("ttl", DEFAULT_SLOT_AUTO_MAX_BYTES);
        // TTL de 1 s via load abaixo; aqui usamos um índice novo com ttl curto.
        let short = PrefixDiskIndex::load(dir.join("short.json"), DEFAULT_SLOT_AUTO_MAX_BYTES, 1);
        short.record("m", &["h".to_string()], "old", "p-old", 1, 10);
        // Força o carimbo para o passado e regrava outra entrada: "old" expira.
        {
            let mut st = short.state.lock().unwrap();
            st.iter_mut().for_each(|e| e.updated_at -= 10);
        }
        let dropped = short.record("m", &["h".to_string()], "new", "p-new", 1, 10);
        assert!(
            dropped.contains(&"old".to_string()),
            "dropped = {dropped:?}"
        );
        assert_eq!(short.stats().0, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
