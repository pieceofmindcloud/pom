use super::*;

// ── Resposta direta (`relay/v2-direct`) ──────────────────────────────────────

/// Para onde o resultado desta cadeia deve ir, quando quem despachou pediu
/// resposta direta. Ausente em todo quadro do modo hub e do anel borbulhado —
/// é este campo, e só ele, que liga o caminho novo.
pub(crate) fn direct_reply_to(meta: &Value) -> Option<Value> {
    meta.get("reply_to").filter(|v| !v.is_null()).cloned()
}

/// O que este hop devolve a quem o chamou quando a cadeia responde direto: um
/// aceite, e nada mais. A resposta de verdade não passa por aqui — ver
/// [`crate::direct`].
pub(crate) fn direct_ack() -> Response {
    direct_ack_with_progress(None)
}

pub(crate) fn direct_ack_with_progress(progress: Option<Value>) -> Response {
    let mut meta = json!({"accepted": true});
    if let Some(progress) = progress {
        meta["prompt_progress"] = progress;
    }
    match wire::encode_batch_single(&meta, None, WireDtype::F16) {
        Ok(bytes) => (
            StatusCode::OK,
            [("content-type", "application/octet-stream")],
            bytes,
        )
            .into_response(),
        Err(e) => err500(e),
    }
}

/// Repassa o residual ao próximo hop e some. Roda numa task própria: quem nos
/// chamou já foi aceito e não espera mais nada daqui.
///
/// Falhou o repasse, quem denuncia somos nós — direto ao gateway, nomeando o
/// nó que realmente morreu. É o caminho que substitui o `RelayFailure` subindo
/// pela pilha aninhada do anel borbulhado.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn forward_direct(
    hops: Vec<PipelineStep>,
    path: &'static str,
    base_meta: Value,
    residual: Vec<f32>,
    shape: TensorShape,
    dtype: WireDtype,
    attempts: u32,
    reply_to: Value,
) {
    forward_direct_ordered(
        hops, path, base_meta, residual, shape, dtype, attempts, reply_to, None, None, None,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn forward_direct_ordered(
    hops: Vec<PipelineStep>,
    path: &'static str,
    mut base_meta: Value,
    residual: Vec<f32>,
    shape: TensorShape,
    dtype: WireDtype,
    attempts: u32,
    reply_to: Value,
    order: Option<DirectForwardOrder>,
    sequence_id: Option<String>,
    chunk_index: Option<u32>,
) {
    let next = hops[0].clone();
    let remaining = &hops[1..];
    let terminal = base_meta
        .get("is_final_chunk")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let turn = match (order, sequence_id, chunk_index) {
        (Some(order), Some(sequence_id), Some(chunk_index)) => {
            match order.wait_turn(sequence_id, chunk_index).await {
                Ok(turn) => Some(turn),
                Err(error) => {
                    let failure = crate::relay::RelayFailure {
                        failed_node_id: next.node_id.clone(),
                        kind: "protocol".into(),
                        message: error.to_string(),
                    };
                    crate::direct::deliver_failure(http_client(), &reply_to, failure).await;
                    return;
                }
            }
        }
        _ => None,
    };
    let destination_hop = base_meta
        .get("hop_timings")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    base_meta["is_last"] = json!(remaining.is_empty());
    base_meta["scale"] = json!(1.0);
    base_meta["next_hops"] = json!(remaining);
    base_meta["exec_start"] = json!(next.exec_start);
    base_meta["exec_end"] = json!(next.exec_end);
    base_meta["reply_to"] = reply_to.clone();

    let mut last_issue_kind = None;
    let sent = crate::relay::send_frame_or_tunnel(
        http_client(),
        &next,
        crate::infer_tunnel::tunnel_sender(),
        path,
        &base_meta,
        Some((&residual, shape)),
        dtype,
        dtype.frame_encoding(),
        attempts,
        &next.node_id,
        |issue| last_issue_kind = Some(issue.kind),
    )
    .await;

    match sent {
        Ok(sent) => {
            if let Some(turn) = turn {
                turn.finish(true, terminal);
            }
            crate::direct::deliver_transport(http_client(), &reply_to, destination_hop, &sent)
                .await;
        }
        Err(e) => {
            if let Some(turn) = turn {
                turn.finish(false, false);
            }
            let failure = crate::relay::extract_relay_failure(&e).unwrap_or_else(|| {
                crate::relay::RelayFailure {
                    failed_node_id: next.node_id.clone(),
                    kind: issue_kind_label(last_issue_kind).to_string(),
                    message: e.to_string(),
                }
            });
            warn!(
                "repasse direto para {} falhou: {} - avisando o gateway",
                failure.failed_node_id, failure.message
            );
            crate::direct::deliver_failure(http_client(), &reply_to, failure).await;
        }
    }
}

pub(super) fn issue_kind_label(kind: Option<crate::relay::SendIssueKind>) -> &'static str {
    match kind {
        Some(crate::relay::SendIssueKind::Timeout) => "timeout",
        Some(crate::relay::SendIssueKind::Transport) => "transport",
        Some(crate::relay::SendIssueKind::Http) => "http",
        Some(crate::relay::SendIssueKind::Protocol) => "protocol",
        None => "unknown",
    }
}

/// Search `history` for the most recent prior occurrence of its own trailing
/// `m` tokens, and propose whatever followed there as a draft continuation
/// of up to `k - 1` tokens. An empty result (no history, no match, or the
/// match runs off the end of `history` with nothing after it) means "draft
/// nothing" — the caller then decodes exactly `[confirmed_token]`, identical
/// to today's non-speculative behavior. This never looks at model weights or
/// probabilities — it's a plain array search — which is exactly why
/// verification (`longest_match`, below) can never trust it: the draft is
/// only ever a guess about what to *batch*, never a substitute for sampling.
/// `m` is the **longest** n-gram tried, not the only one: the search walks
/// `m, m-1, … 1` and stops at the first length that matches, so a window that
/// finds nothing degrades into a shorter, weaker guess instead of into no
/// guess at all. Longer wins because it is stronger evidence; how much of the
/// continuation is trusted scales with the matched length (`m` tokens of
/// context authorise at most `m + 1` guesses), because a one-token match
/// pointing anywhere in the history is nearly a coin flip and every extra
/// drafted token widens the batch on every hop of the ring.
/// Vale rascunhar nesta rodada?
///
/// Especular é apostar: um rascunho recusado tem de ser desfeito do cache
/// antes da rodada seguinte (`gateway::rollback_sessions`, via
/// `/v1/session/rollback` -> `kv_remove` -> `llama_memory_seq_rm`).
///
/// `kv_can_shift` não decide se a cauda monta o próximo `next_draft`: ele é a
/// capacidade de K-shift usada pelo `--ctx-shift` de referência do llama.cpp,
/// não a capacidade de desfazer um rascunho. O rollback especulativo real usa
/// `kv_remove` (`llama_memory_seq_rm`) depois de cada lote e falha fechado se o
/// motor recusar a remoção. O caminho remoto faz esse rollback local em cada
/// hop antes de borbulhar a resposta; o caminho legado mantém o endpoint
/// `handle_rollback` e o fan-out do gateway.
pub(crate) fn mtp_tail_speculation_reason(
    mtp_ready: bool,
    mtp_unavailable_reason: Option<&str>,
) -> Option<&'static str> {
    if !mtp_ready {
        return Some(
            if mtp_unavailable_reason
                .is_some_and(|reason| reason == crate::llm::MTP_NO_NEXTN_REASON)
            {
                SPECULATION_REASON_TAIL_WITHOUT_NEXTN
            } else {
                SPECULATION_REASON_MODEL_WITHOUT_MTP
            },
        );
    }
    None
}

pub(crate) fn should_draft(speculate: Option<Speculate>, mtp_ready: bool) -> bool {
    match speculate {
        None => false,
        Some(s) if s.source == DraftSource::Mtp => {
            // O rascunho pode nascer aqui (bloco `nextn` local, máquina
            // única) ou vir pronto do gateway (a cauda de um pipeline
            // fatiado o produziu na rodada anterior).
            mtp_ready || s.draft.is_some()
        }
        Some(_) => true,
    }
}

/// O convite a rascunhar que a cabeça recebe numa rodada.
///
/// `source` é o campo que separa os dois mecanismos, e ele existe porque eles
/// **não** são a mesma coisa com números diferentes: `Lookup` é uma busca por
/// repetição no texto já gerado (`find_draft`, nenhum peso envolvido) e `Mtp`
/// é o bloco `nextn` treinado dentro do GGUF (`crate::mtp`). Um `speculate`
/// sem `source` vem de um gateway antigo e vale `Lookup`, que é o que ele
/// sempre quis dizer.
#[derive(Debug, Clone)]
pub(crate) struct Speculate {
    /// Largura do lote: o token confirmado mais até `k - 1` palpites.
    pub k: u32,
    /// Janela de história do `find_draft`. Ignorada pelo `Mtp`, que não olha
    /// história nenhuma.
    pub m: u32,
    pub source: DraftSource,
    /// Rascunho pronto, produzido pela cauda num pipeline fatiado. A cabeça usa
    /// este em vez de chamar `mtp_draft`. `None` significa que a cabeça deve
    /// produzir o próprio rascunho (modelo inteiro) ou que não há rascunho.
    pub draft: Option<Vec<i32>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DraftSource {
    Lookup,
    Mtp,
}

impl DraftSource {
    /// O nome no fio (wire), espelhando o `DraftSource::wire` de `prefill.rs`.
    pub(crate) fn wire(&self) -> &'static str {
        match self {
            DraftSource::Lookup => "lookup",
            DraftSource::Mtp => "mtp",
        }
    }
}

impl Speculate {
    /// Lê o convite do campo `speculate` do quadro.
    ///
    /// Público ao crate porque a bancada `spec_lab` fala o protocolo de
    /// verdade: uma segunda cópia desta leitura ali passaria a valer como
    /// contrato sem nunca ter sido conferida contra esta.
    pub(crate) fn from_meta(meta: &Value) -> Option<Self> {
        let v = meta.get("speculate")?;
        let draft = v.get("draft").and_then(|d| {
            d.as_array().map(|a| {
                a.iter()
                    .filter_map(|v| v.as_i64())
                    .map(|v| v as i32)
                    .collect()
            })
        });
        Some(Speculate {
            k: v.get("k")?.as_u64()? as u32,
            m: v.get("m")?.as_u64()? as u32,
            source: match v.get("source").and_then(|s| s.as_str()) {
                Some("mtp") => DraftSource::Mtp,
                _ => DraftSource::Lookup,
            },
            draft,
        })
    }

    /// Serializa o convite para o campo `speculate` do quadro - a contraparte
    /// de `from_meta`. O caminho direto legado usa o valor preenchido pelo
    /// gateway; no stream remoto, a cabeça preenche o próprio valor antes da
    /// rodada seguinte.
    pub(crate) fn to_meta(&self) -> Value {
        let mut v = json!({"k": self.k, "m": self.m, "source": self.source.wire()});
        if let Some(draft) = &self.draft {
            v["draft"] = json!(draft);
        }
        v
    }
}

pub(crate) fn find_draft(history: &[i32], m: usize, k: u32) -> Vec<i32> {
    let extra = k.saturating_sub(1) as usize;
    if extra == 0 {
        return Vec::new();
    }
    for len in (1..=m.max(1)).rev() {
        if history.len() <= len {
            continue;
        }
        let needle = &history[history.len() - len..];
        let allowed = extra.min(len + 1);
        // Most recent first: `start` walks backward from the last position a
        // *prior* (non-overlapping-with-needle) occurrence could begin.
        for start in (0..history.len() - len).rev() {
            if &history[start..start + len] != needle {
                continue;
            }
            let continuation_start = start + len;
            if continuation_start >= history.len() {
                continue;
            }
            let continuation_end = (continuation_start + allowed).min(history.len());
            return history[continuation_start..continuation_end].to_vec();
        }
    }
    Vec::new()
}

/// How many leading positions `sampled` and `drafted` agree on. `sampled`
/// holds one real, freshly-sampled token per decoded position (always
/// computed, whether or not it ends up "accepted"); `drafted` holds what was
/// guessed for positions `1..`, one shorter than `sampled` since position 0
/// is the already-confirmed seed token with nothing in `drafted` to compare
/// it against. The accepted output is always `sampled[..=result]` — the
/// matched prefix plus one more real sample, which is the genuine next token
/// whether it means "the draft's next unverified guess" (full match) or
/// "the correction at the first place the draft was wrong" (partial match).
pub(crate) fn longest_match(sampled: &[i32], drafted: &[i32]) -> usize {
    sampled
        .iter()
        .zip(drafted)
        .take_while(|(a, b)| a == b)
        .count()
}

/// New `n_past` after discarding the speculative positions that weren't
/// confirmed. `batch_width` positions were decoded into the KV cache;
/// `accept_count` of them (the confirmed seed token plus every draft token
/// that matched — see `longest_match`) are valid and kept, the rest must be
/// rolled back with `LlmEngine::kv_remove`. Saturating throughout: a caller
/// passing a degenerate `accept_count > batch_width` gets "roll back
/// nothing" rather than a panic or a wrapped position.
pub(super) fn rollback_from(n_past_after_batch: u32, batch_width: u32, accept_count: u32) -> u32 {
    n_past_after_batch.saturating_sub(batch_width.saturating_sub(accept_count))
}

#[cfg(test)]
mod chat_template_tests {
    use super::use_chat_template;

    #[test]
    fn request_mode_overrides_the_legacy_server_default() {
        assert!(use_chat_template(Some(true), false));
        assert!(!use_chat_template(Some(false), true));
        assert!(use_chat_template(None, true));
    }
}

#[cfg(test)]
mod speculative_tests {
    use super::*;

    fn lookup(k: u32, m: u32) -> Option<Speculate> {
        Some(Speculate {
            k,
            m,
            source: DraftSource::Lookup,
            draft: None,
        })
    }

    fn mtp(k: u32) -> Option<Speculate> {
        Some(Speculate {
            k,
            m: 0,
            source: DraftSource::Mtp,
            draft: None,
        })
    }

    /// Um pedido de lookup sempre rascunha - a decisão de aceitar o desfazer
    /// não é mais estática (`kv_can_shift`), é o `seq_rm` de verdade em
    /// `handle_rollback`, checada em tempo real. Ver o doc de `should_draft`.
    #[test]
    fn a_lookup_round_always_drafts_when_asked() {
        assert!(should_draft(lookup(4, 3), false));
        assert!(should_draft(lookup(4, 3), true));
    }

    /// Pedir MTP a um motor sem a cabeça `nextn` e sem rascunho pronto é o
    /// caso normal de uma fatia que não é a final: recusar é a resposta
    /// certa, e o gateway lê a recusa (`speculated: false`) e para de pedir.
    /// Rascunhar por lookup no lugar seria trocar o mecanismo por baixo de
    /// quem pediu.
    #[test]
    fn mtp_asked_of_an_engine_without_the_nextn_head_is_refused() {
        assert!(!should_draft(mtp(4), false));
        assert!(should_draft(mtp(4), true));
    }

    #[test]
    fn razoes_da_cauda_separam_nextn_modelo_e_prontidao() {
        assert_eq!(
            mtp_tail_speculation_reason(false, Some(crate::llm::MTP_NO_NEXTN_REASON)),
            Some(SPECULATION_REASON_TAIL_WITHOUT_NEXTN)
        );
        assert_eq!(
            mtp_tail_speculation_reason(false, Some("falha de alocação")),
            Some(SPECULATION_REASON_MODEL_WITHOUT_MTP)
        );
        assert_eq!(mtp_tail_speculation_reason(true, None), None);
    }

    /// Um rascunho MTP já pronto (a cauda de um pipeline fatiado produziu na
    /// rodada anterior) faz a cabeça aceitar mesmo sem a cabeça `nextn` local
    /// - ela só precisa verificar o lote, não gerá-lo.
    #[test]
    fn a_premade_mtp_draft_is_accepted_without_the_local_nextn_head() {
        let mut spec = mtp(4).unwrap();
        spec.draft = Some(vec![1, 2, 3]);
        assert!(should_draft(Some(spec), false));
    }

    /// Sem especulacao pedida, o resto nao importa.
    #[test]
    fn without_speculation_asked_there_is_no_draft() {
        assert!(!should_draft(None, true));
        assert!(!should_draft(None, false));
    }

    /// O campo `source` é o que separa os dois mecanismos no fio. Um quadro
    /// de gateway antigo não o traz, e "ausente" tem que continuar querendo
    /// dizer lookup: era a única especulação que existia quando ele foi
    /// escrito.
    #[test]
    fn a_frame_without_a_draft_source_still_means_lookup() {
        let spec =
            Speculate::from_meta(&json!({"speculate": {"k": 4, "m": 3}})).expect("k e m bastam");
        assert_eq!(spec.source, DraftSource::Lookup);
        assert_eq!((spec.k, spec.m), (4, 3));
    }

    #[test]
    fn a_frame_asking_for_mtp_says_so() {
        let spec = Speculate::from_meta(&json!({"speculate": {"k": 4, "m": 0, "source": "mtp"}}))
            .expect("quadro completo");
        assert_eq!(spec.source, DraftSource::Mtp);
    }

    #[test]
    fn a_frame_without_speculate_asks_for_no_draft_at_all() {
        assert!(Speculate::from_meta(&json!({"session_id": "x"})).is_none());
    }

    #[test]
    fn find_draft_returns_nothing_without_a_match() {
        assert_eq!(find_draft(&[1, 2, 3, 4, 5], 3, 4), Vec::<i32>::new());
    }

    #[test]
    fn find_draft_proposes_what_followed_the_match() {
        // Trailing 3 tokens [1,2,3] last occurred at index 1..4; [7,1]
        // followed it there.
        assert_eq!(find_draft(&[9, 1, 2, 3, 7, 1, 2, 3], 3, 3), vec![7, 1]);
    }

    #[test]
    fn find_draft_needs_more_history_than_just_the_needle() {
        assert_eq!(find_draft(&[1, 2, 3], 3, 5), Vec::<i32>::new());
    }

    #[test]
    fn find_draft_k_of_one_never_drafts_anything() {
        assert_eq!(find_draft(&[1, 2, 3, 4, 5], 2, 1), Vec::<i32>::new());
    }

    #[test]
    fn find_draft_picks_the_most_recent_occurrence() {
        // [1,2,3] occurs at 0..3 and 4..7; the earlier one is followed by
        // 99, the later (more recent) one by 88. The more recent one must win.
        let history = [1, 2, 3, 99, 1, 2, 3, 88, 1, 2, 3];
        assert_eq!(find_draft(&history, 3, 2), vec![88]);
    }

    /// A janela é o **maior** n-grama tentado, não o único. Sem o recuo, um
    /// texto onde só o último token se repete não rascunha nada e a
    /// especulação fica ociosa justamente onde ela é barata (verificar K
    /// tokens num MoE custa quase o mesmo que verificar 1).
    #[test]
    fn find_draft_falls_back_to_a_shorter_ngram_when_the_window_does_not_match() {
        // Janela 3 = [7,8,9], que nunca ocorreu antes. O sufixo [9] ocorreu,
        // seguido de 42.
        let history = [1, 9, 42, 43, 7, 8, 9];
        assert_eq!(find_draft(&history, 3, 4), vec![42, 43]);
    }

    /// O n-grama mais longo continua tendo precedência: ele é a evidência mais
    /// forte, e um casamento curto que aponte para outro lugar não pode
    /// atropelá-lo.
    #[test]
    fn find_draft_prefers_the_longest_ngram_available() {
        //           0  1  2  3   4  5  6  7
        let history = [5, 6, 7, 90, 9, 70, 5, 6, 7];
        // Janela 3 ([5,6,7]) casa em 0..3, seguido de 90. O sufixo [7] casa
        // mais tarde (índice 2) — mas o de 3 vence.
        assert_eq!(find_draft(&history, 3, 2), vec![90]);
    }

    /// Um casamento de um token só é evidência fraca; ele rascunha menos que um
    /// casamento longo. Rejeição não corrompe nada (a verificação é sempre
    /// contra o token amostrado de verdade), mas cada token rascunhado a mais
    /// alarga o batch de todo hop do anel.
    #[test]
    fn find_draft_trusts_a_short_match_for_fewer_tokens() {
        let history = [4, 1, 2, 3, 9, 9, 9, 4];
        // Sufixo [4] casa no índice 0; k=8 pediria 7 tokens, mas um casamento
        // de comprimento 1 só autoriza 2.
        assert_eq!(find_draft(&history, 1, 8), vec![1, 2]);
    }

    #[test]
    fn longest_match_stops_at_the_first_disagreement() {
        assert_eq!(longest_match(&[1, 2, 9, 4], &[1, 2, 3]), 2);
    }

    #[test]
    fn longest_match_of_a_full_match_is_the_drafts_whole_length() {
        assert_eq!(longest_match(&[1, 2, 3, 4], &[1, 2, 3]), 3);
    }

    #[test]
    fn longest_match_against_an_empty_draft_is_zero() {
        assert_eq!(longest_match(&[1, 2, 3], &[]), 0);
    }

    #[test]
    fn rollback_from_a_full_match_is_a_no_op() {
        assert_eq!(rollback_from(100, 4, 4), 100);
    }

    #[test]
    fn rollback_from_a_partial_match_removes_the_gap() {
        // n_past was 96 before the batch, 4 positions were decoded, only 2
        // (the seed + one draft token) were confirmed — 2 must come off.
        assert_eq!(rollback_from(100, 4, 2), 98);
    }

    #[test]
    fn rollback_from_saturates_instead_of_underflowing() {
        assert_eq!(rollback_from(100, 4, 0), 96);
        // Degenerate: accept_count larger than batch_width still can't
        // remove more than the whole batch.
        assert_eq!(rollback_from(100, 4, 9), 100);
    }
}
