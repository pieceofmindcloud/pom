use super::*;

pub(super) fn model_metadata_string(model: *const sys::llama_model, key: &str) -> Option<String> {
    let key = CString::new(key).ok()?;
    let mut buf = vec![0u8; 32768];
    let n = unsafe {
        sys::llama_model_meta_val_str(model, key.as_ptr(), buf.as_mut_ptr() as *mut i8, buf.len())
    };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// O lado dominante dos buffers `context` + `compute` de um contexto MTP.
/// Buffers host pequenos de staging não transformam um contexto cujo KV e
/// grafo estão na VRAM em um recuo para RAM; quando a maioria fica no host, o
/// operador precisa ver RAM no painel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MtpAllocation {
    Vram,
    Ram,
}

impl MtpAllocation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Vram => "vram",
            Self::Ram => "ram",
        }
    }
}

pub fn mtp_allocation_from_context_memory(value: &serde_json::Value) -> Option<MtpAllocation> {
    let mut vram_bytes = 0u64;
    let mut ram_bytes = 0u64;
    for buffer in value.get("buffers")?.as_array()? {
        let bytes = buffer
            .get("context")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
            .saturating_add(
                buffer
                    .get("compute")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
            );
        if bytes == 0 {
            continue;
        }
        let is_host = buffer
            .get("host")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or_else(|| {
                buffer
                    .get("device")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|device| device.eq_ignore_ascii_case("CPU"))
            });
        if is_host {
            ram_bytes = ram_bytes.saturating_add(bytes);
        } else {
            vram_bytes = vram_bytes.saturating_add(bytes);
        }
    }
    match (vram_bytes, ram_bytes) {
        (0, 0) => None,
        (vram, ram) if ram > vram => Some(MtpAllocation::Ram),
        _ => Some(MtpAllocation::Vram),
    }
}

pub fn context_allocation(ctx: *const sys::llama_context) -> Option<MtpAllocation> {
    let value = unsafe { json_from_wrapper(|out| sys::llama_rs_context_memory_json(ctx, out)) }?;
    mtp_allocation_from_context_memory(&value)
}

// ── Configuration ─────────────────────────────────────────────────────────────

pub const MTP_NO_NEXTN_REASON: &str = "o modelo carregado não traz o bloco nextn";

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Projector multimodal opcional, carregado como o `--mmproj` do llama.cpp.
    pub mmproj_path: Option<String>,
    /// Capacidade total do cache KV. O runtime já multiplica a janela pública
    /// por sessão por `max_sessions` antes de montar esta configuração.
    pub n_ctx: u32,
    pub offload_kqv: bool,
    pub cache_type_k: KvCacheType,
    pub cache_type_v: KvCacheType,
    pub fit_memory: bool,
    /// VRAM que fica livre após a projeção do backend, por dispositivo.
    /// Processos atuais já ficam fora do snapshot de memória livre do backend;
    /// este campo é somente a reserva de segurança configurável.
    pub fit_memory_margin_mb: u32,
    pub n_batch: u32,
    pub n_gpu_layers: i32,
    pub n_threads: i32,
    pub max_sessions: u32,
    /// True for non-last peers that need to hand the residual to the next peer.
    pub need_embeddings: bool,
    /// Ligar a predição multi-token pelo bloco `nextn` do modelo, quando ele
    /// tiver um. Distinto da especulação por lookup (`find_draft`) - ver
    /// `crate::mtp`. Um modelo sem `nextn` não é erro de configuração: o
    /// contexto informa essa ausência para a telemetria e segue sem MTP.
    pub mtp: bool,
    /// Permite recuar o bloco `nextn` para pesos e cache na RAM se a tentativa
    /// com a VRAM falhar. Só é lido quando `mtp` está ligado.
    pub mtp_cpu_fallback_enabled: bool,
    /// Quantos snapshots de estado recorrente manter por sequência
    /// (`llama_context_params::n_rs_seq`), para permitir desfazer um
    /// rascunho especulativo recusado num modelo híbrido/recorrente
    /// (Qwen3.5+: `llm_arch_supports_rs_rollback` no llama.cpp). `0` é o
    /// padrão do llama.cpp - sem isto, `llama_memory_recurrent::seq_rm`
    /// nunca aceita um rollback parcial, e vale a pena pedir um valor maior
    /// que a maior largura de lote especulativo configurada (`speculative_k`,
    /// `mtp_draft_tokens + 1`); arquitetura sem suporte simplesmente zera o
    /// pedido de volta (`llama-context.cpp`), então pedir aqui não machuca
    /// modelo comum. Ver `infer_server::should_draft` e
    /// `gateway::rollback_sessions`, que são o resto do mecanismo.
    pub rs_seq_window: u32,
    /// Load routed MoE expert metadata without allocating expert weights in llama.cpp.
    pub moe_external_executor: bool,
    /// Per-layer ownership passed to the generic llama.cpp hook. Zero keeps
    /// the native experts, one delegates the whole layer to the device cache.
    pub moe_external_executor_layers: Vec<u8>,
}

pub fn apply_cache_types(
    cparams: &mut sys::llama_context_params,
    cache_type_k: KvCacheType,
    cache_type_v: KvCacheType,
) {
    cparams.type_k = cache_type_to_ggml(cache_type_k);
    cparams.type_v = cache_type_to_ggml(cache_type_v);
}

pub(super) fn cache_type_to_ggml(cache_type: KvCacheType) -> sys::ggml_type {
    match cache_type {
        KvCacheType::F32 => sys::GGML_TYPE_F32,
        KvCacheType::F16 => sys::GGML_TYPE_F16,
        KvCacheType::Bf16 => sys::GGML_TYPE_BF16,
        KvCacheType::Q8_0 => sys::GGML_TYPE_Q8_0,
        KvCacheType::Q4_0 => sys::GGML_TYPE_Q4_0,
        KvCacheType::Q4_1 => sys::GGML_TYPE_Q4_1,
        KvCacheType::Iq4Nl => sys::GGML_TYPE_IQ4_NL,
        KvCacheType::Q5_0 => sys::GGML_TYPE_Q5_0,
        KvCacheType::Q5_1 => sys::GGML_TYPE_Q5_1,
    }
}

// ── Engine ────────────────────────────────────────────────────────────────────

/// Os pesos, carregados uma vez e compartilhados por todos os contextos.
///
/// Separar isto do contexto é o que torna o paralelismo viável sem duplicar
/// memória: o que pesa são os pesos (gigabytes), e o llama.cpp os trata como
/// somente-leitura depois da carga - vários `llama_context` podem apontar para
/// o mesmo `llama_model`. O contexto carrega o cache KV; o `n_ctx` interno já
/// é a capacidade total necessária para todas as sequências.
pub struct LoadedModel {
    pub(super) model: *mut sys::llama_model,
    pub(super) vocab: *const sys::llama_vocab,
    pub n_embd: usize,
    pub n_vocab: usize,
    /// Whether this model instance has the optional `nextn` tensors loaded.
    /// `llama_model_n_layer_nextn` only reports metadata and remains positive
    /// when the caller deliberately loaded the target with `load_mtp=false`.
    pub(super) mtp_weights_loaded: bool,
}

// SAFETY: depois da carga o `llama_model` é somente-leitura; é o próprio
// llama.cpp que permite vários contextos sobre um modelo.
unsafe impl Send for LoadedModel {}
unsafe impl Sync for LoadedModel {}

/// Primeiro palpite para o buffer do template de chat. Não é limite: quando o
/// llama.cpp informa que precisa de mais, `apply_chat_template` realoca com o
/// tamanho exato e renderiza de novo.
pub(super) const CHAT_TEMPLATE_INITIAL_BUF: usize = 65536;

/// Quantas sequências de KV o contexto precisa reservar para `max_sessions`
/// sessões simultâneas.
///
/// **Duas por sessão**: uma guarda o prompt e ninguém escreve nela; a outra
/// recebe a geração do turno e é apagada inteira no fim (ver
/// `LlmEngine::kv_copy_seq` e `infer_server::InferState::open_generation`). O
/// par é fixo - a sequência de geração da sessão que ocupa `seq` é
/// `seq + max_sessions` -, então nunca falta slot de geração para uma sessão
/// que já tem slot de prompt.
///
/// Vem sempre acompanhado de `kv_unified = true`: sem ele o llama.cpp fatia
/// `n_ctx` por sequência (`n_ctx_seq = n_ctx / n_seq_max`) e dobrar as
/// sequências cortaria a janela de prompt pela metade. Unificado, as duas
/// sequências dividem as mesmas células, a janela continua sendo `n_ctx`
/// inteiro e a cópia prompt→geração não move tensor nenhum.
///
/// **O que isto custa, declarado.** Duas famílias de cache não compartilham
/// células e passam a reservar de verdade duas fatias por sessão:
///
/// - o estado recorrente (`llama_memory_recurrent`, dimensionado por
///   `n_seq_max`), porque estado de prompt e estado de geração são estados
///   diferentes - não há como um modelo Mamba/GDN reter o prompt sem guardar o
///   estado dele em separado;
/// - o cache de estados com múltiplos fluxos, que hoje ignora o pedido de
///   unificação (`unified_raw = false`) e mantém um stream por sequência.
///
/// Em ambos os casos o `fit_memory` (`llama_rs_fit_params`, que recebe estes
/// mesmos `cparams`) dimensiona o contexto já contando as duas sequências, e
/// `/health` publica `n_seq_max` para o operador conferir. A alternativa seria
/// oferecer o cache só a alguns modelos, que é exatamente o que a decisão de
/// produto recusou.
pub fn kv_seq_budget(max_sessions: u32) -> u32 {
    max_sessions.max(1).saturating_mul(2)
}

pub(super) const DEFAULT_GPU_LAYERS: i32 = 99;
pub(super) const FIT_MEMORY_MIN_MARGIN_BYTES: usize = 4 * 1024 * 1024;
pub(super) const FIT_MIN_CONTEXT: u32 = 4096;
/// The CUDA graph and kernel workspaces scale with the microbatch, while the
/// public `n_batch` can stay large enough to accept a prompt chunk. A smaller
/// ubatch is the safe automatic fallback before llama.cpp reserves those
/// workspaces on a nearly-full GPU.
pub(super) const FIT_MAX_UBATCH: u32 = 512;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FitDeviceMemory {
    pub(super) total_bytes: usize,
    pub(super) free_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FitMemorySnapshot {
    pub(super) total_bytes: usize,
    pub(super) free_bytes: usize,
    pub(super) margin_bytes: usize,
}

pub(super) fn fit_memory_configured_margin_bytes(configured_margin_mb: u32) -> usize {
    usize::try_from(u64::from(configured_margin_mb) * 1024 * 1024)
        .unwrap_or(usize::MAX)
        .max(FIT_MEMORY_MIN_MARGIN_BYTES)
}

/// Returns the amount that `common_fit_params` should leave unused on one
/// device. The backend's `free_bytes` is already the memory available to POM:
/// CUDA obtains it from `cudaMemGetInfo`, so allocations owned by other
/// processes are already excluded. The margin therefore covers only allocator
/// and late-workspace drift, plus a process that starts after this snapshot.
///
/// When the card has less free memory than the requested reserve, using the
/// whole free amount would make every positive allocation fail the fit. A
/// smaller reserve is still useful until the card cannot even provide the
/// minimum safety gap; at that point zero is the only honest target.
pub(super) fn fit_memory_margin_bytes(memory: FitDeviceMemory, configured_margin_mb: u32) -> usize {
    let free_bytes = memory.free_bytes.min(memory.total_bytes);
    if free_bytes < FIT_MEMORY_MIN_MARGIN_BYTES {
        return 0;
    }
    free_bytes.min(fit_memory_configured_margin_bytes(configured_margin_mb))
}

pub(super) fn fit_memory_margins(
    memories: &[FitDeviceMemory],
    n_devices: usize,
    configured_margin_mb: u32,
) -> Vec<usize> {
    let fallback = fit_memory_configured_margin_bytes(configured_margin_mb);
    (0..n_devices)
        .map(|index| {
            memories
                .get(index)
                .map(|memory| fit_memory_margin_bytes(*memory, configured_margin_mb))
                .unwrap_or(fallback)
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MtpFitMode {
    Disabled,
    Vram,
    Ram,
}

pub(super) fn mtp_fit_mode_for_model(
    requested: MtpFitMode,
    has_nextn: bool,
    architecture: Option<&str>,
) -> (MtpFitMode, Option<String>) {
    if requested == MtpFitMode::Disabled || !has_nextn {
        return (mtp_mode_for_nextn_availability(requested, has_nextn), None);
    }

    if let Some(reason) = crate::mtp::mtp_graph_unavailable_reason(architecture) {
        return (MtpFitMode::Disabled, Some(reason));
    }

    (requested, None)
}

pub(super) fn mtp_mode_for_nextn_availability(
    requested: MtpFitMode,
    has_nextn: bool,
) -> MtpFitMode {
    if !has_nextn {
        MtpFitMode::Disabled
    } else {
        requested
    }
}

/// O fit conjunto só pode habilitar o MTP se conservar o encaixe integral que
/// o modelo principal obteve sozinho. `common_fit_params` usa a mesma contagem
/// para as camadas principais e as camadas `nextn`; aceitar um resultado
/// reduzido aqui faria o sufixo MTP empurrar camadas principais para a CPU.
pub(super) fn choose_mtp_fit_mode(
    requested: MtpFitMode,
    primary_gpu_layers: i32,
    mtp_gpu_layers: Option<i32>,
) -> MtpFitMode {
    match requested {
        MtpFitMode::Disabled => MtpFitMode::Disabled,
        MtpFitMode::Vram if primary_gpu_layers == -1 && mtp_gpu_layers == Some(-1) => {
            MtpFitMode::Vram
        }
        MtpFitMode::Vram | MtpFitMode::Ram => MtpFitMode::Ram,
    }
}

pub(super) fn mtp_mode_without_gpu_fit(requested: MtpFitMode) -> MtpFitMode {
    match requested {
        MtpFitMode::Disabled => MtpFitMode::Disabled,
        MtpFitMode::Vram | MtpFitMode::Ram => MtpFitMode::Ram,
    }
}

pub(super) fn fit_memory_margins_with_mtp(
    margins: &[usize],
    mtp_cost: &[usize],
    mode: MtpFitMode,
) -> Vec<usize> {
    margins
        .iter()
        .enumerate()
        .map(|(index, &margin)| {
            if mode == MtpFitMode::Vram {
                margin.saturating_add(mtp_cost.get(index).copied().unwrap_or(0))
            } else {
                margin
            }
        })
        .collect()
}

/// Calcula apenas o cache KV do contexto MTP, sem abrir o modelo nem montar
/// seu grafo. O espaço de compute é deliberadamente deixado para a tentativa
/// real de `llama_init_from_model`, que pode recuar para a RAM se não couber.
/// Como o bloco nextn fica no fim, sua reserva vai para o último dispositivo
/// na ordem slow-to-fast usada pelo fit.
pub(super) fn estimate_mtp_vram_costs(
    info: &crate::gguf::GgufInfo,
    cfg: &EngineConfig,
    n_devices: usize,
) -> Option<Vec<usize>> {
    let mut costs = vec![0usize; n_devices];
    if !cfg.offload_kqv || n_devices == 0 {
        return Some(costs);
    }

    let block_count = info.block_count?;
    let last_layer = block_count.checked_sub(1)?;
    let nextn_layers = info.nextn_predict_layers.unwrap_or(1).max(1);
    let first_nextn_layer = block_count.checked_sub(nextn_layers)?;
    let geometry = info
        .kv_geometry_for_range(first_nextn_layer, last_layer)
        .ok()?;
    let bytes = crate::kv_budget::kv_bytes_for_layers(
        u64::from(cfg.n_ctx),
        u64::from(kv_seq_budget(cfg.max_sessions)),
        &geometry,
        cfg.cache_type_k,
        cfg.cache_type_v,
    )
    .ok()?;
    costs[n_devices - 1] = usize::try_from(bytes).ok()?;
    Some(costs)
}

/// Quantas GPUs físicas o fit deve considerar.
///
/// `max_devices` é `llama_max_devices()`, a **capacidade de compilação** dos
/// vetores que o wrapper C preenche (16), nunca o número de placas presentes.
/// Confundir os dois fazia o aviso de snapshot parcial disparar em toda máquina
/// com menos de 16 GPUs, e - o defeito real - fazia a reserva de KV do contexto
/// MTP cair no índice 15, um dispositivo inexistente, em vez da última placa
/// verdadeira. O fit então acreditava ter VRAM livre que já estava prometida.
///
/// A contagem elegível vem de `llama_rs_get_devices`; quando ela é vazia, vale
/// a contagem do snapshot de memória. Sem nenhuma das duas resta um slot para
/// o host, porque o fit sempre planeja ao menos um destino.
pub(super) fn fit_device_count(
    available_count: usize,
    memory_count: usize,
    max_devices: usize,
) -> usize {
    let counted = if available_count > 0 {
        available_count
    } else {
        memory_count
    };
    counted.clamp(1, max_devices.max(1))
}

pub(super) fn fit_memory_snapshots(
    memories: &[FitDeviceMemory],
    margins: &[usize],
) -> Vec<FitMemorySnapshot> {
    margins
        .iter()
        .enumerate()
        .map(|(index, &margin_bytes)| {
            let memory = memories.get(index).copied().unwrap_or(FitDeviceMemory {
                total_bytes: 0,
                free_bytes: 0,
            });
            FitMemorySnapshot {
                total_bytes: memory.total_bytes,
                free_bytes: memory.free_bytes,
                margin_bytes,
            }
        })
        .collect()
}

#[cfg(test)]
mod fit_memory_margin_tests {
    use super::*;

    const MIB: usize = 1024 * 1024;

    fn memory(total_mib: usize, free_mib: usize) -> FitDeviceMemory {
        FitDeviceMemory {
            total_bytes: total_mib * MIB,
            free_bytes: free_mib * MIB,
        }
    }

    #[test]
    fn sem_outros_processos_reserva_o_padrao_pequeno() {
        assert_eq!(
            fit_memory_margin_bytes(memory(16 * 1024, 16 * 1024), 256),
            256 * MIB
        );
    }

    #[test]
    fn processos_existentes_nao_sao_descontados_duas_vezes() {
        assert_eq!(
            fit_memory_margin_bytes(memory(16 * 1024, 4 * 1024), 256),
            256 * MIB
        );
    }

    #[test]
    fn memoria_livre_menor_que_o_pedido_reduz_a_reserva() {
        assert_eq!(
            fit_memory_margin_bytes(memory(16 * 1024, 64), 256),
            64 * MIB
        );
    }

    #[test]
    fn memoria_insuficiente_ate_para_o_minimo_nao_faz_underflow() {
        assert_eq!(fit_memory_margin_bytes(memory(16 * 1024, 2), 256), 0);
    }

    #[test]
    fn margens_sao_calculadas_independentemente_para_cada_dispositivo() {
        let margins = fit_memory_margins(
            &[
                memory(16 * 1024, 16 * 1024),
                memory(24 * 1024, 64),
                memory(8 * 1024, 2),
            ],
            3,
            256,
        );

        assert_eq!(margins, vec![256 * MIB, 64 * MIB, 0]);
    }

    #[test]
    fn snapshot_do_fit_preserva_memoria_e_orcamento_por_dispositivo() {
        let memories = vec![memory(16 * 1024, 15 * 1024), memory(24 * 1024, 20 * 1024)];
        let margins = fit_memory_margins(&memories, memories.len(), 256);

        assert_eq!(
            fit_memory_snapshots(&memories, &margins),
            vec![
                FitMemorySnapshot {
                    total_bytes: 16 * 1024 * MIB,
                    free_bytes: 15 * 1024 * MIB,
                    margin_bytes: 256 * MIB,
                },
                FitMemorySnapshot {
                    total_bytes: 24 * 1024 * MIB,
                    free_bytes: 20 * 1024 * MIB,
                    margin_bytes: 256 * MIB,
                },
            ]
        );
    }

    #[test]
    fn fit_usa_a_contagem_dos_dispositivos_elegiveis_e_nao_o_limite_generico() {
        assert_eq!(fit_device_count(2, 0, 16), 2);
        assert_eq!(fit_device_count(0, 2, 16), 2);
        assert_eq!(fit_device_count(0, 0, 16), 1);
    }

    #[test]
    fn arquivo_sem_nextn_nao_pede_carga_nem_contexto_mtp() {
        assert_eq!(
            mtp_mode_for_nextn_availability(MtpFitMode::Vram, false),
            MtpFitMode::Disabled
        );
        assert_eq!(
            mtp_mode_for_nextn_availability(MtpFitMode::Ram, false),
            MtpFitMode::Disabled
        );
        assert_eq!(
            mtp_mode_for_nextn_availability(MtpFitMode::Vram, true),
            MtpFitMode::Vram
        );
    }

    #[test]
    fn arquitetura_com_nextn_sem_grafo_desliga_o_fit_mtp_com_motivo() {
        let (mode, reason) = mtp_fit_mode_for_model(MtpFitMode::Vram, true, Some("graniteswitch"));

        assert_eq!(mode, MtpFitMode::Disabled);
        assert_eq!(
            reason.as_deref(),
            Some("a arquitetura graniteswitch não possui grafo MTP suportado pelo llama.cpp")
        );
    }

    #[test]
    fn arquitetura_com_grafo_mtp_mantem_a_tentativa_em_vram() {
        assert_eq!(
            mtp_fit_mode_for_model(MtpFitMode::Vram, true, Some("qwen35moe")),
            (MtpFitMode::Vram, None)
        );
    }

    #[test]
    fn reserva_mtp_analitica_calcula_so_o_kv_e_coloca_no_ultimo_dispositivo() {
        let info = crate::gguf::GgufInfo {
            block_count: Some(41),
            nextn_predict_layers: Some(1),
            attention_head_count_kv: Some(vec![2]),
            attention_key_length: Some(vec![64]),
            attention_value_length: Some(vec![64]),
            ..Default::default()
        };
        let cfg = EngineConfig {
            n_ctx: 4096,
            max_sessions: 1,
            offload_kqv: true,
            cache_type_k: crate::KvCacheType::F16,
            cache_type_v: crate::KvCacheType::F16,
            fit_memory: true,
            fit_memory_margin_mb: 256,
            n_batch: 128,
            n_gpu_layers: 99,
            n_threads: 0,
            need_embeddings: false,
            mtp: true,
            mtp_cpu_fallback_enabled: true,
            rs_seq_window: 0,
            moe_external_executor: false,
            moe_external_executor_layers: Vec::new(),
            mmproj_path: None,
        };

        assert_eq!(
            estimate_mtp_vram_costs(&info, &cfg, 2),
            Some(vec![0, 2 * 4096 * 2 * 64 * 2 * 2])
        );
    }

    /// Regressão do defeito de 2026-09-08 na ksia: com `llama_max_devices()`
    /// (16) no lugar da contagem física, a reserva de KV do contexto MTP caía
    /// no índice 15 - um dispositivo que não existe - e as duas RTX ficavam
    /// com margem zero para ele. O fit então planejava sobre VRAM já
    /// prometida e o decode abortava na segunda placa.
    #[test]
    fn reserva_mtp_vai_para_a_ultima_placa_real_e_nao_para_o_limite_de_compilacao() {
        let info = crate::gguf::GgufInfo {
            block_count: Some(41),
            nextn_predict_layers: Some(1),
            attention_head_count_kv: Some(vec![2]),
            attention_key_length: Some(vec![64]),
            attention_value_length: Some(vec![64]),
            ..Default::default()
        };
        let cfg = EngineConfig {
            n_ctx: 4096,
            max_sessions: 1,
            offload_kqv: true,
            cache_type_k: crate::KvCacheType::F16,
            cache_type_v: crate::KvCacheType::F16,
            fit_memory: true,
            fit_memory_margin_mb: 256,
            n_batch: 128,
            n_gpu_layers: 99,
            n_threads: 0,
            need_embeddings: false,
            mtp: true,
            mtp_cpu_fallback_enabled: true,
            rs_seq_window: 0,
            moe_external_executor: false,
            moe_external_executor_layers: Vec::new(),
            mmproj_path: None,
        };

        let physical = fit_device_count(2, 2, 16);
        let costs = estimate_mtp_vram_costs(&info, &cfg, physical)
            .expect("o custo do contexto MTP precisa ser estimável");

        assert_eq!(costs.len(), 2, "só existem duas placas nesta máquina");
        assert!(costs[1] > 0, "a reserva pertence à última placa real");

        // O que acontecia antes: um vetor de 16 posições cuja reserva o C
        // nunca leria, porque ele só indexa os dispositivos que existem.
        let inflado = estimate_mtp_vram_costs(&info, &cfg, 16)
            .expect("o custo do contexto MTP precisa ser estimável");
        assert_eq!(inflado[1], 0, "a última placa real ficava sem reserva");
    }

    #[test]
    fn pedido_mtp_reserva_o_custo_do_segundo_contexto_em_cada_dispositivo() {
        let margins = vec![256 * MIB, 256 * MIB];
        let mtp_cost = vec![727 * MIB, 311 * MIB];

        assert_eq!(
            fit_memory_margins_with_mtp(&margins, &mtp_cost, MtpFitMode::Vram),
            vec![983 * MIB, 567 * MIB]
        );
    }

    #[test]
    fn mtp_apertado_nao_pode_reduzir_camadas_principais() {
        // Reproduz a inversão de 27fda8b: o fit principal cabe inteiro, mas
        // o fit conjunto só cabe se remover camadas. Nesse caso o MTP cede.
        assert_eq!(
            choose_mtp_fit_mode(MtpFitMode::Vram, -1, Some(50)),
            MtpFitMode::Ram
        );
    }

    #[test]
    fn mtp_em_vram_so_e_aceito_quando_o_fit_conjunto_tambem_cabe() {
        assert_eq!(
            choose_mtp_fit_mode(MtpFitMode::Vram, -1, Some(-1)),
            MtpFitMode::Vram
        );
        assert_eq!(
            choose_mtp_fit_mode(MtpFitMode::Vram, -1, None),
            MtpFitMode::Ram
        );
    }

    #[test]
    fn principal_reduzido_nao_da_direito_ao_mtp_na_vram() {
        assert_eq!(
            choose_mtp_fit_mode(MtpFitMode::Vram, 50, Some(-1)),
            MtpFitMode::Ram
        );
    }

    #[test]
    fn mtp_desligado_mantem_a_distribuicao_do_fit_principal() {
        assert_eq!(
            choose_mtp_fit_mode(MtpFitMode::Disabled, 50, Some(50)),
            MtpFitMode::Disabled
        );
    }

    #[test]
    fn mtp_desmarcado_nao_reserva_orcamento() {
        let margins = vec![256 * MIB, 256 * MIB];
        let mtp_cost = vec![727 * MIB, 311 * MIB];

        assert_eq!(
            fit_memory_margins_with_mtp(&margins, &mtp_cost, MtpFitMode::Disabled),
            margins
        );
    }

    #[test]
    fn mtp_desmarcado_nao_ativa_o_preflight_da_arquitetura() {
        assert_eq!(
            mtp_fit_mode_for_model(MtpFitMode::Disabled, true, Some("graniteswitch")),
            (MtpFitMode::Disabled, None)
        );
    }
}

/// Parâmetros do modelo já ajustados pelo `common_fit_params`.
///
/// Os vetores ficam vivos até `llama_model_load_from_file` consumir os
/// ponteiros guardados em `llama_model_params`. Isso é necessário no caso de
/// mais de uma GPU local, em que o fit também escreve `tensor_split` e
/// `tensor_buft_overrides`.
pub(super) struct FitPlan {
    pub(super) config: EngineConfig,
    pub(super) tensor_split: Vec<f32>,
    pub(super) tensor_buft_overrides: Vec<sys::llama_model_tensor_buft_override>,
    /// A NULL-terminated device list, kept alive for the model load. It is
    /// empty when speed could not be ranked confidently, preserving llama's
    /// default device selection.
    pub(super) devices: Vec<sys::ggml_backend_dev_t>,
    /// Whether the target model should load its optional `nextn` weights in
    /// VRAM. `Ram` keeps those weights out of the target so the CPU companion
    /// can own them without changing the primary placement.
    pub(super) mtp_mode: MtpFitMode,
    /// Razão para manter o MTP desligado quando a metadata anuncia `nextn`,
    /// mas o fork não possui um grafo seguro para a arquitetura.
    pub(super) mtp_unavailable_reason: Option<String>,
    pub(super) applied: bool,
}

pub(super) fn fit_can_adjust_layers(cfg: &EngineConfig) -> bool {
    cfg.fit_memory && cfg.n_gpu_layers == DEFAULT_GPU_LAYERS
}

pub(super) fn empty_fit_plan(cfg: &EngineConfig) -> FitPlan {
    let mtp_mode = if cfg.mtp {
        MtpFitMode::Vram
    } else {
        MtpFitMode::Disabled
    };
    empty_fit_plan_with_mtp_mode(cfg, mtp_mode)
}

pub(super) fn empty_fit_plan_with_mtp_mode(cfg: &EngineConfig, mtp_mode: MtpFitMode) -> FitPlan {
    empty_fit_plan_with_mtp_mode_and_reason(cfg, mtp_mode, None)
}

pub(super) fn empty_fit_plan_with_mtp_mode_and_reason(
    cfg: &EngineConfig,
    mtp_mode: MtpFitMode,
    mtp_unavailable_reason: Option<String>,
) -> FitPlan {
    FitPlan {
        config: cfg.clone(),
        tensor_split: Vec::new(),
        tensor_buft_overrides: Vec::new(),
        devices: Vec::new(),
        mtp_mode,
        mtp_unavailable_reason,
        applied: false,
    }
}

/// O POM exporta o residual pelo callback, mas o llama.cpp também pode tentar
/// copiar o resultado de embeddings. Essa cópia usa `n_embd_out` como se fosse
/// a largura real do tensor; quando as duas larguras divergem, o pedido pode
/// ultrapassar `ggml_nbytes(tensor)` antes de o callback ser consumido.
pub(super) fn generic_embeddings_compatible(
    need_embeddings: bool,
    n_embd: usize,
    n_embd_out: usize,
) -> bool {
    need_embeddings && n_embd == n_embd_out
}

pub(super) fn backend_registry_name(device: sys::ggml_backend_dev_t) -> Option<String> {
    if device.is_null() {
        return None;
    }

    let registry = unsafe { sys::ggml_backend_dev_backend_reg(device) };
    if registry.is_null() {
        return None;
    }

    let name = unsafe { sys::ggml_backend_reg_name(registry) };
    if name.is_null() {
        return None;
    }

    unsafe { CStr::from_ptr(name).to_str().ok().map(str::to_owned) }
}

pub fn effective_n_ubatch(cfg: &EngineConfig) -> u32 {
    let n_batch = cfg.n_batch.max(1);
    if cfg.fit_memory {
        n_batch.min(FIT_MAX_UBATCH)
    } else {
        n_batch
    }
}

/// Aplica a política única de lote público e microlote de computação a cada
/// contexto que o motor cria, inclusive o contexto MTP.
pub fn apply_context_batch_params(cparams: &mut sys::llama_context_params, cfg: &EngineConfig) {
    cparams.n_batch = cfg.n_batch;
    cparams.n_ubatch = effective_n_ubatch(cfg);
}

pub(super) fn fit_plan(
    model_path: &str,
    cfg: &EngineConfig,
    mtp_mode: MtpFitMode,
) -> Result<FitPlan> {
    let (mtp_mode, mtp_unavailable_reason) = if mtp_mode == MtpFitMode::Disabled {
        (MtpFitMode::Disabled, None)
    } else {
        let path = std::path::Path::new(model_path);
        let has_nextn = crate::gguf::has_nextn_tensors(path).unwrap_or(false);
        let architecture = crate::gguf::read_info(path)
            .ok()
            .and_then(|info| info.architecture);
        let (mode, reason) = mtp_fit_mode_for_model(mtp_mode, has_nextn, architecture.as_deref());
        if !has_nextn {
            tracing::info!(
                model = %model_path,
                "MTP indisponível: o arquivo não traz tensores nextn"
            );
        } else if let Some(reason) = reason.as_deref() {
            warn!(model = %model_path, reason, "MTP indisponível para a arquitetura do modelo");
        }
        (mode, reason)
    };

    if !fit_can_adjust_layers(cfg) {
        if cfg.fit_memory && cfg.n_gpu_layers != DEFAULT_GPU_LAYERS {
            warn!(
                "llama.cpp fit ignorado porque n_gpu_layers foi definido explicitamente como {}",
                cfg.n_gpu_layers
            );
        }
        return Ok(empty_fit_plan_with_mtp_mode_and_reason(
            cfg,
            mtp_mode_without_gpu_fit(mtp_mode),
            mtp_unavailable_reason,
        ));
    }

    let path = CString::new(model_path)
        .with_context(|| format!("invalid model path for llama.cpp fit: {model_path}"))?;
    // `common_fit_params` queries the backend devices and their free memory.
    // The call is idempotent and mirrors `LoadedModel::load`, which also keeps
    // the initialization close to every possible model-loading entry point.
    unsafe { sys::llama_backend_init() };
    crate::sysmon::mark_backend_initialized();
    let n_devices = unsafe { sys::llama_max_devices() }.max(1);
    let n_overrides = unsafe { sys::llama_max_tensor_buft_overrides() }.max(1);

    // Take the memory snapshot before the speed probes allocate their small
    // benchmark graph. This preserves the fit snapshot timing guarantee and
    // makes the measured order orthogonal to the memory budget.
    let mut available_devices = vec![ptr::null_mut(); n_devices];
    let available_count = unsafe {
        sys::llama_rs_get_devices(available_devices.as_mut_ptr(), available_devices.len())
    };
    available_devices.truncate(available_count.min(n_devices));
    let mut device_memories = vec![
        sys::llama_rs_device_memory {
            free_bytes: 0,
            total_bytes: 0,
        };
        n_devices
    ];
    let memory_count = if available_devices.is_empty() {
        // Keep the old wrapper path for an empty device registry so a missing
        // snapshot continues to take the existing safe fit fallback.
        let count = unsafe {
            sys::llama_rs_get_device_memory(device_memories.as_mut_ptr(), device_memories.len())
        };
        device_memories.truncate(count.min(n_devices));
        count
    } else {
        for (memory, &device) in device_memories.iter_mut().zip(&available_devices) {
            let mut free_bytes = 0;
            let mut total_bytes = 0;
            unsafe { sys::ggml_backend_dev_memory(device, &mut free_bytes, &mut total_bytes) };
            memory.free_bytes = free_bytes;
            memory.total_bytes = total_bytes;
        }
        device_memories.truncate(available_devices.len());
        available_devices.len()
    };

    let memory_snapshot_complete = device_memories
        .iter()
        .all(|memory| memory.total_bytes > 0 && memory.free_bytes <= memory.total_bytes);
    let speed_scores = if available_devices.len() > 1 && memory_snapshot_complete {
        available_devices
            .iter()
            .map(|&device| {
                let score = unsafe { sys::llama_rs_benchmark_device(device) };
                if score.is_finite() && score > 0.0 {
                    Some(f64::from(score))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let backend_names = available_devices
        .iter()
        .map(|&device| backend_registry_name(device))
        .collect::<Vec<_>>();
    let backend_name_refs = backend_names
        .iter()
        .map(Option::as_deref)
        .collect::<Vec<_>>();
    let ordered_devices = device_placement::confident_slow_to_fast_order_for_backends(
        &speed_scores,
        &backend_name_refs,
    )
    .filter(|order| {
        order
            .iter()
            .enumerate()
            .any(|(position, &original)| position != original)
    })
    .map(|order| {
        let mut devices = order
            .iter()
            .map(|&index| available_devices[index])
            .collect::<Vec<_>>();
        let reordered_memories = order
            .iter()
            .map(|&index| device_memories[index])
            .collect::<Vec<_>>();
        device_memories = reordered_memories;
        devices.push(ptr::null_mut());
        devices
    })
    .unwrap_or_default();
    if available_devices.len() > 1 {
        tracing::info!(
            device_backend_names = ?backend_names,
            device_speed_scores = ?speed_scores,
            speed_order_applied = !ordered_devices.is_empty(),
            "llama.cpp fit sondou a velocidade relativa dos dispositivos"
        );
    }
    let fit_memories = device_memories
        .iter()
        .map(|memory| FitDeviceMemory {
            total_bytes: memory.total_bytes,
            free_bytes: memory.free_bytes,
        })
        .collect::<Vec<_>>();
    // Os vetores entregues ao C ficam com a capacidade de compilação, que o
    // wrapper preenche até o número de placas que ele mesmo enxerga; as
    // decisões físicas usam a contagem real. Ver `fit_device_count`.
    let physical_devices = fit_device_count(available_devices.len(), memory_count, n_devices);
    let mut margins = fit_memory_margins(&fit_memories, n_devices, cfg.fit_memory_margin_mb);
    if memory_count != physical_devices {
        warn!(
            reported_devices = memory_count,
            expected_devices = physical_devices,
            "llama.cpp fit recebeu um snapshot parcial de memória"
        );
    }
    let snapshots = fit_memory_snapshots(&fit_memories, &margins);
    let process_gpu_memory_mb = crate::sysmon::current_process_gpu_memory_mb();
    tracing::info!(
        process_id = std::process::id(),
        process_gpu_memory_mb = process_gpu_memory_mb.unwrap_or(0.0),
        process_gpu_memory_known = process_gpu_memory_mb.is_some(),
        "llama.cpp fit memória do processo no ponto da sondagem"
    );
    for (device, snapshot) in snapshots.iter().enumerate() {
        tracing::info!(
            device,
            total_bytes = snapshot.total_bytes,
            free_bytes = snapshot.free_bytes,
            margin_bytes = snapshot.margin_bytes,
            budget_bytes = snapshot.free_bytes.saturating_sub(snapshot.margin_bytes),
            "llama.cpp fit memória e orçamento por dispositivo"
        );
    }
    let make_model_params = |load_mtp: bool| {
        let mut params = unsafe { sys::llama_model_default_params() };
        params.n_gpu_layers = -1;
        if !ordered_devices.is_empty() {
            // `ordered_devices` includes the NULL sentinel required by
            // llama.cpp. Keep it in FitPlan so the same order reaches the real
            // model load.
            params.devices = ordered_devices.as_ptr() as *mut sys::ggml_backend_dev_t;
        }
        params.load_mtp = load_mtp;
        params.moe_external_executor = cfg.moe_external_executor;
        if !cfg.moe_external_executor_layers.is_empty() {
            params.moe_external_executor_layers = cfg.moe_external_executor_layers.as_ptr();
            params.moe_external_executor_layer_count = cfg.moe_external_executor_layers.len();
        }
        params
    };
    let make_context_params = || {
        let mut params = unsafe { sys::llama_context_default_params() };
        params.n_ctx = cfg.n_ctx;
        apply_context_batch_params(&mut params, cfg);
        params.n_seq_max = kv_seq_budget(cfg.max_sessions);
        params.kv_unified = true;
        params.offload_kqv = cfg.offload_kqv;
        apply_cache_types(&mut params, cfg.cache_type_k, cfg.cache_type_v);
        params.embeddings = cfg.need_embeddings;
        params.n_rs_seq = cfg.rs_seq_window;
        params
    };

    // First fit only the work that every token must execute. The MTP pass below
    // is allowed to reserve its own context only after this plan fits intact.
    let mut primary_tensor_split = vec![0.0_f32; n_devices];
    let mut primary_tensor_buft_overrides = vec![
        sys::llama_model_tensor_buft_override {
            pattern: ptr::null(),
            buft: ptr::null_mut(),
        };
        n_overrides
    ];
    let mut primary_mparams = make_model_params(false);
    let mut primary_cparams = make_context_params();
    let primary_status = unsafe {
        sys::llama_rs_fit_params(
            path.as_ptr(),
            &mut primary_mparams,
            &mut primary_cparams,
            primary_tensor_split.as_mut_ptr(),
            primary_tensor_buft_overrides.as_mut_ptr(),
            margins.as_mut_ptr(),
            FIT_MIN_CONTEXT,
            sys::GGML_LOG_LEVEL_WARN,
        )
    };

    if primary_status != 0 {
        warn!(
            "llama.cpp fit não encontrou uma configuração automática (status {primary_status}); usando os parâmetros solicitados sem MTP na GPU"
        );
        return Ok(empty_fit_plan_with_mtp_mode_and_reason(
            cfg,
            mtp_mode_without_gpu_fit(mtp_mode),
            mtp_unavailable_reason,
        ));
    }

    let primary_gpu_layers = primary_mparams.n_gpu_layers;
    let mut selected_mtp_mode = choose_mtp_fit_mode(mtp_mode, primary_gpu_layers, None);
    let mut selected_n_gpu_layers = primary_mparams.n_gpu_layers;
    let mut selected_n_ctx = primary_cparams.n_ctx;
    let mut selected_tensor_split = primary_tensor_split;
    let mut selected_tensor_buft_overrides = primary_tensor_buft_overrides;

    if mtp_mode == MtpFitMode::Vram && primary_gpu_layers == -1 {
        let mut mtp_mparams = make_model_params(true);
        let mtp_cparams = make_context_params();
        let mut joint_margins = match crate::gguf::read_info(std::path::Path::new(model_path))
            .ok()
            .and_then(|info| estimate_mtp_vram_costs(&info, cfg, physical_devices))
        {
            Some(mtp_cost) => {
                tracing::info!(
                    mtp_vram_cost_estimate = ?mtp_cost,
                    "llama.cpp fit reservou o KV estimado do contexto MTP por dispositivo"
                );
                fit_memory_margins_with_mtp(&margins, &mtp_cost, MtpFitMode::Vram)
            }
            None => {
                warn!(
                    "não foi possível estimar o KV do contexto MTP; o grafo não será sondado e o recuo para RAM ficará disponível se a GPU falhar"
                );
                margins.clone()
            }
        };
        let mut mtp_tensor_split = vec![0.0_f32; n_devices];
        let mut mtp_tensor_buft_overrides = vec![
            sys::llama_model_tensor_buft_override {
                pattern: ptr::null(),
                buft: ptr::null_mut(),
            };
            n_overrides
        ];
        let mut mtp_cparams = mtp_cparams;
        let mtp_status = unsafe {
            sys::llama_rs_fit_params(
                path.as_ptr(),
                &mut mtp_mparams,
                &mut mtp_cparams,
                mtp_tensor_split.as_mut_ptr(),
                mtp_tensor_buft_overrides.as_mut_ptr(),
                joint_margins.as_mut_ptr(),
                FIT_MIN_CONTEXT,
                sys::GGML_LOG_LEVEL_WARN,
            )
        };
        let joint_gpu_layers = (mtp_status == 0).then_some(mtp_mparams.n_gpu_layers);
        selected_mtp_mode = choose_mtp_fit_mode(mtp_mode, primary_gpu_layers, joint_gpu_layers);
        if selected_mtp_mode == MtpFitMode::Vram {
            selected_n_gpu_layers = mtp_mparams.n_gpu_layers;
            selected_n_ctx = mtp_cparams.n_ctx;
            selected_tensor_split = mtp_tensor_split;
            selected_tensor_buft_overrides = mtp_tensor_buft_overrides;
        } else {
            tracing::info!(
                primary_gpu_layers,
                joint_gpu_layers = ?joint_gpu_layers,
                "llama.cpp fit recusou a reserva MTP para preservar as camadas principais"
            );
        }
    } else if mtp_mode == MtpFitMode::Vram {
        tracing::info!(
            primary_gpu_layers,
            "llama.cpp fit manteve o MTP fora da VRAM porque o encaixe principal já precisou reduzir camadas"
        );
    }

    let mut effective = cfg.clone();
    effective.n_gpu_layers = selected_n_gpu_layers;
    effective.n_ctx = selected_n_ctx;

    tracing::info!(
        requested_gpu_layers = cfg.n_gpu_layers,
        effective_gpu_layers = effective.n_gpu_layers,
        requested_context = cfg.n_ctx,
        effective_context = effective.n_ctx,
        mtp_fit_mode = ?selected_mtp_mode,
        "llama.cpp fit ajustou os parâmetros de memória"
    );

    Ok(FitPlan {
        config: effective,
        tensor_split: selected_tensor_split,
        tensor_buft_overrides: selected_tensor_buft_overrides,
        devices: ordered_devices,
        mtp_mode: selected_mtp_mode,
        mtp_unavailable_reason,
        applied: true,
    })
}
