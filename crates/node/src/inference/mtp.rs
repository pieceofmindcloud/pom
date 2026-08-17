//! Predição multi-token (MTP) pelo bloco `nextn` do próprio modelo.
//!
//! # O que isto é, e o que **não** é
//!
//! O nó já tinha uma "decodificação especulativa": `infer_server::find_draft`,
//! que procura no texto já gerado uma repetição do próprio final e chuta que a
//! continuação se repete. É *prompt-lookup decoding* - uma busca em array que
//! nunca toca os pesos - e por isso só acelera saída repetitiva. Num prompt
//! comum ela não acha nada, o rascunho sai vazio e a geração custa exatamente o
//! que custava.
//!
//! MTP é outro mecanismo. Modelos como o Qwen3.5/3.8 (`qwen35`, `qwen35moe`)
//! trazem **dentro do GGUF** um bloco extra treinado, o `nextn`, que recebe o
//! estado oculto do tronco na posição `p` junto com o embedding do token da
//! posição `p+1` e prevê o token de `p+2`. É uma cabeça de rascunho de
//! verdade, treinada com o modelo, e é ela que o LM Studio liga quando oferece
//! "MTP". Os dois são independentes e ligam por chaves separadas
//! (`speculative_decoding_enabled` e `mtp_enabled`): reaproveitar um
//! interruptor faria "ligar especulação" significar duas coisas diferentes.
//!
//! # O que muda na resposta: nada
//!
//! Rascunho é aposta sobre **o que entra no lote**, nunca sobre o que sai. A
//! verificação em `infer_server::step_blocking` amostra cada posição de novo,
//! pelo mesmo `sample_token_profiled` de sempre, e só aceita o prefixo que
//! bateu. Um rascunho ruim custa aceitação (velocidade); nunca correção. É por
//! isso que dá para rodar com uma única cabeça `nextn` mesmo num modelo que
//! traga várias treinadas: o pior caso é um lote desperdiçado.
//!
//! # Onde o bloco `nextn` mora, e por que isso limita o cluster
//!
//! O bloco vive na **última** camada do modelo (`blk.<n-1>.nextn.*`), então
//! num modelo fatiado ele está na fatia final - a cauda do anel. Só a cauda
//! consegue rascunhar, e o rascunho tem que chegar à cabeça, que é quem monta
//! o lote. Hoje, portanto, o MTP só entra em ação quando **uma máquina serve o
//! modelo inteiro** (cabeça e cauda são o mesmo processo, e o rascunho não
//! atravessa rede nenhuma). Num pipeline de N nós o motor da cabeça não tem o
//! bloco, `MtpDrafter::attach` devolve `None` e o nó simplesmente não anuncia
//! a capacidade. Ver `docs/subsystems/inferencia.md` para a conta do que
//! custaria estender ao caso fatiado.
//!
//! # De onde veio o algoritmo
//!
//! De `llama.cpp/common/speculative.cpp`, `common_speculative_impl_draft_mtp`
//! (o driver que o `llama-server` usa). Aqui está portado o caminho de uma
//! sequência, uma cabeça e memória não compartilhada - qwen35/qwen35moe. Os
//! outros dois modos do upstream (`chain_heads` do step35, `is_mem_shared` do
//! gemma4) não estão: um modelo com várias cabeças roda com a primeira, o que
//! é um rascunho mais fraco e nunca uma resposta errada.

use anyhow::{bail, Result};
use std::ptr;

use llama_cpp_sys_2 as sys;

// ── As entradas NextN que o bindgen não emite ────────────────────────────────
//
// Ver `llama_mtp_shim.cpp`: elas moram em `llama-ext.h`, fora do `extern "C"`
// de `llama.h`, e o `wrapper.h` da crate `-sys` não inclui esse cabeçalho.
extern "C" {
    fn pom_llama_set_embeddings_nextn(
        ctx: *mut sys::llama_context,
        value: bool,
        masked: bool,
    );
    fn pom_llama_get_embeddings_nextn_ith(
        ctx: *mut sys::llama_context,
        i: std::os::raw::c_int,
    ) -> *mut f32;
}

/// A cabeça de rascunho `nextn`, com o contexto MTP e o estado que atravessa
/// as rodadas.
///
/// O contexto de rascunho é criado **sobre os mesmos pesos** do alvo
/// (`llama_init_from_model` com `ctx_type = LLAMA_CONTEXT_TYPE_MTP`), então
/// não custa um segundo modelo na memória: o que ele acrescenta é um cache KV
/// de **uma** camada, que é o tamanho do bloco `nextn`.
pub struct MtpDrafter {
    /// O contexto que roda o grafo `DECODER_MTP`. Dono: liberado no `Drop`.
    ctx: *mut sys::llama_context,
    n_embd: usize,
    n_vocab: usize,

    /// O `h` que a próxima chamada precisa parear com o primeiro token dela.
    ///
    /// MTP na posição `p+1` consome o par `(h_p, x_{p+1})`, e `h_p` nasce no
    /// decode **anterior**. Sem guardar essa linha, todo lote começaria com um
    /// `h` errado (ou zerado) na primeira posição.
    pending_h: Vec<f32>,

    /// As linhas de `h` do último lote do alvo, uma por posição decodificada.
    /// É daqui que `accept` escolhe o novo `pending_h` depois de saber quantos
    /// tokens do rascunho passaram na verificação.
    verify_h: Vec<f32>,
    verify_rows: usize,
}

// SAFETY: o `llama_context` é usado sob o mesmo `Mutex` que serializa o
// contexto alvo em `infer_server::SharedState` - nunca por duas threads ao
// mesmo tempo. É a mesma promessa que `LlmEngine` já faz.
unsafe impl Send for MtpDrafter {}

impl Drop for MtpDrafter {
    fn drop(&mut self) {
        unsafe {
            if !self.ctx.is_null() {
                sys::llama_free(self.ctx);
            }
        }
    }
}

/// Quantas cabeças `nextn` este modelo traz. Zero significa "sem MTP" - é o
/// caso de todo modelo sem o bloco e de **toda fatia não-final** de um modelo
/// que o tenha (ver `splitter::split`, que zera `nextn_predict_layers` fora da
/// fatia final justamente porque o bloco não viaja com ela).
pub fn nextn_layers(model: *mut sys::llama_model) -> i32 {
    unsafe { sys::llama_model_n_layer_nextn(model as *const _) }
}

impl MtpDrafter {
    /// Monta o contexto de rascunho sobre um modelo já carregado.
    ///
    /// `Ok(None)` - e não erro - quando o modelo não tem bloco `nextn`: é a
    /// resposta certa para a fatia do meio de um pipeline e para qualquer
    /// modelo sem MTP, e quem chama trata como "este nó não anuncia MTP".
    ///
    /// `ctx_tgt` é o contexto alvo, que passa a emitir o estado oculto do
    /// tronco (`llama_set_embeddings_nextn`, sem máscara: para **toda**
    /// posição do lote, não só as com logits). Sem isso não há `h` para
    /// alimentar a cabeça.
    pub fn attach(
        model: *mut sys::llama_model,
        ctx_tgt: *mut sys::llama_context,
        cfg: &crate::llm::EngineConfig,
        n_embd: usize,
        n_vocab: usize,
    ) -> Result<Option<Self>> {
        if nextn_layers(model) <= 0 {
            return Ok(None);
        }

        // A janela e o lote acompanham o contexto alvo: o rascunho anda
        // posição a posição com ele, e um cache menor faria o `seq_rm` da
        // rodada seguinte apagar o que ainda vale.
        let mut cparams = unsafe { sys::llama_context_default_params() };
        cparams.n_ctx = cfg.n_ctx;
        cparams.n_batch = cfg.n_batch;
        cparams.n_ubatch = cfg.n_batch;
        cparams.n_seq_max = cfg.max_sessions;
        cparams.ctx_type = sys::LLAMA_CONTEXT_TYPE_MTP;
        // Sem instantâneos de estado recorrente: o bloco `nextn` é uma camada
        // de atenção comum, e o upstream zera este campo pelo mesmo motivo.
        cparams.n_rs_seq = 0;
        if cfg.n_threads > 0 {
            cparams.n_threads = cfg.n_threads;
            cparams.n_threads_batch = cfg.n_threads;
        }

        let ctx = unsafe { sys::llama_init_from_model(model, cparams) };
        if ctx.is_null() {
            bail!("llama_init_from_model falhou para o contexto MTP (ctx_type=MTP)");
        }

        unsafe {
            // Alvo: emite `h` para toda posição do lote (`masked = false`),
            // porque `observe` lê uma linha por posição decodificada.
            pom_llama_set_embeddings_nextn(ctx_tgt, true, false);
            // Rascunho: só nas posições com logits (`masked = true`) - são as
            // únicas cujo `h` alimenta o passo seguinte do rascunho.
            pom_llama_set_embeddings_nextn(ctx, true, true);
        }

        tracing::info!(
            "MTP pronto: {} cabeça(s) nextn, contexto de rascunho com n_ctx={}",
            nextn_layers(model),
            cfg.n_ctx
        );

        Ok(Some(MtpDrafter {
            ctx,
            n_embd,
            n_vocab,
            pending_h: vec![0.0; n_embd],
            verify_h: Vec::new(),
            verify_rows: 0,
        }))
    }

    /// Acompanha um lote que o contexto **alvo** acabou de decodificar.
    ///
    /// Faz duas coisas, nessa ordem:
    ///
    /// 1. Repete o lote no contexto de rascunho, deslocando o estado oculto uma
    ///    posição para a direita - a linha `k` recebe `h` da posição `k-1`, e a
    ///    linha 0 recebe o `pending_h` guardado da chamada anterior. É esse
    ///    pareamento `(h_p, x_{p+1})` que o bloco `nextn` foi treinado a
    ///    consumir. Sem logits em posição nenhuma: aqui só se quer o cache KV
    ///    da camada `nextn` em dia, e pedir logits pagaria a cabeça de saída
    ///    (150k de vocabulário) por token de prompt.
    /// 2. Guarda as linhas de `h` do alvo, que `accept` usará para escolher o
    ///    `pending_h` da próxima rodada.
    ///
    /// O `seq_rm` antes do decode não é zelo: a rodada anterior deixou no cache
    /// do rascunho as posições que ela **apostou**, e a verificação pode ter
    /// recusado parte delas. Reescrever por cima sem apagar deixaria duas
    /// células para a mesma posição, e a atenção leria as duas.
    pub fn observe(
        &mut self,
        ctx_tgt: *mut sys::llama_context,
        tokens: &[i32],
        pos_offset: u32,
        seq_id: u32,
    ) -> Result<()> {
        let n = tokens.len();
        if n == 0 {
            return Ok(());
        }

        // 1. Alcança o alvo no contexto de rascunho.
        unsafe {
            let mem = sys::llama_get_memory(self.ctx);
            if !mem.is_null() {
                sys::llama_memory_seq_rm(mem, seq_id as i32, pos_offset as i32, -1);
            }
        }

        // `llama_batch_init` aloca **ou** `token` **ou** `embd`, nunca os dois;
        // pedindo `embd` ele devolve `token` nulo. O MTP precisa dos dois (o
        // token entra pelo embedding do próprio bloco, o `h` vem do tronco),
        // então o vetor de tokens é nosso e o ponteiro é devolvido ao que era
        // antes de `llama_batch_free` - que libera `token` se ele não for
        // nulo, e liberar memória do Rust ali seria corrupção de heap.
        let mut batch = unsafe { sys::llama_batch_init(n as i32, self.n_embd as i32, 1) };
        let mut owned_tokens: Vec<i32> = tokens.to_vec();
        let restore_token = batch.token;
        batch.n_tokens = n as i32;
        batch.token = owned_tokens.as_mut_ptr();

        let rc = unsafe {
            for i in 0..n {
                *batch.pos.add(i) = pos_offset as i32 + i as i32;
                *batch.n_seq_id.add(i) = 1;
                *(*batch.seq_id.add(i)).add(0) = seq_id as i32;
                *batch.logits.add(i) = 0;
            }
            // Linha 0: o `h` que sobrou da chamada anterior. Linhas 1..n: o `h`
            // que o alvo acabou de produzir para as posições 0..n-1.
            ptr::copy_nonoverlapping(self.pending_h.as_ptr(), batch.embd, self.n_embd);
            let mut faltou = None;
            for i in 1..n {
                let src = pom_llama_get_embeddings_nextn_ith(ctx_tgt, (i - 1) as i32);
                if src.is_null() {
                    faltou = Some(i - 1);
                    break;
                }
                ptr::copy_nonoverlapping(src, batch.embd.add(i * self.n_embd), self.n_embd);
            }
            if let Some(i) = faltou {
                batch.token = restore_token;
                sys::llama_batch_free(batch);
                bail!("llama_get_embeddings_nextn_ith devolveu NULL para i={i}");
            }

            let rc = sys::llama_decode(self.ctx, batch);
            batch.token = restore_token;
            sys::llama_batch_free(batch);
            rc
        };
        if rc != 0 {
            bail!("llama_decode no contexto MTP falhou rc={rc}");
        }

        // 2. Guarda as linhas do alvo para a escolha de `accept`.
        self.verify_h.clear();
        self.verify_h.reserve(n * self.n_embd);
        for i in 0..n {
            let src = unsafe { pom_llama_get_embeddings_nextn_ith(ctx_tgt, i as i32) };
            if src.is_null() {
                self.verify_rows = 0;
                bail!("llama_get_embeddings_nextn_ith devolveu NULL para i={i}");
            }
            self.verify_h
                .extend_from_slice(unsafe { std::slice::from_raw_parts(src, self.n_embd) });
        }
        self.verify_rows = n;
        self.pending_h
            .copy_from_slice(&self.verify_h[(n - 1) * self.n_embd..n * self.n_embd]);
        Ok(())
    }

    /// Anota quantos tokens **do rascunho** a verificação aceitou, e move o
    /// `pending_h` para a linha certa.
    ///
    /// `accepted` é o `longest_match` da rodada, não o total emitido: o token
    /// emitido a mais é sempre a amostragem real da primeira posição não
    /// confirmada, e o `h` que a próxima rodada precisa é o da última posição
    /// que **entrou** no cache, que é `accepted`.
    pub fn accept(&mut self, accepted: usize) {
        if self.verify_rows == 0 {
            return;
        }
        let row = accepted.min(self.verify_rows - 1);
        self.pending_h
            .copy_from_slice(&self.verify_h[row * self.n_embd..(row + 1) * self.n_embd]);
    }

    /// Rascunha até `k` tokens a partir do último token confirmado.
    ///
    /// Um decode do bloco `nextn` por token rascunhado: o primeiro parte de
    /// `(pending_h, id_last)`, e cada seguinte reaproveita o `h` que o próprio
    /// bloco produziu. `p_min` corta o rascunho no primeiro token de que a
    /// cabeça não tem certeza - continuar depois disso só alarga o lote de
    /// todos os hops para uma aposta que a verificação vai recusar.
    pub fn draft(&mut self, id_last: i32, n_past: u32, seq_id: u32, k: u32, p_min: f32) -> Vec<i32> {
        if k == 0 {
            return Vec::new();
        }
        // O cache do rascunho pode ter sobras da rodada anterior nas posições
        // que a verificação recusou; ver `observe`.
        unsafe {
            let mem = sys::llama_get_memory(self.ctx);
            if !mem.is_null() {
                sys::llama_memory_seq_rm(mem, seq_id as i32, n_past as i32, -1);
            }
        }

        let mut out: Vec<i32> = Vec::with_capacity(k as usize);
        let mut token = id_last;
        let mut h: Vec<f32> = self.pending_h.clone();

        for i in 0..k {
            match self.step_once(token, n_past + i, seq_id, &h) {
                Some((id, p, next_h)) => {
                    if p < p_min {
                        break;
                    }
                    out.push(id);
                    token = id;
                    h = next_h;
                }
                None => break,
            }
        }
        out
    }

    /// Um decode do bloco `nextn`: devolve o token mais provável, a
    /// probabilidade dele e o `h` que alimenta o passo seguinte.
    fn step_once(
        &mut self,
        token: i32,
        pos: u32,
        seq_id: u32,
        h: &[f32],
    ) -> Option<(i32, f32, Vec<f32>)> {
        let mut batch = unsafe { sys::llama_batch_init(1, self.n_embd as i32, 1) };
        // Ver `observe`: o ponteiro de tokens é nosso e volta ao original
        // antes do `llama_batch_free`.
        let mut owned = [token];
        let restore_token = batch.token;
        batch.n_tokens = 1;
        batch.token = owned.as_mut_ptr();

        let rc = unsafe {
            *batch.pos = pos as i32;
            *batch.n_seq_id = 1;
            *(*batch.seq_id).add(0) = seq_id as i32;
            *batch.logits = 1;
            ptr::copy_nonoverlapping(h.as_ptr(), batch.embd, self.n_embd);
            let rc = sys::llama_decode(self.ctx, batch);
            batch.token = restore_token;
            sys::llama_batch_free(batch);
            rc
        };
        if rc != 0 {
            tracing::warn!("rascunho MTP: llama_decode falhou rc={rc}, seguindo sem rascunho");
            return None;
        }

        let logits = unsafe { sys::llama_get_logits_ith(self.ctx, 0) };
        if logits.is_null() {
            return None;
        }
        let logits = unsafe { std::slice::from_raw_parts(logits, self.n_vocab) };
        let (id, p) = argmax_with_probability(logits);

        let next_h = unsafe { pom_llama_get_embeddings_nextn_ith(self.ctx, 0) };
        if next_h.is_null() {
            return None;
        }
        let next_h = unsafe { std::slice::from_raw_parts(next_h, self.n_embd) }.to_vec();
        Some((id, p, next_h))
    }
}

/// O token mais provável e a probabilidade dele, num passo só sobre os logits.
///
/// A probabilidade sai de um softmax estável (subtrai o máximo antes de
/// exponenciar); sem isso um logit alto estoura em `inf` e a razão vira `NaN`,
/// que compararia `false` contra qualquer `p_min` e desligaria o corte em
/// silêncio.
fn argmax_with_probability(logits: &[f32]) -> (i32, f32) {
    if logits.is_empty() {
        return (0, 0.0);
    }
    let mut best = 0usize;
    let mut max = f32::NEG_INFINITY;
    for (i, &l) in logits.iter().enumerate() {
        if l > max {
            max = l;
            best = i;
        }
    }
    let sum: f32 = logits.iter().map(|&l| (l - max).exp()).sum();
    (best as i32, if sum > 0.0 { 1.0 / sum } else { 0.0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argmax_with_probability_finds_the_peak_and_its_share() {
        let (id, p) = argmax_with_probability(&[0.0, 2.0, 1.0]);
        assert_eq!(id, 1);
        // e^0 + e^2 + e^1 = 1 + 7.389 + 2.718 → e^2 / soma ≈ 0,665
        assert!((p - 0.665).abs() < 0.01, "p = {p}");
    }

    /// Logits grandes acontecem (um modelo confiante), e o softmax ingênuo
    /// devolveria `inf/inf = NaN` — que passaria por qualquer `p_min` sem
    /// ninguém notar, porque `NaN < x` é `false`.
    #[test]
    fn argmax_with_probability_survives_logits_that_would_overflow() {
        let (id, p) = argmax_with_probability(&[100.0, 200.0, 150.0]);
        assert_eq!(id, 1);
        assert!(p.is_finite() && (p - 1.0).abs() < 1e-6, "p = {p}");
    }

    #[test]
    fn argmax_with_probability_of_a_flat_distribution_is_one_over_n() {
        let (_, p) = argmax_with_probability(&[1.0, 1.0, 1.0, 1.0]);
        assert!((p - 0.25).abs() < 1e-6, "p = {p}");
    }

    /// Sonda ao vivo: carrega um GGUF **com** bloco `nextn` e exerce o ciclo
    /// inteiro - anexar, `observe` de um prompt, `draft`, `accept`.
    ///
    /// `#[ignore]` e dirigida por `POM_MTP_MODEL=<caminho.gguf>` porque
    /// precisa de um modelo em disco (o de referência é
    /// `unsloth/Qwen3.5-4B-MTP-GGUF`, 33 blocos com `nextn` em `blk.32`) e de
    /// backend de GPU compilado. Rode com:
    ///
    /// ```text
    /// POM_MTP_MODEL=/caminho/Qwen3.5-4B-Q4_K_M.gguf \
    ///   cargo test --release --features metal --bin pom \
    ///   mtp::tests::the_nextn_head_drafts_from_a_real_model -- --ignored --nocapture
    /// ```
    ///
    /// O que ela prova, e que nenhum teste de unidade prova: que os quatro
    /// símbolos da ponte C++ **linkam e executam**, que o contexto
    /// `ctx_type = MTP` sobe sobre os mesmos pesos, e que a cabeça devolve
    /// tokens plausíveis em vez de lixo. O que ela **não** prova é ganho de
    /// tok/s - isso é medição de ponta a ponta, e está pendente.
    #[test]
    #[ignore]
    fn the_nextn_head_drafts_from_a_real_model() {
        let Ok(path) = std::env::var("POM_MTP_MODEL") else {
            eprintln!("defina POM_MTP_MODEL para rodar esta sonda");
            return;
        };
        let cfg = crate::llm::EngineConfig {
            n_ctx: 4096,
            offload_kqv: true,
            n_batch: 512,
            n_gpu_layers: 99,
            n_threads: 0,
            max_sessions: 1,
            need_embeddings: false,
            mtp: true,
            rs_seq_window: 0,
        };
        let mut engine = crate::llm::LlmEngine::load(&path, &cfg).expect("carregar o modelo");
        assert!(
            engine.mtp_ready(),
            "o modelo em {path} não trouxe bloco nextn — a sonda precisa de um que traga"
        );

        let prompt = engine.tokenize("The capital of France is Paris. The capital of Italy is", true, false);
        assert!(prompt.len() > 4, "prompt curto demais para a sonda");
        engine
            .decode_tokens(&prompt, 0, 0, false)
            .expect("prefill do alvo (e o observe da cabeça, dentro dele)");

        // O último token confirmado é o próprio fim do prompt; a cabeça
        // rascunha a partir dele exatamente como `step_blocking` faria.
        let n_past = prompt.len() as u32;
        let seed = *prompt.last().unwrap();
        let draft = engine.mtp_draft(seed, n_past, 0, 3, 0.0);
        assert!(
            !draft.is_empty(),
            "a cabeça nextn não rascunhou nada — sem rascunho o MTP não acelera coisa nenhuma"
        );
        let texto: String = draft.iter().map(|&t| engine.token_to_piece(t, false)).collect();
        eprintln!("rascunho MTP: {draft:?} → {texto:?}");
        assert!(
            draft.iter().all(|&t| t >= 0 && (t as usize) < engine.n_vocab),
            "token fora do vocabulário: {draft:?}"
        );

        // Aceitar move o `pending_h`; a rodada seguinte tem que continuar
        // rascunhando em vez de degenerar.
        engine.mtp_accept(1);
        let de_novo = engine.mtp_draft(seed, n_past, 0, 3, 0.0);
        assert!(
            !de_novo.is_empty(),
            "a segunda rodada parou de rascunhar — o estado entre rodadas não sobreviveu"
        );
    }
}
