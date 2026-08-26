//! Harness diferencial do cache de prefixo.
//!
//! A referência é uma lista simples e lenta. A implementação em teste é o
//! cache real. O proxy quebrado altera deliberadamente o tamanho retornado no
//! hit; o autoteste exige que a comparação encontre a divergência, para que
//! uma regressão silenciosa não seja confundida com uma bateria que nunca
//! detecta nada.

#![cfg(test)]

use crate::prefix_cache::{PrefixCache, PrefixRouteKey};

#[derive(Debug, Clone)]
enum Operation {
    Insert {
        key: &'static str,
        prompt: &'static str,
        session: &'static str,
        tokens: u32,
    },
    Lookup {
        key: &'static str,
        prompt: &'static str,
    },
    Release,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Observation {
    key: String,
    prompt: String,
    session: String,
    tokens: u32,
}

#[derive(Debug, Clone)]
struct ReferenceEntry {
    observation: Observation,
    busy: bool,
    last_used: u64,
}

#[derive(Default)]
struct Reference {
    entries: Vec<ReferenceEntry>,
    clock: u64,
}

impl Reference {
    fn apply(&mut self, op: &Operation) -> Option<Observation> {
        self.clock += 1;
        match op {
            Operation::Insert {
                key,
                prompt,
                session,
                tokens,
            } => {
                self.entries.push(ReferenceEntry {
                    observation: Observation {
                        key: (*key).into(),
                        prompt: (*prompt).into(),
                        session: (*session).into(),
                        tokens: *tokens,
                    },
                    busy: false,
                    last_used: self.clock,
                });
                while self.entries.len() > 4 {
                    let Some((index, _)) = self
                        .entries
                        .iter()
                        .enumerate()
                        .filter(|(_, entry)| !entry.busy)
                        .min_by_key(|(_, entry)| entry.last_used)
                    else {
                        break;
                    };
                    self.entries.remove(index);
                }
                None
            }
            Operation::Lookup { key, prompt } => {
                let candidate = self
                    .entries
                    .iter()
                    .enumerate()
                    .filter(|(_, entry)| {
                        !entry.busy
                            && entry.observation.key == *key
                            && prompt.starts_with(&entry.observation.prompt)
                    })
                    .max_by_key(|(_, entry)| entry.observation.prompt.len())
                    .map(|(index, _)| index)?;
                let entry = &mut self.entries[candidate];
                entry.busy = true;
                entry.last_used = self.clock;
                Some(entry.observation.clone())
            }
            Operation::Release => None,
        }
    }

    fn release(&mut self) {
        for entry in &mut self.entries {
            if entry.busy {
                entry.busy = false;
                entry.last_used = self.clock;
            }
        }
    }
}

trait CacheProxy {
    fn insert(&self, key: &str, prompt: &str, session: &str, tokens: u32);
    fn lookup(&self, key: &str, prompt: &str) -> Option<Observation>;
    fn release(&self);
}

struct RealProxy {
    cache: std::sync::Arc<PrefixCache>,
    active: std::sync::Mutex<Vec<crate::prefix_cache::PrefixLease>>,
}

impl RealProxy {
    fn new() -> Self {
        Self {
            cache: PrefixCache::new(4),
            active: std::sync::Mutex::new(Vec::new()),
        }
    }
}

fn key(value: &str) -> PrefixRouteKey {
    PrefixRouteKey::new(value, ["head".to_string()])
}

impl CacheProxy for RealProxy {
    fn insert(&self, key_name: &str, prompt: &str, session: &str, tokens: u32) {
        self.cache
            .acquire(key(key_name), "", session.into())
            .commit(prompt.into(), tokens);
    }

    fn lookup(&self, key_name: &str, prompt: &str) -> Option<Observation> {
        let lease = self
            .cache
            .acquire(key(key_name), prompt, "new-session".into());
        if !lease.is_hit() {
            lease.discard();
            return None;
        }
        let observation = Observation {
            key: key_name.into(),
            prompt: lease.cached_prompt.clone(),
            session: lease.session_id().into(),
            tokens: lease.cached_tokens,
        };
        self.active.lock().unwrap().push(lease);
        Some(observation)
    }

    fn release(&self) {
        let mut active = self.active.lock().unwrap();
        let leases = std::mem::take(&mut *active);
        drop(active);
        for lease in leases {
            lease.discard();
        }
    }
}

/// Um adapter que simula uma regressão silenciosa no valor de `cached_tokens`.
/// Ele continua usando o cache real para que a bateria teste o mesmo contrato.
struct BrokenProxy {
    inner: RealProxy,
}

impl CacheProxy for BrokenProxy {
    fn insert(&self, key: &str, prompt: &str, session: &str, tokens: u32) {
        self.inner.insert(key, prompt, session, tokens);
    }

    fn lookup(&self, key: &str, prompt: &str) -> Option<Observation> {
        self.inner.lookup(key, prompt).map(|mut observed| {
            observed.tokens += 1;
            observed
        })
    }

    fn release(&self) {
        self.inner.release();
    }
}

fn run_battery<P: CacheProxy>(proxy: &P, operations: &[Operation]) -> Vec<String> {
    let mut reference = Reference::default();
    let mut mismatches = Vec::new();
    for op in operations {
        let expected = reference.apply(op);
        let actual = match op {
            Operation::Insert {
                key,
                prompt,
                session,
                tokens,
            } => {
                proxy.insert(key, prompt, session, *tokens);
                None
            }
            Operation::Lookup { key, prompt } => proxy.lookup(key, prompt),
            Operation::Release => {
                proxy.release();
                None
            }
        };
        if expected != actual {
            mismatches.push(format!("{op:?}: expected {expected:?}, got {actual:?}"));
        }
        if matches!(op, Operation::Release) {
            reference.release();
        }
    }
    mismatches
}

#[test]
fn differential_battery_detects_a_broken_cache_proxy() {
    let operations = [
        Operation::Insert {
            key: "model-a",
            prompt: "system",
            session: "s1",
            tokens: 2,
        },
        Operation::Lookup {
            key: "model-a",
            prompt: "system\nfirst-turn",
        },
        Operation::Insert {
            key: "model-a",
            prompt: "system\nuser",
            session: "s2",
            tokens: 4,
        },
        Operation::Lookup {
            key: "model-a",
            prompt: "system\nuser\nnext",
        },
        Operation::Lookup {
            key: "model-a",
            prompt: "system\nuser\nblocked",
        },
        Operation::Release,
        Operation::Lookup {
            key: "model-b",
            prompt: "system\nuser\nother-model",
        },
        Operation::Lookup {
            key: "model-a",
            prompt: "system\nuser\nnext",
        },
        Operation::Release,
    ];
    let real_mismatches = run_battery(&RealProxy::new(), &operations);
    assert!(
        real_mismatches.is_empty(),
        "referência e cache real divergiram: {real_mismatches:?}"
    );

    let broken_mismatches = run_battery(
        &BrokenProxy {
            inner: RealProxy::new(),
        },
        &operations,
    );
    assert!(
        !broken_mismatches.is_empty(),
        "o proxy quebrado passou sem disparar a bateria diferencial"
    );
}
