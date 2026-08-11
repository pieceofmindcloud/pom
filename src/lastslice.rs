//! A fatia que este nó servia, guardada para sobreviver a um reinício.
//!
//! A atribuição de modelo vive só em memória (é decisão de projeto: quem manda
//! é o plano de implantação, não um arquivo esquecido no disco). Um reinício
//! pedido pela interface, porém, não é uma mudança de intenção — é a mesma
//! intenção com um binário novo. Então a fatia é anotada antes de reiniciar e
//! consumida no arranque seguinte.

use crate::runtime::NodeConfig;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Onde este nó anota a sua fatia.
///
/// Deriva do caminho da config porque dois nós da mesma máquina rodam com
/// `RDMA_P2P_CONFIG` diferentes; um nome fixo faria um sobrescrever o outro.
pub fn path() -> PathBuf {
    path_for(&crate::config::Config::config_path())
}

fn path_for(config_path: &Path) -> PathBuf {
    let mut s = config_path.as_os_str().to_os_string();
    s.push(".last-slice.json");
    PathBuf::from(s)
}

/// Anota a fatia servida agora.
pub fn save(cfg: &NodeConfig) -> Result<()> {
    let p = path();
    std::fs::write(&p, serde_json::to_string_pretty(cfg)?)
        .with_context(|| format!("não deu para gravar {}", p.display()))
}

/// Lê a fatia anotada e apaga o arquivo. Devolve `None` quando não há nada.
pub fn take() -> Option<NodeConfig> {
    take_from(&path())
}

fn take_from(p: &Path) -> Option<NodeConfig> {
    let raw = std::fs::read_to_string(p).ok()?;
    // Apaga antes de aplicar: um arquivo que não parseia, ou uma fatia que o
    // nó não consegue mais servir, não pode reaparecer a cada arranque.
    let _ = std::fs::remove_file(p);
    serde_json::from_str(&raw).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dois nós na mesma máquina rodam com RDMA_P2P_CONFIG diferentes. Se o
    /// arquivo de fatia tivesse nome fixo, um sobrescreveria o do outro e a
    /// máquina voltaria com as duas fatias trocadas.
    #[test]
    fn o_caminho_deriva_da_config_de_cada_no() {
        assert_eq!(
            path_for(std::path::Path::new("/repo/rdma-p2p.json")),
            std::path::PathBuf::from("/repo/rdma-p2p.json.last-slice.json")
        );
        assert_eq!(
            path_for(std::path::Path::new("/repo/rdma-p2p-cpu.json")),
            std::path::PathBuf::from("/repo/rdma-p2p-cpu.json.last-slice.json")
        );
        assert_ne!(
            path_for(std::path::Path::new("/repo/rdma-p2p.json")),
            path_for(std::path::Path::new("/repo/rdma-p2p-cpu.json"))
        );
    }

    /// `take` é consumo: um arquivo órfão de um restart que deu errado não pode
    /// ficar ressuscitando fatia velha a cada arranque.
    #[test]
    fn take_le_uma_vez_e_apaga() {
        let dir = std::env::temp_dir().join(format!("rdma-lastslice-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("cfg.json.last-slice.json");
        std::fs::write(&f, sample_config_json()).unwrap();

        let first = take_from(&f);
        assert!(first.is_some(), "a primeira leitura devolve a fatia");
        assert!(!f.exists(), "e apaga o arquivo");
        assert!(take_from(&f).is_none(), "a segunda não devolve nada");

        std::fs::remove_dir_all(&dir).ok();
    }

    fn sample_config_json() -> String {
        serde_json::to_string(&crate::runtime::NodeConfig {
            model_path: "/models/m_0_7.gguf".into(),
            model_label: "qwen35".into(),
            start_layer: 0,
            end_layer: 7,
            total_layers: 40,
            peer_count: 5,
            peer_index: 0,
            primary: false,
            inference_port: 8181,
            n_gpu_layers: 99,
            n_ctx: 8192,
            offload_kqv: true,
            n_batch: 512,
            max_sessions: 1,
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
        })
        .unwrap()
    }
}
