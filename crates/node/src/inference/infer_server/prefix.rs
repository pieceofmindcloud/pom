use super::*;

// ── Session ───────────────────────────────────────────────────────────────────

/// O que a cabeça **executou** com o prefixo retido no pedaço 0.
///
/// Viaja de volta ao gateway e adiante para os hops seguintes. A distinção
/// importa: o gateway escolhe *tentar* reusar, a cabeça é quem decide se deu, e
/// era exatamente essa diferença que a telemetria antiga não mostrava.

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PrefixDecision {
    pub(crate) reused: bool,
    pub(crate) cached_tokens: u32,
    /// Quantos tokens abriram o prefixo retido, mesmo quando a divergência
    /// obrigou o prefill a recomeçar a frio.
    pub(crate) matched_tokens: u32,
}

/// A regra do reuso, sem sessão nem motor por perto.
///
/// `retidos` são exatamente os tokens do prompt do turno anterior que estão no
/// KV, e portanto seu tamanho tem que ser `n_past`. O prefixo serve quando
/// esses tokens ainda abrem o prompt novo e sobra pelo menos um token para
/// decodificar:
/// aí o prefill continua da posição `n_past` e **nenhuma posição sai do KV**,
/// que é a diferença entre funcionar em todo modelo e depender de o motor
/// aceitar `llama_memory_seq_rm` parcial.
///
/// Note que a comparação é contra `n_past`, não contra o prompt retido inteiro:
/// o que o turno anterior guardou no KV termina antes do rabicho volátil do
/// template de conversa (ver `stable_prefix_len`). Divergir depois de `n_past`
/// é o caso **normal** - é onde a conversa cresceu.
///
/// Recusa quando os `n_past` tokens não abrem mais o prompt novo (a conversa
/// foi editada, trocou de template, mudou a lista de ferramentas), quando não
/// sobraria token para decodificar, e quando a sessão não tem prompt no KV.
/// Recusar custa o reuso daquele turno e nunca a requisição.
pub(crate) fn plan_prefix_reuse(retidos: &[i32], n_past: u32, novos: &[i32]) -> PrefixDecision {
    let comum = retidos
        .iter()
        .zip(novos)
        .take_while(|(old, new)| old == new)
        .count() as u32;
    if retidos.len() as u32 != n_past {
        return PrefixDecision {
            matched_tokens: comum.min(n_past),
            ..PrefixDecision::default()
        };
    }
    // O último token do novo prompt tem de ser decodificado: é dele que saem os
    // logits do primeiro token gerado.
    let cabe_decodificar = (n_past as usize) < novos.len();
    if n_past > 0 && comum >= n_past && cabe_decodificar {
        PrefixDecision {
            reused: true,
            cached_tokens: n_past,
            matched_tokens: n_past,
        }
    } else {
        PrefixDecision {
            matched_tokens: comum.min(n_past),
            ..PrefixDecision::default()
        }
    }
}

/// Quantos tokens do começo deste prompt continuam valendo quando a conversa
/// crescer - a fronteira entre o que vale a pena reter e o que é deste turno.
///
/// **Medido, não presumido.** O template de conversa fecha o prompt com o
/// rabicho que abre o turno do assistente (`<|im_end|>\n<|im_start|>assistant\n`
/// no chatml, outra coisa noutro modelo). Esse rabicho fica no *fim* do prompt,
/// então o prompt de um turno nunca é prefixo do prompt do turno seguinte: o
/// texto novo entra antes dele. Medido no Ornith-1.0-35B: prompt de 50 tokens,
/// 45 estáveis, 5 voláteis - e era exatamente por 5 tokens que todo reuso era
/// recusado, com a retenção e o cache funcionando perfeitamente.
///
/// A sonda renderiza o mesmo prompt com a continuação que `render_prompt`
/// acrescentaria e compara os tokens: o prefixo comum é, por construção, a
/// parte que sobrevive. Vale para qualquer template e qualquer tokenizador,
/// porque é medição e não suposição sobre o formato. Sonda conservadora custa
/// alguns tokens de reuso; nunca corrompe, porque `plan_prefix_reuse` só
/// aproveita o que está de fato no KV.
pub(super) fn stable_prefix_len(
    engine: &crate::llm::LlmEngine,
    texto: &str,
    use_tmpl: bool,
    no_think: bool,
    tokens: &[i32],
) -> usize {
    // A forma exata do que o turno seguinte acrescenta - ver `render_prompt`.
    const CONTINUACAO: &str = "\n\nAssistant: x";
    let sondado = if use_tmpl {
        match engine.apply_chat_template(
            &[("user", &format!("{texto}{CONTINUACAO}"))],
            true,
            no_think,
        ) {
            Ok(rendered) => rendered,
            // Sem sonda não há fronteira medida: não retenha nada que possa
            // depender de um template que não conseguimos comparar.
            Err(_) => return 0,
        }
    } else {
        format!("{texto}{CONTINUACAO}")
    };
    let sondados = engine.tokenize(&sondado, !use_tmpl, true);
    let comum = tokens
        .iter()
        .zip(&sondados)
        .take_while(|(a, b)| a == b)
        .count();
    // Reserve the final prompt token for `gen_seq`. Without this cap, a raw
    // prompt whose continuation tokenizes independently could leave the whole
    // prompt in `prompt_seq`, putting the first generated token back into the
    // retained sequence.
    retainable_prefix_len(comum, tokens.len())
}

pub(super) fn retainable_prefix_len(common: usize, prompt_len: usize) -> usize {
    common.min(prompt_len.saturating_sub(1))
}

/// A posição absoluta do KV em que este pedaço de prompt deve começar.
///
/// `start_pos` vem do peer que tokenizou. Ele só pode faltar no pedaço 0, onde
/// a posição da própria sessão é a referência: zero numa sessão recomeçada,
/// ou o fim do prefixo retido quando houve reuso. Para qualquer pedaço
/// posterior, inferir a posição com `chunk_index * chunk_size` é incorreto:
/// um pedaço anterior pode ter terminado no token 42, não no múltiplo 512.
/// Recusar o frame sem posição nomeia a divergência e deixa o gateway fazer o
/// fallback frio, em vez de misturar KV de sessões diferentes.
pub(super) fn expected_prefill_pos(
    start_pos: Option<u32>,
    chunk_index: u32,
    session_n_past: u32,
) -> Result<u32> {
    match start_pos {
        Some(position) => Ok(position),
        None if chunk_index == 0 => Ok(session_n_past),
        None => bail!(
            "chunk {chunk_index} sem start_pos absoluto; não é seguro derivar a posição do índice"
        ),
    }
}

#[cfg(test)]
mod prefix_rule_tests {
    use super::*;

    /// O caso do relatório: o cache acerta, o pedaço 0 é julgado por zero, e o
    /// reuso é desfeito antes de decodificar.
    #[test]
    fn chunk_zero_of_a_hit_starts_where_the_retained_prefix_ends() {
        assert_eq!(expected_prefill_pos(None, 0, 1523).unwrap(), 1523);
    }

    #[test]
    fn chunk_zero_of_a_cold_session_starts_at_zero() {
        assert_eq!(expected_prefill_pos(None, 0, 0).unwrap(), 0);
    }

    /// Do pedaço 1 em diante quem manda é a posição que o peer devolveu.
    #[test]
    fn later_chunks_follow_the_position_the_head_reported() {
        assert_eq!(expected_prefill_pos(Some(1600), 1, 1600).unwrap(), 1600);
        assert!(expected_prefill_pos(None, 2, 1024).is_err());
    }

    #[test]
    fn partial_head_prefill_publishes_absolute_progress() {
        let meta = partial_prefill_meta(true, 600, 42, 512);

        assert_eq!(meta["done"], false);
        assert_eq!(meta["more_chunks"], true);
        assert_eq!(meta["n_prompt_tokens"], 600);
        assert_eq!(meta["start_pos"], 42);
        assert_eq!(meta["next_pos"], 512);
    }

    #[test]
    fn a_conversation_that_only_grows_reuses_the_whole_previous_prompt() {
        let turno1 = vec![10, 11, 12, 13];
        let turno2 = vec![10, 11, 12, 13, 20, 21];
        let d = plan_prefix_reuse(&turno1, 4, &turno2);
        assert!(d.reused);
        assert_eq!(d.cached_tokens, 4);
    }

    /// Divergência no meio exigiria encolher o KV. Recomeçar é a resposta, e
    /// ela não custa nada além do reuso daquele turno.
    #[test]
    fn a_prompt_that_diverges_restarts_instead_of_shrinking_the_cache() {
        let d = plan_prefix_reuse(&[10, 11, 12, 13], 4, &[10, 11, 99, 13, 20]);
        assert!(!d.reused);
        assert_eq!(d.cached_tokens, 0);
        assert_eq!(d.matched_tokens, 2);
    }

    /// Prompt idêntico ao retido: não sobraria token para decodificar, e recuar
    /// um só continuaria sendo remoção parcial. Recomeça.
    #[test]
    fn an_identical_prompt_restarts_because_nothing_would_be_left_to_decode() {
        let tokens = vec![10, 11, 12, 13];
        let d = plan_prefix_reuse(&tokens, 4, &tokens);
        assert!(!d.reused);
    }

    #[test]
    fn stable_frontier_always_leaves_a_prompt_token_for_generation() {
        assert_eq!(retainable_prefix_len(9, 10), 9);
        assert_eq!(retainable_prefix_len(10, 10), 9);
        assert_eq!(retainable_prefix_len(1, 1), 0);
        assert_eq!(retainable_prefix_len(0, 0), 0);
    }

    /// O KV pode ter menos posições que os tokens guardados (turno anterior
    /// interrompido no meio do prefill). Reusar até `n_past` seria mentira
    /// sobre o que está no cache.
    #[test]
    fn a_partially_ingested_prompt_is_not_offered_as_a_prefix() {
        let d = plan_prefix_reuse(&[10, 11, 12, 13], 2, &[10, 11, 12, 13, 20]);
        assert!(!d.reused, "o retido tem 4 tokens mas só 2 estão no KV");
    }

    #[test]
    fn a_session_with_no_prompt_in_the_cache_is_not_a_hit() {
        assert!(!plan_prefix_reuse(&[], 0, &[10, 11]).reused);
    }

    /// Cada sessão tem uma sequência de prompt e a de geração emparelhada com
    /// ela - ver `llm::kv_seq_budget`. Os dois conjuntos não podem se cruzar.
    #[test]
    fn prompt_and_generation_sequences_never_collide() {
        let max_sessions = 4u32;
        let orcamento = crate::llm::kv_seq_budget(max_sessions);
        assert_eq!(orcamento, 8);
        let prompts: Vec<u32> = (0..max_sessions).collect();
        let geracoes: Vec<u32> = prompts.iter().map(|s| s + max_sessions).collect();
        for g in &geracoes {
            assert!(
                !prompts.contains(g),
                "sequência {g} servindo aos dois papéis"
            );
            assert!(
                *g < orcamento,
                "sequência {g} fora do orçamento do contexto"
            );
        }
    }
}

/// Uma conversa viva neste peer, com o KV do prompt separado do KV da geração.
///
/// **O prompt nunca é estendido.** `prompt_seq` recebe os pedaços do prompt e
/// para aí; assim que o turno vai gerar, `open_generation` copia esse KV para
/// `gen_seq` e todo token gerado cai na cópia. No fim do turno a cópia é
/// apagada inteira - `kv_remove(gen, 0, -1)`, remoção total, que todo motor
/// aceita - e `prompt_seq` continua exatamente com o prompt, pronto para o
/// turno seguinte. É por isso que o reuso de prefixo não depende de o modelo
/// aceitar remoção parcial de KV: nada precisa ser removido do meio.
pub(super) struct Session {
    /// Sequência do prompt. Só o prefill escreve nela.
    pub(super) prompt_seq: u32,
    /// Sequência da geração deste turno, aberta na primeira decodificação de
    /// token gerado. `None` entre turnos.
    pub(super) gen_seq: Option<u32>,
    /// Quantos tokens de prompt estão no KV de `prompt_seq`. É a posição para
    /// onde a sessão volta no fim do turno, e o que o gateway confirma no
    /// `/v1/session/rewind`.
    pub(super) prompt_len: u32,
    /// Onde acaba a parte do prompt que sobrevive ao turno seguinte, e portanto
    /// onde a sequência de geração começa - ver `stable_prefix_len`. Tudo daqui
    /// para a frente (o rabicho do template e os tokens gerados) vive na cópia
    /// e some no fim do turno. `u32::MAX` é "ainda não sei", que mantém tudo na
    /// sequência do prompt: é o estado dos hops antes de a cabeça informar.
    pub(super) stable_len: u32,
    pub(super) n_past: u32,
    pub(super) recent_tokens: Vec<i32>,
    /// Stored by the first peer so subsequent chunks don't re-tokenize.
    pub(super) prompt_tokens: Vec<i32>,
    /// UTF-8 tail for this request's token stream. It must never be shared
    /// between sessions because a byte prefix from one generation can only be
    /// completed by a later token in that same generation.
    pub(super) utf8: crate::llm::Utf8Assembler,
    /// `true` entre turnos: o KV guarda só o prompt e ninguém está gerando.
    /// Uma sessão retida é cache reclamável - `alloc_seq` pode despejá-la
    /// quando falta slot, e o turno seguinte apenas perde o reuso.
    pub(super) retained: bool,
    pub(super) last_used: Instant,
}

impl Session {
    pub(super) fn new(seq_id: u32) -> Self {
        Self {
            prompt_seq: seq_id,
            gen_seq: None,
            prompt_len: 0,
            stable_len: u32::MAX,
            n_past: 0,
            recent_tokens: Vec::new(),
            prompt_tokens: Vec::new(),
            utf8: crate::llm::Utf8Assembler::new(),
            retained: false,
            last_used: Instant::now(),
        }
    }

    /// Onde a próxima decodificação escreve: a cópia de geração quando o turno
    /// já está gerando, a sequência do prompt enquanto ele ainda entra.
    pub(super) fn seq_id(&self) -> u32 {
        self.gen_seq.unwrap_or(self.prompt_seq)
    }

    pub(super) fn feed_token_piece(&mut self, piece: &[u8]) -> String {
        self.utf8.feed(piece)
    }

    pub(super) fn finish_token_stream(&mut self) -> String {
        self.utf8.flush()
    }
}

#[cfg(test)]
mod session_text_tests {
    use super::*;

    /// Reproduz o caminho de emissão que alimenta o delta do chat: duas peças
    /// de token chegam em chamadas distintas, mas o consumidor precisa ver o
    /// caractere inteiro, não dois U+FFFD criados por conversões independentes.
    #[test]
    fn streaming_token_pieces_reach_the_consumer_as_one_utf8_character() {
        let mut session = Session::new(0);

        let mut emitted = String::new();
        emitted.push_str(&session.feed_token_piece(b"caf\xc3"));
        emitted.push_str(&session.feed_token_piece(b"\xa9"));

        assert_eq!(emitted, "café");
    }

    /// O fim da sessão precisa drenar um prefixo truncado uma única vez.
    #[test]
    fn ending_stream_flushes_pending_bytes_without_leaking_them() {
        let mut session = Session::new(0);

        assert_eq!(session.feed_token_piece(b"ok\xc3"), "ok");
        assert_eq!(session.finish_token_stream(), "\u{fffd}");
        assert_eq!(session.finish_token_stream(), "");
    }

    /// Duas requisições intercaladas não podem compartilhar o rabicho de UTF-8.
    #[test]
    fn concurrent_sessions_keep_pending_utf8_isolated() {
        let mut first = Session::new(0);
        let mut second = Session::new(1);

        assert_eq!(first.feed_token_piece(b"Jo\xc3"), "Jo");
        assert_eq!(second.feed_token_piece(b"\xf0\x9f"), "");
        assert_eq!(first.feed_token_piece(b"\xa3o"), "ão");
        assert_eq!(second.feed_token_piece(b"\x98\x80"), "😀");
    }
}

// ── Snapshots de KV cache (`--slot-save-path`) ───────────────────────────────

/// Versão do arquivo sidecar. Sobe quando o formato do `.json` muda de forma
/// incompatível; o `.bin` é o formato do próprio llama.cpp (`LLAMA_STATE_SEQ_*`).
pub(super) const SLOT_SIDECAR_VERSION: u32 = 1;

/// Metadados que acompanham cada `<name>.bin`, gravados ao lado como
/// `<name>.json`. O `.bin` do llama.cpp valida a geometria de KV (contagem de
/// camadas, tipo e tamanho de linha) ao restaurar, mas **não** valida o modelo
/// nem a faixa de camadas; é o que este sidecar cobre antes de tocar no motor.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct SlotSidecar {
    pub(super) format_version: u32,
    /// Rótulo técnico do modelo. Igual em todos os peers da implantação.
    pub(super) model_label: String,
    /// Caminho do GGUF neste peer (uma fatia, ou o modelo inteiro).
    pub(super) model_path: String,
    pub(super) start_layer: u32,
    pub(super) end_layer: u32,
    pub(super) total_layers: u32,
    /// Largura do hidden state do motor carregado.
    pub(super) n_embd: u32,
    /// Quantos tokens de prompt o snapshot cobre (a fronteira estável retida,
    /// `Session::prompt_len`). É a posição em que a sessão restaurada fica.
    pub(super) retained_tokens: u32,
    /// Tokens reais do prompt quando este peer os conhece (a cabeça); vazio nos
    /// hops que só viram hidden state. O `.bin` carrega um cabeçalho de tokens
    /// próprio (consultivo), então este campo serve só ao reuso de prefixo.
    #[serde(default)]
    pub(super) prompt_tokens: Vec<i32>,
    /// RFC3339 de quando o snapshot foi gravado.
    pub(super) saved_at: String,
    /// Tamanho do `.bin` em bytes, como o llama.cpp reportou.
    pub(super) bin_bytes: u64,
}

/// Recusa um nome de snapshot que não seja um único componente de caminho
/// seguro. Mesma disciplina de `transfer::safe_model_path`: sem separador, sem
/// `..`, sem nome vazio ou oculto. O gateway compõe `"<nome>__<node_id>"`, e o
/// `node_id` é hex de hash, então o conjunto `[A-Za-z0-9._-]` basta.
pub(super) fn sanitize_slot_name(name: &str) -> Result<&str> {
    let name = name.trim();
    if name.is_empty() {
        bail!("nome de snapshot vazio");
    }
    if name.len() > 200 {
        bail!("nome de snapshot longo demais (máx 200)");
    }
    if name.starts_with('.') {
        bail!("nome de snapshot não pode começar com '.'");
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        bail!("nome de snapshot só aceita letras, dígitos, '.', '_' e '-'");
    }
    Ok(name)
}

pub(super) fn slot_bin_path(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    dir.join(format!("{name}.bin"))
}

pub(super) fn slot_json_path(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    dir.join(format!("{name}.json"))
}

/// Apaga o `.bin` e o `.json` de um snapshot. Devolve `true` quando algo foi
/// removido; um snapshot ausente não é erro (mesma ética de
/// `/v1/session/release`).
pub(super) fn erase_slot(dir: &std::path::Path, name: &str) -> Result<bool> {
    let name = sanitize_slot_name(name)?;
    let mut removed = false;
    for path in [slot_bin_path(dir, name), slot_json_path(dir, name)] {
        match std::fs::remove_file(&path) {
            Ok(()) => removed = true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(anyhow::anyhow!("apagar {}: {error}", path.display())),
        }
    }
    Ok(removed)
}

/// Conta sidecars sem abrir o conteúdo. /health é consultado a cada
/// requisição de geração e não pode segurar o mutex do motor enquanto lê e
/// interpreta cada JSON.
pub(super) fn count_slot_entries(dir: &std::path::Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| {
            let path = entry.path();
            path.is_file()
                && path.extension().and_then(|e| e.to_str()) == Some("json")
                && path.file_stem().and_then(|s| s.to_str()) != Some("prefix-auto-index")
        })
        .count()
}

/// Resultado de `InferState::restore_slot`, ecoado na resposta da rota para o
/// gateway conferir que todos os hops restauraram a mesma fronteira.
pub(super) struct SlotRestoreOutcome {
    pub(super) restored_tokens: u32,
}

#[cfg(test)]
mod slot_name_tests {
    use super::sanitize_slot_name;

    #[test]
    fn accepts_a_plain_component_and_the_gateway_node_namespacing() {
        assert_eq!(sanitize_slot_name("demo").unwrap(), "demo");
        assert_eq!(
            sanitize_slot_name("demo__a1b2c3d4e5f6").unwrap(),
            "demo__a1b2c3d4e5f6"
        );
        assert_eq!(sanitize_slot_name("  trimmed  ").unwrap(), "trimmed");
    }

    #[test]
    fn refuses_path_traversal_and_separators() {
        for bad in [
            "",
            ".",
            "..",
            "a/b",
            "a\\b",
            "../escape",
            ".hidden",
            "a b",
            "sÿm",
        ] {
            assert!(sanitize_slot_name(bad).is_err(), "esperava recusar {bad:?}");
        }
    }
}

#[cfg(test)]
mod slot_count_tests {
    use super::count_slot_entries;

    #[test]
    fn slot_count_usa_somente_entradas_do_diretorio() {
        let dir = std::env::temp_dir().join(format!("pom-slot-count-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).expect("diretório temporário");
        std::fs::write(dir.join("quebrado.json"), b"conteudo que nao e json")
            .expect("sidecar corrompido");
        std::fs::write(dir.join("prefix-auto-index.json"), b"indice").expect("índice automático");
        std::fs::write(dir.join("nao-e-slot.txt"), b"outro arquivo").expect("arquivo extra");

        let count = count_slot_entries(&dir);
        std::fs::remove_dir_all(&dir).expect("limpeza do diretório temporário");

        assert_eq!(count, 1);
    }
}

// ── Shared state ──────────────────────────────────────────────────────────────

pub(crate) struct InferState {
    pub(super) engine: LlmEngine,
    pub(super) moe: Option<Arc<crate::moe::runtime::MoeRuntime>>,
    pub(super) moe_fallback_reason: Option<String>,
    pub(super) sessions: HashMap<String, Session>,
    pub(super) seq_pool: Vec<u32>,
    pub(super) max_sessions: u32,
    pub(super) sampling: SamplingConfig,
    pub(super) no_thinking: bool,
    /// Whether to apply the model's chat template before tokenising.
    pub(super) chat_template: bool,
    pub(super) start_layer: u32,
    pub(super) end_layer: u32,
    /// Total de camadas da implantação (`NodeConfig::total_layers`). Guardado
    /// para validar um snapshot de KV antes de restaurá-lo.
    pub(super) total_layers: u32,
    /// Rótulo técnico do modelo servido, compartilhado por todos os peers da
    /// mesma implantação. Identidade do snapshot de KV.
    pub(super) model_label: String,
    /// Caminho do GGUF carregado neste peer. Identidade do snapshot de KV.
    pub(super) model_path: String,
    /// Diretório de snapshots de KV cache (`Config::slot_save_path`), ou `None`
    /// quando o recurso está desligado nesta máquina. Ver `save_slot`.
    pub(super) slot_dir: Option<std::path::PathBuf>,
    /// Acordado sempre que um slot de sequência pode ter ficado livre (fim de
    /// sessão, retenção, coleta de ociosidade, despejo). O `run_scheduler`
    /// espera nele quando o item da frente é um prefill de sessão nova e não
    /// há slot: a requisição fica na fila FIFO em vez de ser recusada com
    /// "seq pool esgotado".
    pub(super) slot_freed: Arc<tokio::sync::Notify>,
    pub(super) original_block_count: Option<u32>,
    pub(super) inference_path: crate::executor::InferencePath,
    /// Corte de confiança do rascunho MTP — ver `NodeConfig::mtp_min_probability`.
    pub(super) mtp_min_probability: f32,
    /// Duração do último handshake TLS aceito por este servidor, em ms.
    ///
    /// Medida no laço de accept (`serve.rs`), que é onde o `TlsAcceptor` roda e
    /// o tempo é exato. Sai daqui para o `meta` da resposta, e de lá para a
    /// telemetria por salto do gateway. `None` quando o nó não tem TLS.
    pub(super) tls_handshake_ms: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    /// Identidade TP deste nó. O rank 0 precisa da lista de membros para pôr
    /// os workers no mesmo decode; sem isso a barreira nunca fecha.
    pub(super) tp: Option<crate::network::TpIdentity>,
}

pub(crate) type SharedState = Arc<Mutex<InferState>>;

pub(super) fn use_chat_template(requested: Option<bool>, configured_default: bool) -> bool {
    requested.unwrap_or(configured_default)
}

pub(super) fn record_terminal_engine(
    timings: &mut NodeTimings,
    path: crate::executor::InferencePath,
    moe_forwarded: bool,
) {
    if path.is_moe() && moe_forwarded {
        timings.record_engine(crate::executor::ExecutionEngine::Moe);
    } else {
        timings.record_engine(crate::executor::ExecutionEngine::LlamaCpp);
    }
}

/// Anota no meta o que este peer **executou** com o prefixo retido.
///
/// Só o pedaço 0 carrega a decisão, porque só ele pode reusar. Os hops
/// seguintes obedecem a este campo em vez do pedido original do gateway: quem
/// tokeniza é quem sabe se o prefixo servia, e um hop reusando enquanto a
/// cabeça recomeçou (ou o contrário) desalinha as posições de todo o pipeline.
pub(super) fn apply_prefix_decision(
    meta: &mut Value,
    chunk_index: u32,
    prefix: PrefixDecision,
    requested_reuse: bool,
    requested_cached_tokens: u32,
    requested_prefix_match_tokens: u32,
    owns_tokens: bool,
) {
    if chunk_index != 0 {
        return;
    }
    if owns_tokens {
        meta["reuse_prefix"] = json!(prefix.reused);
        meta["cached_tokens"] = json!(prefix.cached_tokens);
        meta["prefix_match_tokens"] = json!(prefix.matched_tokens);
    } else {
        // Só a cabeça pode decidir se os tokens batem. Todos os hops
        // intermediários devem transportar a decisão autoritativa recebida;
        // recalculá-la como `false` faria o hop seguinte resetar a sessão e
        // depois receber um hidden state com posição positiva.
        meta["reuse_prefix"] = json!(requested_reuse);
        meta["cached_tokens"] = json!(requested_cached_tokens);
        meta["prefix_match_tokens"] = json!(requested_prefix_match_tokens);
    }
}

#[cfg(test)]
mod prefix_decision_tests {
    use super::*;

    #[tokio::test]
    async fn direct_forward_waits_for_the_previous_chunk() {
        let order = DirectForwardOrder::default();
        let first = order.wait_turn("prefill-1", 0).await.unwrap();
        let waiter_order = order.clone();
        let second = tokio::spawn(async move {
            waiter_order.wait_turn("prefill-1", 1).await.map(|turn| {
                turn.finish(true, true);
                1
            })
        });

        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        assert!(!second.is_finished());
        first.finish(true, false);
        assert_eq!(second.await.unwrap().unwrap(), 1);
    }

    #[tokio::test]
    async fn direct_forward_failure_reaches_the_next_chunk() {
        let order = DirectForwardOrder::default();
        let first = order.wait_turn("prefill-2", 0).await.unwrap();
        let waiter_order = order.clone();
        let second = tokio::spawn(async move { waiter_order.wait_turn("prefill-2", 1).await });

        tokio::task::yield_now().await;
        first.finish(false, false);
        let error = match second.await.unwrap() {
            Ok(_) => panic!("o chunk seguinte não pode ultrapassar uma falha"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("repasse direto falhou"));
    }

    #[test]
    fn an_intermediate_hop_echoes_the_head_decision() {
        let mut meta = json!({
            "reuse_prefix": false,
            "cached_tokens": 0,
        });

        apply_prefix_decision(&mut meta, 0, PrefixDecision::default(), true, 42, 17, false);

        assert_eq!(meta["reuse_prefix"], true);
        assert_eq!(meta["cached_tokens"], 42);
        assert_eq!(meta["prefix_match_tokens"], 17);
    }

    #[test]
    fn the_token_owner_reports_what_it_executed() {
        let mut meta = json!({
            "reuse_prefix": true,
            "cached_tokens": 99,
        });

        apply_prefix_decision(
            &mut meta,
            0,
            PrefixDecision {
                reused: false,
                cached_tokens: 0,
                matched_tokens: 0,
            },
            true,
            42,
            0,
            true,
        );

        assert_eq!(meta["reuse_prefix"], false);
        assert_eq!(meta["cached_tokens"], 0);
    }
}

/// Anota no meta a fronteira medida por quem tokenizou, para os hops seguintes
/// partirem o prompt na mesma posição. Ver `Session::stable_len`.
pub(super) fn apply_stable_len(meta: &mut Value, stable_len: u32) {
    if stable_len != u32::MAX {
        meta["stable_len"] = json!(stable_len);
    }
}

pub(super) fn partial_prefill_meta(
    more_chunks: bool,
    n_prompt_tokens: usize,
    start_pos: u32,
    next_pos: u32,
) -> Value {
    json!({
        "done": false,
        "more_chunks": more_chunks,
        "n_prompt_tokens": n_prompt_tokens,
        "start_pos": start_pos,
        "next_pos": next_pos,
    })
}

pub(super) fn terminal_prefill_meta(
    n_prompt_tokens: usize,
    start_pos: u32,
    next_pos: u32,
    done: bool,
) -> Value {
    json!({
        "done": done,
        "more_chunks": false,
        "n_prompt_tokens": n_prompt_tokens,
        "start_pos": start_pos,
        "next_pos": next_pos,
    })
}

/// The inference listener is API-key-only. Unknown keys are checked against
/// the principal through the authenticated node-to-node admin channel.
fn inference_token_from_header(value: &str) -> Option<&str> {
    let value = value.trim();
    let mut parts = value.splitn(2, char::is_whitespace);
    let scheme = parts.next()?;
    let token = if scheme.eq_ignore_ascii_case("bearer") {
        parts.next()?.trim()
    } else {
        value
    };
    (!token.is_empty()).then_some(token)
}

pub(super) async fn require_inference_auth(
    State((config, database, network)): State<(
        crate::auth::AuthState,
        Option<Arc<crate::db::PomDb>>,
        Option<Arc<crate::network::Network>>,
    )>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let tokens = inference_credential_candidates(request.headers());

    if tokens.is_empty() {
        return (StatusCode::UNAUTHORIZED, "missing inference credential").into_response();
    }

    let session_id = request
        .headers()
        .get(crate::auth::INFERENCE_SESSION_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty());

    for token in tokens {
        if inference_credential_is_valid(&config, database.as_ref(), &token).await {
            if let Some(session_id) = session_id {
                crate::infer_server::http_client().remember_inference_token(session_id, &token);
            }
            return next.run(request).await;
        }
        if let (Some(network), Some(database)) = (network.as_ref(), database.as_ref()) {
            if let Ok(Some(key)) = crate::admin::validate_api_key_remote_parts(
                network,
                crate::infer_server::http_client(),
                &config,
                &token,
            )
            .await
            {
                let origin = if key.origin_node_id.is_empty() {
                    &key.node_id
                } else {
                    &key.origin_node_id
                };
                if config
                    .read()
                    .await
                    .blocked_nodes
                    .iter()
                    .any(|blocked| blocked == origin)
                {
                    continue;
                }
                if key.active && key.key == token && database.insert_api_key(&key).is_ok() {
                    let mut cfg = config.write().await;
                    cfg.api_keys.retain(|existing| existing.id != key.id);
                    cfg.api_keys.push(key);
                    if let Some(session_id) = session_id {
                        crate::infer_server::http_client()
                            .remember_inference_token(session_id, &token);
                    }
                    return next.run(request).await;
                }
            }
        }
    }
    (StatusCode::FORBIDDEN, "invalid inference API key").into_response()
}

/// A regra de credencial da porta de inferência, isolada do middleware porque
/// o canal persistente (`/v1/link`) precisa **da mesma regra por quadro**.
///
/// Uma conexão longa que autenticasse só na abertura manteria válida por horas
/// uma API key revogada por outro processo - exatamente o que a revalidação por
/// requisição existe para impedir. O custo é o mesmo de hoje: tráfego entre
/// peers carrega a API key da sessão e passa pelo banco local.
pub(crate) async fn inference_credential_is_valid(
    config: &crate::auth::AuthState,
    database: Option<&Arc<crate::db::PomDb>>,
    token: &str,
) -> bool {
    match database {
        Some(database) => {
            let Some(key) = database.api_key_for_token(token).ok().flatten() else {
                return false;
            };
            let cfg = config.read().await;
            let origin = if key.origin_node_id.is_empty() {
                &key.node_id
            } else {
                &key.origin_node_id
            };
            key.active && !cfg.blocked_nodes.iter().any(|blocked| blocked == origin)
        }
        None => crate::auth::is_valid_inference_token(&*config.read().await, token),
    }
}

/// Return all API-key candidates in the same order as the middleware. Headers
/// remain independent: a stale Authorization value cannot hide a valid
/// `x-api-key` value.
pub(super) fn inference_credential_candidates(headers: &axum::http::HeaderMap) -> Vec<String> {
    let mut tokens = Vec::with_capacity(2);
    if let Some(token) = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(inference_token_from_header)
    {
        tokens.push(token.to_string());
    }
    if let Some(token) = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .and_then(inference_token_from_header)
        .filter(|token| !tokens.iter().any(|candidate| candidate == token))
    {
        tokens.push(token.to_string());
    }
    tokens
}
