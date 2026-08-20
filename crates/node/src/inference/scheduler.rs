use std::{
    collections::{HashSet, VecDeque},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkClass {
    PrefillTokens,
    PrefillHidden,
    DecodeTokens,
    DecodeHidden,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulerConfig {
    pub enabled: bool,
    pub max_queue_requests: usize,
    pub batch_window: Duration,
    pub max_batch_sequences: usize,
    pub max_batch_tokens: usize,
}

impl SchedulerConfig {
    #[cfg(test)]
    pub fn test(max_queue_requests: usize, max_batch_sequences: usize) -> Self {
        Self {
            enabled: true,
            max_queue_requests,
            batch_window: Duration::ZERO,
            max_batch_sequences,
            max_batch_tokens: usize::MAX,
        }
    }

    pub fn from_values(
        enabled: bool,
        max_queue_requests: u32,
        batch_window_ms: u32,
        max_batch_sequences: u32,
        max_batch_tokens: u32,
        max_sessions: u32,
        n_batch: u32,
    ) -> Self {
        Self {
            enabled,
            max_queue_requests: max_queue_requests.max(1) as usize,
            batch_window: Duration::from_millis(batch_window_ms as u64),
            max_batch_sequences: max_batch_sequences.max(1).min(max_sessions.max(1)) as usize,
            max_batch_tokens: max_batch_tokens.max(1).min(n_batch.max(1)) as usize,
        }
    }

    pub fn from_node_config(cfg: &crate::runtime::NodeConfig) -> Self {
        Self::from_values(
            cfg.batching_enabled,
            cfg.max_queue_requests,
            cfg.batch_window_ms,
            cfg.max_batch_sequences,
            cfg.max_batch_tokens,
            cfg.max_sessions,
            cfg.n_batch,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueItem<T> {
    pub session_id: String,
    pub class: WorkClass,
    pub tokens: usize,
    pub enqueued_at: Instant,
    pub payload: T,
}

impl QueueItem<()> {
    #[cfg(test)]
    fn test(session_id: &str, class: WorkClass, tokens: usize) -> Self {
        Self {
            session_id: session_id.into(),
            class,
            tokens,
            enqueued_at: Instant::now(),
            payload: (),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchSelection<T> {
    pub items: Vec<QueueItem<T>>,
    pub total_tokens: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueFull {
    pub limit: usize,
}

#[derive(Debug)]
pub struct SchedulerQueue<T> {
    config: SchedulerConfig,
    pending: VecDeque<QueueItem<T>>,
}

impl<T> SchedulerQueue<T> {
    pub fn new(config: SchedulerConfig) -> Self {
        Self {
            config,
            pending: VecDeque::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn front_class(&self) -> Option<WorkClass> {
        self.pending.front().map(|item| item.class)
    }

    pub fn try_push(&mut self, item: QueueItem<T>) -> Result<(), QueueFull> {
        if self.pending.len() >= self.config.max_queue_requests {
            return Err(QueueFull {
                limit: self.config.max_queue_requests,
            });
        }
        self.pending.push_back(item);
        Ok(())
    }

    pub fn take_batch(&mut self, _now: Instant, class: WorkClass) -> BatchSelection<T> {
        let mut selected = Vec::new();
        let mut seen_sessions = HashSet::new();
        let mut total_tokens = 0usize;
        let pending_count = self.pending.len();

        for _ in 0..pending_count {
            let Some(item) = self.pending.pop_front() else {
                break;
            };
            if item.class != class
                || seen_sessions.contains(item.session_id.as_str())
                || selected.len() >= self.config.max_batch_sequences
                || (total_tokens.saturating_add(item.tokens) > self.config.max_batch_tokens
                    && !selected.is_empty())
            {
                self.pending.push_back(item);
                continue;
            }

            total_tokens = total_tokens.saturating_add(item.tokens);
            seen_sessions.insert(item.session_id.clone());
            selected.push(item);
        }

        BatchSelection {
            items: selected,
            total_tokens,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn replicas_da_mesma_fatia_nao_compartilham_capacidade_da_fila() {
        let mut a = SchedulerQueue::new(SchedulerConfig::test(1, 1));
        let mut b = SchedulerQueue::new(SchedulerConfig::test(1, 1));
        a.try_push(QueueItem::test("sess-a", WorkClass::DecodeTokens, 1))
            .unwrap();
        b.try_push(QueueItem::test("sess-b", WorkClass::DecodeTokens, 1))
            .unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(b.len(), 1);
    }

    #[test]
    fn um_microbatch_nao_duplica_a_mesma_sessao() {
        let mut q = SchedulerQueue::new(SchedulerConfig::test(8, 4));
        q.try_push(QueueItem::test("sess-a", WorkClass::DecodeTokens, 1))
            .unwrap();
        q.try_push(QueueItem::test("sess-a", WorkClass::DecodeTokens, 1))
            .unwrap();
        q.try_push(QueueItem::test("sess-b", WorkClass::DecodeTokens, 1))
            .unwrap();
        let batch = q.take_batch(Instant::now(), WorkClass::DecodeTokens);
        assert_eq!(batch.items.len(), 2);
        assert_eq!(batch.items[0].session_id, "sess-a");
        assert_eq!(batch.items[1].session_id, "sess-b");
    }

    #[test]
    fn fila_cheia_recusa_antes_de_alocar_trabalho() {
        let mut q = SchedulerQueue::new(SchedulerConfig::test(1, 1));
        q.try_push(QueueItem::test("sess-a", WorkClass::PrefillTokens, 1))
            .unwrap();
        assert!(matches!(
            q.try_push(QueueItem::test("sess-b", WorkClass::PrefillTokens, 1)),
            Err(QueueFull { .. })
        ));
    }

    #[test]
    fn limites_do_scheduler_sao_clampados_ao_contexto_do_no() {
        let config = SchedulerConfig::from_values(true, 0, 4, 99, 4096, 2, 512);
        assert_eq!(config.max_queue_requests, 1);
        assert_eq!(config.batch_window, std::time::Duration::from_millis(4));
        assert_eq!(config.max_batch_sequences, 2);
        assert_eq!(config.max_batch_tokens, 512);
    }
}
