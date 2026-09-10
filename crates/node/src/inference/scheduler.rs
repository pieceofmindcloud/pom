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
    /// A request that came from the distributed pipeline. It has one logical
    /// identity across hops, but its first prefill may still need a local KV
    /// slot on this peer.
    pub pipeline_session: bool,
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
            pipeline_session: false,
            class,
            tokens,
            enqueued_at: Instant::now(),
            payload: (),
        }
    }

    #[cfg(test)]
    fn pipeline_test(session_id: &str, class: WorkClass, tokens: usize) -> Self {
        let mut item = Self::test(session_id, class, tokens);
        item.pipeline_session = true;
        item
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

    /// Há quanto tempo espera o prefill de sessão nova mais antigo da fila.
    ///
    /// É o relógio da carência de reclamação: enquanto ninguém espera, uma
    /// sessão retida nunca é despejada; passada a carência, o escalonador
    /// reclama o slot para que a requisição tenha desfecho em vez de ficar
    /// pendurada esperando um slot que só sairia do `reap_idle`.
    pub fn oldest_new_session_wait(
        &self,
        now: Instant,
        is_existing: impl Fn(&str) -> bool,
    ) -> Option<Duration> {
        self.pending
            .iter()
            .filter(|item| {
                matches!(
                    item.class,
                    WorkClass::PrefillTokens | WorkClass::PrefillHidden
                ) && !item.pipeline_session
                    && !is_existing(item.session_id.as_str())
            })
            .map(|item| now.saturating_duration_since(item.enqueued_at))
            .max()
    }

    /// Whether a first prefill from the distributed pipeline is blocked by a
    /// full local sequence pool. This is intentionally separate from the
    /// standalone request grace timer: every hop shares the gateway's logical
    /// session, so a pipeline frame must never sleep for that timer.
    pub fn has_pipeline_new_session_wait(&self, is_existing: impl Fn(&str) -> bool) -> bool {
        self.pending.iter().any(|item| {
            item.pipeline_session
                && matches!(
                    item.class,
                    WorkClass::PrefillTokens | WorkClass::PrefillHidden
                )
                && !is_existing(item.session_id.as_str())
        })
    }

    /// Seleciona um microbatch da classe `class`, respeitando os limites e a
    /// **admissão de sessão**: um `Prefill*` para uma sessão que ainda não
    /// existe no motor (`is_existing` devolve `false`) consome um dos
    /// `new_session_slots` disponíveis; esgotado o orçamento, o item fica na
    /// fila em ordem em vez de ser despachado e recusado com "seq pool
    /// esgotado". Decode e continuações de sessão viva não consomem slot.
    pub fn take_batch(
        &mut self,
        _now: Instant,
        class: WorkClass,
        is_existing: impl Fn(&str) -> bool,
        new_session_slots: usize,
    ) -> BatchSelection<T> {
        let mut selected = Vec::new();
        let mut seen_sessions = HashSet::new();
        let mut total_tokens = 0usize;
        let mut new_used = 0usize;
        let pending_count = self.pending.len();

        for _ in 0..pending_count {
            let Some(item) = self.pending.pop_front() else {
                break;
            };
            let needs_slot = matches!(
                item.class,
                WorkClass::PrefillTokens | WorkClass::PrefillHidden
            ) && !is_existing(item.session_id.as_str());
            if item.class != class
                || seen_sessions.contains(item.session_id.as_str())
                || selected.len() >= self.config.max_batch_sequences
                || (needs_slot && new_used >= new_session_slots)
                || (total_tokens.saturating_add(item.tokens) > self.config.max_batch_tokens
                    && !selected.is_empty())
            {
                self.pending.push_back(item);
                continue;
            }

            if needs_slot {
                new_used += 1;
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
        let batch = q.take_batch(Instant::now(), WorkClass::DecodeTokens, |_| true, 0);
        assert_eq!(batch.items.len(), 2);
        assert_eq!(batch.items[0].session_id, "sess-a");
        assert_eq!(batch.items[1].session_id, "sess-b");
    }

    #[test]
    fn a_new_session_prefill_waits_in_order_when_no_slot_is_free() {
        let mut q = SchedulerQueue::new(SchedulerConfig::test(8, 4));
        q.try_push(QueueItem::test("live", WorkClass::PrefillTokens, 10))
            .unwrap();
        q.try_push(QueueItem::test("wait-1", WorkClass::PrefillTokens, 10))
            .unwrap();
        q.try_push(QueueItem::test("wait-2", WorkClass::PrefillTokens, 10))
            .unwrap();

        // "live" já tem sessão; os outros dois são novos e não há slot livre.
        let existing = |sid: &str| sid == "live";
        let batch = q.take_batch(Instant::now(), WorkClass::PrefillTokens, existing, 0);
        assert_eq!(
            batch
                .items
                .iter()
                .map(|i| i.session_id.as_str())
                .collect::<Vec<_>>(),
            ["live"],
            "só a continuação da sessão viva entra"
        );
        assert_eq!(q.len(), 2, "os dois novos ficam na fila");

        // Abre um slot: o próximo da fila entra, o outro continua esperando.
        let batch = q.take_batch(Instant::now(), WorkClass::PrefillTokens, |_| false, 1);
        assert_eq!(
            batch
                .items
                .iter()
                .map(|i| i.session_id.as_str())
                .collect::<Vec<_>>(),
            ["wait-1"],
            "FIFO: wait-1 antes de wait-2"
        );
        assert_eq!(q.len(), 1);
    }

    /// O outro lado da corrida do incidente: com `max_sessions=1` e a sessão
    /// do agente retida, o pedido pequeno não é admitido - fica na fila com a
    /// espera medida, em vez de entrar despejando o prefixo de 62 mil tokens.
    #[test]
    fn prefill_novo_espera_em_vez_de_despejar_a_sessao_retida() {
        let mut q = SchedulerQueue::new(SchedulerConfig::test(4, 1));
        q.try_push(QueueItem::test("title", WorkClass::PrefillTokens, 35))
            .unwrap();

        // Nenhum slot livre: a sessão retida do agente é cache quente, não
        // capacidade, então `new_session_slots` devolveu zero.
        let batch = q.take_batch(Instant::now(), WorkClass::PrefillTokens, |_| false, 0);
        assert!(batch.items.is_empty(), "o pedido pequeno não é admitido");
        assert_eq!(q.len(), 1, "e continua na fila, sem ser recusado");

        // A espera é observável, que é o relógio da carência de reclamação.
        let waited = q.oldest_new_session_wait(Instant::now(), |_| false);
        assert!(waited.is_some(), "a espera do prefill novo é medida");

        // Uma sessão já viva não é contada como espera de sessão nova.
        assert!(q
            .oldest_new_session_wait(Instant::now(), |_| true)
            .is_none());
    }

    #[test]
    fn prefill_de_pipeline_nao_entra_na_carencia_de_pedido_avulso() {
        let mut q = SchedulerQueue::new(SchedulerConfig::test(4, 1));
        q.try_push(QueueItem::pipeline_test(
            "pipeline",
            WorkClass::PrefillTokens,
            35,
        ))
        .unwrap();

        assert!(q.has_pipeline_new_session_wait(|_| false));
        assert!(
            q.oldest_new_session_wait(Instant::now(), |_| false)
                .is_none(),
            "uma requisição de pipeline não pode armar o timer do pedido avulso"
        );
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
