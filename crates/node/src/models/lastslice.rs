//! A fatia que este nó servia, guardada para sobreviver a um reinício.
//!
//! A atribuição de modelo vive só em memória (é decisão de projeto: quem manda
//! é o plano de implantação, não um arquivo esquecido no disco). Uma queda,
//! porém, não é uma mudança de intenção — é a mesma intenção com o processo
//! novo. Então a fatia é anotada em disco e consumida no arranque seguinte.
//!
//! **A anotação vale como retomável se e somente se a fatia foi vista
//! carregada e servindo.** Daí ela carregar um [`SliceState`]: quem escreve
//! `Pending` é o `apply`, e quem promove a `Serving` é o `/health` do servidor
//! de inferência respondendo `model_loaded` — o único ponto do código que prova
//! que a fatia está no ar.
//!
//! Confundir os dois é um laço infinito, não um detalhe: uma fatia que não cabe
//! na memória mata o processo antes de carregar, o Docker religa o container, e
//! uma anotação otimista faz o nó tentar a mesma fatia para sempre. O contador
//! `MAX_RESTARTS` não protege disso — é de memória e morre com o processo.
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
    /// O `/health` do servidor de inferência respondeu `model_loaded`.
    ///
    /// É o padrão porque um arquivo escrito antes de 2026-08-15 não tem o
    /// campo, e naquele formato só a rota de reinício escrevia — num nó que
    /// estava servindo.
    #[default]
    Serving,
}

/// O que está anotado em disco: a fatia e o quanto se sabe sobre ela.
#[derive(Serialize, Deserialize)]
struct Annotation {
    #[serde(default)]
    state: SliceState,
    #[serde(flatten)]
    slice: NodeConfig,
}

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
    /// Anotada no `apply` e nunca vista carregada. Não se retoma: foi o laço
    /// infinito de 2026-08-15, em que uma fatia que não cabia na memória
    /// ressuscitava a cada religada do container.
    NeverLoaded(NodeConfig),
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

fn save_at(p: &Path, cfg: &NodeConfig, state: SliceState) -> Result<()> {
    let anot = Annotation {
        state,
        slice: cfg.clone(),
    };
    std::fs::write(p, serde_json::to_string_pretty(&anot)?)
        .with_context(|| format!("não deu para gravar {}", p.display()))
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
}

/// Lê a fatia anotada e apaga o arquivo.
pub fn take() -> LastSlice {
    take_from(&path())
}

fn take_from(p: &Path) -> LastSlice {
    let Ok(raw) = std::fs::read_to_string(p) else {
        return LastSlice::Nothing;
    };
    // Apaga antes de aplicar: um arquivo que não parseia, ou uma fatia que o
    // nó não consegue mais servir, não pode reaparecer a cada arranque.
    let _ = std::fs::remove_file(p);
    let Ok(anot) = serde_json::from_str::<Annotation>(&raw) else {
        return LastSlice::Nothing;
    };
    match anot.state {
        SliceState::Serving => LastSlice::Serving(anot.slice),
        SliceState::Pending => LastSlice::NeverLoaded(anot.slice),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dois nós na mesma máquina rodam com POM_CONFIG diferentes. Se o
    /// arquivo de fatia tivesse nome fixo, um sobrescreveria o do outro e a
    /// máquina voltaria com as duas fatias trocadas.
    #[test]
    fn o_caminho_deriva_da_config_de_cada_no() {
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

    /// `take` é consumo: um arquivo órfão de um restart que deu errado não pode
    /// ficar ressuscitando fatia velha a cada arranque.
    #[test]
    fn take_le_uma_vez_e_apaga() {
        let dir = tmp("take-uma-vez");
        let f = dir.join("cfg.json.last-slice.json");
        save_at(&f, &sample(), SliceState::Serving).unwrap();

        assert!(
            matches!(take_from(&f), LastSlice::Serving(_)),
            "a primeira leitura devolve a fatia"
        );
        assert!(!f.exists(), "e apaga o arquivo");
        assert!(
            matches!(take_from(&f), LastSlice::Nothing),
            "a segunda não devolve nada"
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
    fn fatia_vista_servindo_volta_depois_de_uma_queda() {
        let dir = tmp("servindo-volta");
        let f = dir.join("cfg.json.last-slice.json");

        save_at(&f, &sample(), SliceState::Pending).unwrap(); // apply
        save_at(&f, &sample(), SliceState::Serving).unwrap(); // /health: model_loaded

        match take_from(&f) {
            LastSlice::Serving(cfg) => assert_eq!(cfg.model_label, "qwen35"),
            _ => panic!("uma fatia vista servindo tem que voltar servindo"),
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Parar é decisão de quem operou, e o arranque seguinte não a desfaz.
    #[test]
    fn parar_de_proposito_esquece_a_fatia() {
        let dir = tmp("parar-esquece");
        let f = dir.join("cfg.json.last-slice.json");
        save_at(&f, &sample(), SliceState::Serving).unwrap();

        clear_at(&f);

        assert!(!f.exists(), "parar de propósito esquece");
        assert!(
            matches!(take_from(&f), LastSlice::Nothing),
            "e o arranque seguinte nao ressuscita"
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
    fn fatia_que_nunca_carregou_nao_ressuscita() {
        let dir = tmp("nunca-carregou");
        let f = dir.join("cfg.json.last-slice.json");
        save_at(&f, &sample(), SliceState::Pending).unwrap();

        match take_from(&f) {
            LastSlice::NeverLoaded(cfg) => assert_eq!(cfg.model_label, "qwen35"),
            LastSlice::Serving(_) => panic!("uma fatia que nunca carregou não pode ser retomada"),
            LastSlice::Nothing => panic!("a razão da falha se perdeu: o operador fica sem saber"),
        }
        assert!(!f.exists(), "e o arquivo sai do disco na leitura");
        assert!(
            matches!(take_from(&f), LastSlice::Nothing),
            "o arranque seguinte não encontra mais nada — o laço morre aqui"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Um `apply` sobrepõe a anotação de uma fatia anterior que estava
    /// servindo: da nova ainda não se sabe nada, e é ela que o nó serve agora.
    #[test]
    fn o_apply_de_outra_fatia_derruba_o_servindo_da_anterior() {
        let dir = tmp("apply-sobrepoe");
        let f = dir.join("cfg.json.last-slice.json");
        save_at(&f, &sample(), SliceState::Serving).unwrap();

        let mut nova = sample();
        nova.start_layer = 8;
        nova.end_layer = 15;
        save_at(&f, &nova, SliceState::Pending).unwrap();

        match take_from(&f) {
            LastSlice::NeverLoaded(cfg) => assert_eq!(cfg.start_layer, 8),
            _ => panic!("a fatia nova nunca carregou; a velha já não é servida"),
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Uma anotação escrita por um binário anterior a esta mudança não tem o
    /// campo `state`. Só a rota de reinício a escrevia, e num nó que estava
    /// servindo — então ela conta como servindo, senão o próprio
    /// "Atualizar e reiniciar" perderia a fatia na travessia de versão.
    #[test]
    fn anotacao_antiga_sem_estado_conta_como_servindo() {
        let dir = tmp("formato-antigo");
        let f = dir.join("cfg.json.last-slice.json");
        std::fs::write(&f, serde_json::to_string(&sample()).unwrap()).unwrap();

        assert!(
            matches!(take_from(&f), LastSlice::Serving(_)),
            "o formato antigo é uma fatia que estava no ar"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    fn tmp(nome: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pom-lastslice-{nome}-{}-{:?}",
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
            model_label: "qwen35".into(),
            deployment_name: "qwen35".into(),
            start_layer: 0,
            end_layer: 7,
            total_layers: 40,
            peer_count: 5,
            peer_index: 0,
            primary: false,
            inference_mode: Default::default(),
            inference_port: 8181,
            n_gpu_layers: 99,
            n_ctx: 8192,
            offload_kqv: true,
            cache_type_k: crate::config::KvCacheType::F16,
            cache_type_v: crate::config::KvCacheType::F16,
            fit_memory: true,
            n_batch: 512,
            max_sessions: 1,
            batching_enabled: true,
            max_queue_requests: 128,
            batch_window_ms: 4,
            max_batch_sequences: u32::MAX,
            max_batch_tokens: u32::MAX,
            prefill_chunk_tokens: 512,
            max_new_tokens: 256,
            temperature: 0.7,
            top_p: 0.9,
            top_k: 40,
            repeat_penalty: 1.15,
            repeat_last_n: 64,
            no_thinking: true,
            single_peer: false,
            no_embeddings: false,
            speculative_decoding_enabled: false,
            speculative_k: 4,
            speculative_lookup_window: 3,
            mtp_enabled: false,
            mtp_draft_tokens: 3,
            mtp_min_probability: 0.0,
            tp: None,
        }
    }
}
