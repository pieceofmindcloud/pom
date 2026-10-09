//! A fatia que este nó servia, guardada para sobreviver a um reinício.
//!
//! A atribuição de modelo vive só em memória (é decisão de projeto: quem manda
//! é o plano de implantação, não um arquivo esquecido no disco). Uma queda,
//! porém, não é uma mudança de intenção — é a mesma intenção com o processo
//! novo. Então a fatia é anotada em disco e permanece até a confirmação.
//!
//! **A anotação vale como retomável se e somente se a fatia foi vista
//! carregada e servindo.** Daí ela carregar um [`SliceState`]: quem escreve
//! `Pending` é o `apply`, quem promove a `Serving` é o `/health` do servidor
//! de inferência respondendo `model_loaded` e quem escreve `Idle` é o monitor
//! de ociosidade depois de liberar os pesos.
//!
//! Confundir os dois é um laço infinito, não um detalhe: uma fatia que não cabe
//! na memória mata o processo antes de carregar, o Docker religa o container, e
//! uma anotação otimista faz o nó tentar a mesma fatia para sempre. O contador
//! um contador apenas em memória não protege disso - ele morre com o processo.
//! Reproduzido em `data/pom-reinicio-infinito-carga/`.

use crate::runtime::NodeConfig;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Em que ponto da vida da fatia esta anotação foi escrita.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SliceState {
    /// Aplicada, ainda não vista carregada uma única vez.
    Pending,
    /// O watcher confirmou `model_loaded` em sondagens consecutivas de
    /// `/health`, depois da janela mínima de estabilidade.
    ///
    /// É o padrão porque um arquivo escrito antes de 2026-08-15 não tem o
    /// campo, e naquele formato só a rota de reinício escrevia — num nó que
    /// estava servindo.
    #[default]
    Serving,
    /// O deployment continua ativo, mas os pesos foram liberados por idle.
    Idle,
}

/// O que está anotado em disco: a fatia e o quanto se sabe sobre ela.
#[derive(Serialize, Deserialize)]
struct Annotation {
    #[serde(default)]
    state: SliceState,
    /// Tentativa da implantação que ainda não chegou ao `/health`.
    /// Anotações antigas sem este campo contam como a primeira tentativa.
    #[serde(default)]
    attempts: u32,
    #[serde(flatten)]
    slice: NodeConfig,
}

/// Limite persistente compartilhado pelo journal e pelo supervisor em memória.
/// Dez já é o limite usado pelo supervisor para falhas da mesma geração.
pub const MAX_AUTOMATIC_ATTEMPTS: u32 = 10;

/// O que o arranque encontrou anotado.
///
/// É enum, e não `Option<NodeConfig>`, porque o defeito que ela corrige foi
/// justamente uma fatia ser aplicada de volta sem ninguém perguntar se ela
/// chegou a servir: aqui não dá para pegar a config sem decidir o caso.
pub enum LastSlice {
    /// Nada anotado.
    Nothing,
    /// Vista servindo antes da queda — o nó volta servindo o mesmo.
    Serving(NodeConfig),
    /// O nó volta anunciado e ocioso, sem recarregar antes da primeira chamada.
    Idle(NodeConfig),
    /// Anotada antes da implantação e ainda não confirmada pelo `/health`.
    Pending { cfg: NodeConfig, attempts: u32 },
}

/// Onde este nó anota a sua fatia.
///
/// Deriva do caminho da config porque dois nós da mesma máquina rodam com
/// `POM_CONFIG` diferentes; um nome fixo faria um sobrescrever o outro.
pub fn path() -> PathBuf {
    path_for(&crate::config::Config::config_path())
}

fn path_for(config_path: &Path) -> PathBuf {
    let mut s = config_path.as_os_str().to_os_string();
    s.push(".last-slice.json");
    PathBuf::from(s)
}

/// Anota a fatia, e em que ponto da vida dela.
pub fn save(cfg: &NodeConfig, state: SliceState) -> Result<()> {
    save_at(&path(), cfg, state)
}

pub(crate) fn save_at(p: &Path, cfg: &NodeConfig, state: SliceState) -> Result<()> {
    let annotation = Annotation {
        state,
        attempts: (state == SliceState::Pending).then_some(1).unwrap_or(0),
        slice: cfg.clone(),
    };
    write_annotation(p, &annotation)
}

fn write_annotation(p: &Path, annotation: &Annotation) -> Result<()> {
    let raw = serde_json::to_string_pretty(annotation)?;
    let tmp = p.with_file_name(format!(
        ".{}.tmp-{}",
        p.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("last-slice"),
        std::process::id()
    ));
    std::fs::write(&tmp, raw).with_context(|| format!("could not write {}", tmp.display()))?;
    // Windows não substitui o destino existente com rename. A tentativa
    // normal é atômica; só removemos o destino se a plataforma exigir.
    if let Err(error) = std::fs::rename(&tmp, p) {
        #[cfg(windows)]
        {
            let _ = std::fs::remove_file(p);
            std::fs::rename(&tmp, p).with_context(|| {
                format!("could not replace {} after {error}", p.display())
            })?;
        }
        #[cfg(not(windows))]
        {
            let _ = std::fs::remove_file(&tmp);
            return Err(error).with_context(|| format!("could not replace {}", p.display()));
        }
    }
    Ok(())
}

/// Esquece a fatia anotada.
///
/// Chamada quando o operador tira o modelo de propósito (liberar a fatia, sair
/// da rede, parar para reconfigurar). Sem isto, um nó esvaziado a pedido
/// voltaria servindo no próximo arranque, desfazendo a decisão de quem o
/// esvaziou.
pub fn clear() {
    clear_at(&path());
}

fn clear_at(p: &Path) {
    let _ = std::fs::remove_file(p);
    let _ = std::fs::remove_file(disable_path(p));
}

fn disable_path(p: &Path) -> PathBuf {
    PathBuf::from(format!("{}.disabled", p.display()))
}

/// Impede qualquer retomada automática até uma nova implantação explícita.
/// Serve também quando o processo principal está sendo relançado por fora.
pub fn force_disable() -> Result<()> {
    force_disable_at(&path())
}

fn force_disable_at(p: &Path) -> Result<()> {
    let _ = std::fs::remove_file(p);
    std::fs::write(disable_path(p), "manual\n").with_context(|| {
        format!(
            "could not create the lock {}",
            disable_path(p).display()
        )
    })
}

pub fn auto_resume_disabled() -> bool {
    disable_path(&path()).exists()
}

/// Inicia uma implantação explícita e registra a primeira tentativa antes de
/// qualquer chamada ao supervisor ou ao servidor nativo.
pub fn begin_new_attempt(cfg: &NodeConfig) -> Result<u32> {
    begin_new_attempt_at(&path(), cfg)
}

fn begin_new_attempt_at(p: &Path, cfg: &NodeConfig) -> Result<u32> {
    let _ = std::fs::remove_file(disable_path(p));
    let annotation = Annotation {
        state: SliceState::Pending,
        attempts: 1,
        slice: cfg.clone(),
    };
    write_annotation(p, &annotation)?;
    Ok(annotation.attempts)
}

/// Registra a retomada de uma implantação que sobreviveu a um reinício.
pub fn resume_attempt(cfg: &NodeConfig) -> Result<u32> {
    resume_attempt_at(&path(), cfg)
}

fn resume_attempt_at(p: &Path, cfg: &NodeConfig) -> Result<u32> {
    let attempts = match read_annotation(p) {
        Some(annotation) if annotation.state == SliceState::Pending => {
            annotation.attempts.max(1).saturating_add(1)
        }
        _ => 1,
    };
    let annotation = Annotation {
        state: SliceState::Pending,
        attempts,
        slice: cfg.clone(),
    };
    write_annotation(p, &annotation)?;
    Ok(attempts)
}

/// Informa se o número da tentativa ainda está dentro do orçamento automático.
pub const fn should_resume(attempts: u32) -> bool {
    attempts <= MAX_AUTOMATIC_ATTEMPTS
}

/// Lê a fatia anotada sem consumir o journal.
pub fn inspect() -> LastSlice {
    inspect_at(&path())
}

fn inspect_at(p: &Path) -> LastSlice {
    let Some(annotation) = read_annotation(p) else {
        return LastSlice::Nothing;
    };
    match annotation.state {
        SliceState::Serving => LastSlice::Serving(annotation.slice),
        SliceState::Idle => LastSlice::Idle(annotation.slice),
        SliceState::Pending => LastSlice::Pending {
            cfg: annotation.slice,
            attempts: annotation.attempts.max(1),
        },
    }
}

fn read_annotation(p: &Path) -> Option<Annotation> {
    let Ok(raw) = std::fs::read_to_string(p) else {
        return None;
    };
    serde_json::from_str::<Annotation>(&raw).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dois nós na mesma máquina rodam com POM_CONFIG diferentes. Se o
    /// arquivo de fatia tivesse nome fixo, um sobrescreveria o do outro e a
    /// máquina voltaria com as duas fatias trocadas.
    #[test]
    fn the_path_derives_from_each_node_config() {
        assert_eq!(
            path_for(std::path::Path::new("/repo/pom.json")),
            std::path::PathBuf::from("/repo/pom.json.last-slice.json")
        );
        assert_eq!(
            path_for(std::path::Path::new("/repo/pom-cpu.json")),
            std::path::PathBuf::from("/repo/pom-cpu.json.last-slice.json")
        );
        assert_ne!(
            path_for(std::path::Path::new("/repo/pom.json")),
            path_for(std::path::Path::new("/repo/pom-cpu.json"))
        );
    }

    /// A leitura não consome o journal: uma queda entre a leitura e o início
    /// da tentativa precisa continuar contabilizável no arranque seguinte.
    #[test]
    fn inspect_preserves_the_journal() {
        let dir = tmp("inspect-preserva");
        let f = dir.join("cfg.json.last-slice.json");
        save_at(&f, &sample(), SliceState::Serving).unwrap();

        assert!(
            matches!(inspect_at(&f), LastSlice::Serving(_)),
            "the read returns the slice"
        );
        assert!(f.exists(), "the read cannot erase the journal");
        assert!(
            matches!(inspect_at(&f), LastSlice::Serving(_)),
            "the next read still returns the same slice"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A garantia de `c7b70d8`: uma fatia que estava servindo saudável e caiu
    /// volta servindo sozinha.
    ///
    /// Um segfault nao executa codigo nenhum: em 2026-08-13 a ksia-gpu3 caiu
    /// com SIGSEGV dentro do kernel CUDA do gated_delta_net, o Docker religou
    /// o container pela politica `unless-stopped`, e o no voltou sem fatia -
    /// sumindo do pipeline em silencio ate alguem notar um "Gap at layer 32"
    /// numa requisicao, minutos depois. Por isso quem anota é o `/health`, e
    /// não um caminho de desligamento gracioso que uma queda dessas pula.
    #[test]
    fn a_slice_seen_serving_comes_back_after_a_crash() {
        let dir = tmp("servindo-volta");
        let f = dir.join("cfg.json.last-slice.json");

        save_at(&f, &sample(), SliceState::Pending).unwrap(); // apply
        save_at(&f, &sample(), SliceState::Serving).unwrap(); // /health: model_loaded

        match inspect_at(&f) {
            LastSlice::Serving(cfg) => assert_eq!(cfg.model_label, "qwen35"),
            _ => panic!("a slice seen serving must come back serving"),
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_idle_slice_comes_back_announced_without_loading() {
        let dir = tmp("ociosa-volta");
        let f = dir.join("cfg.json.last-slice.json");
        save_at(&f, &sample(), SliceState::Idle).unwrap();

        assert!(matches!(inspect_at(&f), LastSlice::Idle(cfg) if cfg.model_label == "qwen35"));
        assert!(f.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Parar é decisão de quem operou, e o arranque seguinte não a desfaz.
    #[test]
    fn stopping_on_purpose_forgets_the_slice() {
        let dir = tmp("parar-esquece");
        let f = dir.join("cfg.json.last-slice.json");
        save_at(&f, &sample(), SliceState::Serving).unwrap();

        clear_at(&f);

        assert!(!f.exists(), "stopping on purpose forgets");
        assert!(
            matches!(inspect_at(&f), LastSlice::Nothing),
            "and the next startup does not resurrect it"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Regressão do laço infinito de reinício (2026-08-15).
    ///
    /// Uma fatia que não cabe na memória mata o processo antes de carregar; o
    /// Docker religa o container; se a anotação escrita no `apply` valesse como
    /// "estava servindo", o nó tentaria a mesma fatia para sempre. O contador
    /// `MAX_RESTARTS` não segura isso: ele é de memória e morre junto com o
    /// processo.
    #[test]
    fn a_slice_that_never_loaded_is_not_resurrected() {
        let dir = tmp("nunca-carregou");
        let f = dir.join("cfg.json.last-slice.json");
        save_at(&f, &sample(), SliceState::Pending).unwrap();

        match inspect_at(&f) {
            LastSlice::Pending { cfg, attempts } => {
                assert_eq!(cfg.model_label, "qwen35");
                assert_eq!(attempts, 1);
            }
            LastSlice::Serving(_) => panic!("a slice that never loaded cannot be resumed"),
            LastSlice::Idle(_) => panic!("a pending slice cannot be idle"),
            LastSlice::Nothing => panic!("the reason of the failure was lost: the operator is left not knowing"),
        }
        assert!(
            f.exists(),
            "the journal must survive the next startup"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Um `apply` sobrepõe a anotação de uma fatia anterior que estava
    /// servindo: da nova ainda não se sabe nada, e é ela que o nó serve agora.
    #[test]
    fn applying_another_slice_drops_the_serving_state_of_the_previous_one() {
        let dir = tmp("apply-sobrepoe");
        let f = dir.join("cfg.json.last-slice.json");
        save_at(&f, &sample(), SliceState::Serving).unwrap();

        let mut new = sample();
        new.start_layer = 8;
        new.end_layer = 15;
        save_at(&f, &new, SliceState::Pending).unwrap();

        match inspect_at(&f) {
            LastSlice::Pending { cfg, attempts } => {
                assert_eq!(cfg.start_layer, 8);
                assert_eq!(attempts, 1);
            }
            _ => panic!("the new slice never loaded; the old one is no longer served"),
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Uma anotação escrita por um binário anterior a esta mudança não tem o
    /// campo `state`. Só a rota de reinício a escrevia, e num nó que estava
    /// servindo — então ela conta como servindo, senão o próprio
    /// "Atualizar e reiniciar" perderia a fatia na travessia de versão.
    #[test]
    fn an_old_annotation_without_state_counts_as_serving() {
        let dir = tmp("formato-antigo");
        let f = dir.join("cfg.json.last-slice.json");
        std::fs::write(&f, serde_json::to_string(&sample()).unwrap()).unwrap();

        assert!(
            matches!(inspect_at(&f), LastSlice::Serving(_)),
            "the old format is a slice that was up"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn restarts_of_a_pending_slice_are_counted_and_blocked_at_the_limit() {
        let dir = tmp("tentativas-persistentes");
        let f = dir.join("cfg.json.last-slice.json");
        let cfg = sample();

        assert_eq!(begin_new_attempt_at(&f, &cfg).unwrap(), 1);
        for expected in 2..=MAX_AUTOMATIC_ATTEMPTS {
            assert_eq!(resume_attempt_at(&f, &cfg).unwrap(), expected);
        }

        assert!(should_resume(MAX_AUTOMATIC_ATTEMPTS));
        assert!(!should_resume(MAX_AUTOMATIC_ATTEMPTS + 1));
        assert!(
            matches!(inspect_at(&f), LastSlice::Pending { attempts, .. } if attempts == MAX_AUTOMATIC_ATTEMPTS)
        );
        assert!(
            f.exists(),
            "the block must survive the next startup"
        );

        clear_at(&f);
        assert!(matches!(inspect_at(&f), LastSlice::Nothing));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_health_success_resets_the_count_and_allows_a_new_resume() {
        let dir = tmp("tentativa-sucesso");
        let f = dir.join("cfg.json.last-slice.json");
        let cfg = sample();

        begin_new_attempt_at(&f, &cfg).unwrap();
        resume_attempt_at(&f, &cfg).unwrap();
        save_at(&f, &cfg, SliceState::Serving).unwrap();

        assert!(matches!(inspect_at(&f), LastSlice::Serving(_)));
        assert_eq!(begin_new_attempt_at(&f, &cfg).unwrap(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_manual_block_prevents_resume_until_a_new_deployment() {
        let dir = tmp("bloqueio-manual");
        let f = dir.join("cfg.json.last-slice.json");
        save_at(&f, &sample(), SliceState::Pending).unwrap();

        force_disable_at(&f).unwrap();
        assert!(!f.exists());
        assert!(disable_path(&f).exists());

        begin_new_attempt_at(&f, &sample()).unwrap();
        assert!(f.exists());
        assert!(!disable_path(&f).exists());

        clear_at(&f);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pom-lastslice-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample() -> NodeConfig {
        crate::runtime::NodeConfig {
            deploy_id: String::new(),
            model_path: "/models/m_0_7.gguf".into(),
            mmproj_path: None,
            mtp_draft_path: None,
            model_label: "qwen35".into(),
            deployment_name: "qwen35".into(),
            start_layer: 0,
            end_layer: 7,
            total_layers: 40,
            peer_count: 5,
            peer_index: 0,
            primary: false,
            inference_mode: Default::default(),
            inference_service: None,
            engine_settings: Default::default(),
            inference_port: 8181,
            n_gpu_layers: 99,
            n_ctx: 8192,
            offload_kqv: true,
            hidden_state_dtype: crate::wire::WireDtype::F16,
            cache_type_k: crate::config::KvCacheType::F16,
            cache_type_v: crate::config::KvCacheType::F16,
            slot_save_path: None,
            cache_ram_mib: None,
            fit_memory: true,
            fit_memory_margin_mb: crate::runtime::DEFAULT_FIT_MEMORY_MARGIN_MB,
            n_batch: 512,
            n_ubatch: llama_engine::DEFAULT_N_UBATCH,
            max_sessions: 1,
            batching_enabled: true,
            max_queue_requests: 128,
            batch_window_ms: 4,
            max_batch_sequences: u32::MAX,
            max_batch_tokens: u32::MAX,
            prefill_chunk_tokens: 512,
            prefill_max_inflight_chunks: 2,
            max_new_tokens: 256,
            temperature: 0.7,
            top_p: 0.9,
            top_k: 40,
            repeat_penalty: 1.15,
            repeat_last_n: 64,
            presence_penalty: 0.0,
            frequency_penalty: 0.0,
            min_p: 0.0,
            seed: llama_engine::llm::DEFAULT_SAMPLER_SEED,
            dry_multiplier: 0.0,
            dry_base: 1.75,
            dry_allowed_length: 2,
            dry_penalty_last_n: 1024,
            reasoning_budget: -1,
            no_thinking: true,
            single_peer: false,
            no_embeddings: false,
            speculative_decoding_enabled: false,
            speculative_k: 4,
            speculative_lookup_window: 3,
            mtp_enabled: false,
            mtp_draft_tokens: 3,
            mtp_min_probability: 0.0,
            mtp_cpu_fallback_enabled: true,
            mtp_placement: Default::default(),
            moe_experts_in_ram: false,
            moe_cache_mib: 0,
            adaptive_prefill_chunks: true,
            unload_if_idle: false,
            idle_minutes: 60,
            tp: None,
        }
    }
}
