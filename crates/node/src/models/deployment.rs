//! A implantação como coisa que existe.
//!
//! Até aqui uma implantação não era representada em lugar nenhum: ela era
//! *deduzida*, em cada ponto de uso, do nome do modelo. Quem escolhe os nós de
//! uma requisição comparava `p.model == model_name` (`pipeline::plan_pipeline`),
//! o `deploy.rs` carregava um `group: String` como chave, e desimplantar com
//! dois modelos no ar respondia "há mais de uma pipeline no ar; diga qual".
//! Duas implantações **do mesmo modelo** - em máquinas diferentes, ou com
//! configuração diferente - eram inexprimíveis.
//!
//! O `deploy_id` é essa identidade que faltava: liga um conjunto de nós a um
//! modelo, diz de que forma (modelo inteiro, tensor parallelism ou um conjunto
//! de fatias) e guarda **qual configuração cada nó recebeu**. Sem isso a
//! intenção do operador era descartada no instante em que o launch terminava,
//! e nada no cluster sabia dizer por que uma máquina segurava uma faixa.
//!
//! O molde é o `TpIdentity` (`network.rs`), que já fazia exatamente isto para
//! grupos de tensor parallelism: um id que nomeia um conjunto de nós, cunhado
//! no deploy, carregado no anúncio e no `NodeConfig`, sobrevivente a reinício.
//! Aqui a mesma ideia vale para toda implantação.
//!
//! **Fonte legada:** um arquivo por implantação, em
//! `<POM_HOME>/deployments/`, em cada nó participante. O runtime atual grava e
//! lê o registro em `backend::db::PomDb`; estas funções de arquivo continuam
//! apenas como rede de segurança para a ferramenta descartável de preservação.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Versão do formato. Sobe quando o significado de um campo muda.
pub const DEPLOYMENT_VERSION: u32 = 1;

/// Prefixo do id, para que ele seja reconhecível solto num log ou numa URL.
const ID_PREFIX: &str = "d-";

/// Um `deploy_id` novo.
///
/// Aleatório, e **não** derivado do conteúdo: duas implantações que só diferem
/// na escolha de máquinas precisam ser distinguíveis, e implantar o mesmo
/// arranjo duas vezes de propósito não pode colidir. É o oposto da regra do
/// `pom_id`, que é derivado justamente porque lá o mesmo conteúdo *é* a mesma
/// coisa.
pub fn new_deploy_id() -> String {
    let u = uuid::Uuid::new_v4().simple().to_string();
    format!("{ID_PREFIX}{}", &u[..8])
}

/// Um id plausível: o prefixo mais 8 dígitos hex.
///
/// Vale como validação de entrada **e** como defesa de caminho — é isto que
/// impede um id vindo da rede de virar `../` ao nomear um arquivo.
pub fn is_deploy_id(s: &str) -> bool {
    match s.strip_prefix(ID_PREFIX) {
        Some(resto) => {
            resto.len() == 8
                && resto
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }
        None => false,
    }
}

/// De que forma o modelo está no ar.
///
/// Espelha `deploy::DeployKind`, que é o que a requisição de implantação já
/// usava; aqui ele vira parte do registro em vez de morrer com o job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentKind {
    /// Um conjunto de fatias que juntas cobrem o modelo.
    Shards,
    /// Um GGUF inteiro numa máquina só.
    Whole,
    /// Um modelo dividido por tensor parallelism entre um primary e K workers.
    TensorParallel,
}

/// Que modelo esta implantação serve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeploymentIdentity {
    /// Hash do GGUF inteiro, calculado no arquivo escolhido pelo operador.
    Sha256 { sha256: String },
    /// Identidade do manifesto que produziu as fatias.
    PomId { pom_id: String },
}

/// Que modelo esta implantação serve.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeploymentModel {
    /// O rótulo que o operador vê e que as requisições nomeiam.
    pub label: String,
    pub total_layers: u32,
    /// A rodada de fatiamento que produziu as partes, quando há uma. Liga a
    /// implantação ao artefato exato que ela está rodando. Ver `models/pom.rs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pom_id: Option<String>,
    /// Prova da identidade do artefato. Registros antigos não têm este campo
    /// e não podem ser usados para desimplantar com segurança sem uma prova de
    /// caminho no mesmo peer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<DeploymentIdentity>,
}

/// Um nó da implantação, e a configuração com que ele subiu.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeploymentNode {
    pub node_id: String,
    /// Nome legível da máquina no momento da implantação, para a tela não
    /// precisar resolver um `node_id` de 64 caracteres.
    #[serde(default)]
    pub hostname: String,
    pub start_layer: u32,
    pub end_layer: u32,
    pub peer_index: u32,
    pub peer_count: u32,
    #[serde(default)]
    pub primary: bool,
    /// A parte que este nó carrega, endereçada por conteúdo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part_sha256: Option<String>,
    /// Rank e tamanho do grupo, num deploy de tensor parallelism.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tp_rank: Option<u32>,
    /// A configuração **efetiva** deste nó - já resolvidos os ajustes por
    /// máquina (`n_gpu_layers`, `offload_kqv`) sobre os parâmetros do deploy.
    ///
    /// Guardar o resultado, e não os parâmetros de base mais os ajustes, é o
    /// que faz "com que configuração esta máquina subiu?" ser uma leitura em
    /// vez de uma reconstrução.
    pub config: crate::webui::RuntimeParams,
}

/// O registro de uma implantação.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Deployment {
    pub deployment_version: u32,
    pub deploy_id: String,
    pub kind: DeploymentKind,
    pub model: DeploymentModel,
    pub created_at: String,
    /// `node_id` de quem coordenou a implantação.
    #[serde(default)]
    pub created_by: String,
    /// Quando esta implantação passou a ser a preferida do seu modelo.
    ///
    /// É o que decide, entre duas implantações do mesmo modelo, qual atende uma
    /// requisição que só diz `"qwen3:8b"`. **Derivado, nunca eleito**: a ativa é
    /// a de `activated_at` mais recente, então dois nós que discordem convergem
    /// para a mesma resposta sem protocolo de consenso nenhum. Ver [`active`].
    pub activated_at: String,
    pub nodes: Vec<DeploymentNode>,
}

impl Deployment {
    pub fn new(
        deploy_id: String,
        kind: DeploymentKind,
        model: DeploymentModel,
        created_by: impl Into<String>,
        nodes: Vec<DeploymentNode>,
    ) -> Self {
        let agora = chrono::Utc::now().to_rfc3339();
        Self {
            deployment_version: DEPLOYMENT_VERSION,
            deploy_id,
            kind,
            model,
            created_at: agora.clone(),
            created_by: created_by.into(),
            activated_at: agora,
            nodes,
        }
    }

    pub fn node(&self, node_id: &str) -> Option<&DeploymentNode> {
        self.nodes.iter().find(|n| n.node_id == node_id)
    }

    /// As faixas dos nós cobrem `0..total_layers` sem buraco?
    ///
    /// Num grupo de tensor parallelism todo mundo cobre o modelo inteiro, então
    /// a pergunta é outra: basta haver nó.
    pub fn covers_the_model(&self) -> bool {
        if self.kind == DeploymentKind::TensorParallel {
            return !self.nodes.is_empty();
        }
        let mut faixas: Vec<(u32, u32)> = self
            .nodes
            .iter()
            .map(|n| (n.start_layer, n.end_layer))
            .collect();
        faixas.sort_unstable();
        let mut proxima = 0u32;
        for (inicio, fim) in faixas {
            if inicio != proxima {
                return false;
            }
            proxima = fim + 1;
        }
        proxima == self.model.total_layers
    }

    pub fn validate(&self) -> Result<()> {
        if self.deployment_version > DEPLOYMENT_VERSION {
            bail!(
                "registro versão {} é mais novo que esta build (suporta até {DEPLOYMENT_VERSION})",
                self.deployment_version
            );
        }
        if !is_deploy_id(&self.deploy_id) {
            bail!("deploy_id inválido: {:?}", self.deploy_id);
        }
        if self.model.label.trim().is_empty() {
            bail!("implantação sem rótulo de modelo");
        }
        if self.nodes.is_empty() {
            bail!("implantação sem nós");
        }
        for n in &self.nodes {
            if n.start_layer > n.end_layer {
                bail!(
                    "{} tem faixa invertida ({}-{})",
                    n.node_id,
                    n.start_layer,
                    n.end_layer
                );
            }
        }
        Ok(())
    }
}

/// Qual implantação atende uma requisição que só diz o nome do modelo.
///
/// A mais recentemente ativada entre as daquele rótulo. Empate resolvido pelo
/// `deploy_id`, para que a resposta não dependa da ordem em que os registros
/// foram lidos do disco - duas máquinas têm que chegar à mesma conclusão.
pub fn active<'a>(deployments: &'a [Deployment], label: &str) -> Option<&'a Deployment> {
    deployments
        .iter()
        .filter(|d| d.model.label == label)
        .max_by(|a, b| {
            a.activated_at
                .cmp(&b.activated_at)
                .then_with(|| a.deploy_id.cmp(&b.deploy_id))
        })
}

/// Separa `"qwen3:8b@d-4f2a91c3"` em `("qwen3:8b", Some("d-4f2a91c3"))`.
///
/// Sem o sufixo, a requisição não escolheu implantação e quem responde é a
/// ativa - que é como todo cliente escrito antes disto continua funcionando.
/// Um `@` seguido de coisa que não é um id fica **no nome**: há rótulo de
/// modelo com `@` no mundo real, e engolir um pedaço dele calado seria pior que
/// não achar o modelo.
pub fn split_selector(model: &str) -> (&str, Option<&str>) {
    match model.rsplit_once('@') {
        Some((nome, id)) if is_deploy_id(id) => (nome, Some(id)),
        _ => (model, None),
    }
}

// ── Em disco ─────────────────────────────────────────────────────────────────

/// A pasta legada dos registros: `<POM_HOME>/deployments/`.
#[allow(dead_code)]
pub fn dir() -> PathBuf {
    crate::config::project_root().join("deployments")
}

#[allow(dead_code)]
pub fn path_in(dir: &Path, deploy_id: &str) -> Result<PathBuf> {
    if !is_deploy_id(deploy_id) {
        bail!("deploy_id inválido: {deploy_id:?}");
    }
    Ok(dir.join(format!("{deploy_id}.json")))
}

/// Grava o registro sem deixar arquivo pela metade se a máquina cair no meio.
///
/// Mesmo padrão de `pom::write_atomic`: temporário ao lado e `rename`, que é
/// atômico no mesmo sistema de arquivos.
#[allow(dead_code)]
pub fn save_in(dir: &Path, d: &Deployment) -> Result<PathBuf> {
    d.validate()?;
    std::fs::create_dir_all(dir).with_context(|| format!("criar {}", dir.display()))?;

    let destino = path_in(dir, &d.deploy_id)?;
    let temp = dir.join(format!(".{}.{}.tmp", d.deploy_id, std::process::id()));
    let json = serde_json::to_vec_pretty(d).context("serializar implantação")?;
    {
        let mut f =
            std::fs::File::create(&temp).with_context(|| format!("criar {}", temp.display()))?;
        f.write_all(&json)?;
        f.sync_all()?;
    }
    if let Err(e) = std::fs::rename(&temp, &destino) {
        let _ = std::fs::remove_file(&temp);
        return Err(e).with_context(|| format!("renomear para {}", destino.display()));
    }
    Ok(destino)
}

#[allow(dead_code)]
pub fn save(d: &Deployment) -> Result<PathBuf> {
    save_in(&dir(), d)
}

#[allow(dead_code)]
pub fn load(path: &Path) -> Result<Deployment> {
    let raw = std::fs::read(path).with_context(|| format!("ler {}", path.display()))?;
    let d: Deployment =
        serde_json::from_slice(&raw).with_context(|| format!("interpretar {}", path.display()))?;
    d.validate()
        .with_context(|| format!("registro inválido em {}", path.display()))?;
    Ok(d)
}

/// Todos os registros de uma pasta, ordenados por id para a listagem ser
/// estável entre chamadas.
#[allow(dead_code)]
pub fn load_dir(dir: &Path) -> Vec<Deployment> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match load(&p) {
            Ok(d) => out.push(d),
            // Um registro quebrado não pode derrubar a listagem inteira: as
            // outras implantações seguem no ar e precisam aparecer.
            Err(e) => tracing::warn!("ignorando registro {}: {e:#}", p.display()),
        }
    }
    out.sort_by(|a, b| a.deploy_id.cmp(&b.deploy_id));
    out
}

#[allow(dead_code)]
pub fn all() -> Vec<Deployment> {
    load_dir(&dir())
}

/// Esquece uma implantação neste nó. Idempotente.
#[allow(dead_code)]
pub fn remove_in(dir: &Path, deploy_id: &str) -> Result<()> {
    let p = path_in(dir, deploy_id)?;
    match std::fs::remove_file(&p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remover {}", p.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no(id: &str, inicio: u32, fim: u32, indice: u32, total: u32) -> DeploymentNode {
        DeploymentNode {
            node_id: id.into(),
            hostname: id.into(),
            start_layer: inicio,
            end_layer: fim,
            peer_index: indice,
            peer_count: total,
            primary: indice == 0,
            part_sha256: None,
            tp_rank: None,
            config: crate::webui::RuntimeParams::default(),
        }
    }

    fn implantacao(id: &str, rotulo: &str, nos: Vec<DeploymentNode>) -> Deployment {
        Deployment::new(
            id.into(),
            DeploymentKind::Shards,
            DeploymentModel {
                label: rotulo.into(),
                total_layers: 32,
                pom_id: None,
                identity: None,
            },
            "coordenador",
            nos,
        )
    }

    #[test]
    fn um_id_novo_e_valido_e_nao_repete() {
        let a = new_deploy_id();
        let b = new_deploy_id();
        assert!(is_deploy_id(&a), "{a}");
        assert_ne!(a, b);
    }

    /// Um id vindo da rede vira nome de arquivo: se `..` passasse, um peer
    /// escolheria onde gravar no disco de quem recebe.
    #[test]
    fn id_que_nao_e_id_nao_vira_caminho() {
        assert!(!is_deploy_id("../../etc/passwd"));
        assert!(!is_deploy_id("d-../../x"));
        assert!(!is_deploy_id("d-ABCDEF12"), "hex é minúsculo");
        assert!(!is_deploy_id("d-123"), "curto demais");
        assert!(!is_deploy_id("4f2a91c3"), "sem prefixo");
        assert!(path_in(Path::new("/tmp/d"), "../x").is_err());
    }

    /// O caso que não existia: **o mesmo modelo**, duas implantações.
    #[test]
    fn duas_implantacoes_do_mesmo_modelo_convivem() {
        let uma = implantacao("d-aaaaaaaa", "qwen3:8b", vec![no("n1", 0, 31, 0, 1)]);
        let mut outra = implantacao(
            "d-bbbbbbbb",
            "qwen3:8b",
            vec![no("n2", 0, 15, 0, 2), no("n3", 16, 31, 1, 2)],
        );
        outra.activated_at = "2030-01-01T00:00:00Z".into();

        let todas = vec![uma, outra];
        assert_eq!(
            active(&todas, "qwen3:8b").unwrap().deploy_id,
            "d-bbbbbbbb",
            "a ativa é a de ativação mais recente"
        );
        assert!(active(&todas, "nao-existe").is_none());
    }

    /// Empate tem que ser resolvido igual nas duas máquinas, senão uma manda a
    /// requisição para uma implantação e a outra para a outra.
    #[test]
    fn empate_de_ativacao_e_resolvido_de_forma_estavel() {
        let mut a = implantacao("d-aaaaaaaa", "m", vec![no("n1", 0, 31, 0, 1)]);
        let mut b = implantacao("d-bbbbbbbb", "m", vec![no("n2", 0, 31, 0, 1)]);
        a.activated_at = "2026-01-01T00:00:00Z".into();
        b.activated_at = "2026-01-01T00:00:00Z".into();

        let numa_ordem = vec![a.clone(), b.clone()];
        let na_outra = vec![b, a];
        assert_eq!(
            active(&numa_ordem, "m").unwrap().deploy_id,
            active(&na_outra, "m").unwrap().deploy_id
        );
    }

    #[test]
    fn o_seletor_separa_modelo_de_implantacao() {
        assert_eq!(split_selector("qwen3:8b"), ("qwen3:8b", None));
        assert_eq!(
            split_selector("qwen3:8b@d-4f2a91c3"),
            ("qwen3:8b", Some("d-4f2a91c3"))
        );
        // Rótulo com `@` que não termina em id continua inteiro.
        assert_eq!(
            split_selector("org@example/modelo"),
            ("org@example/modelo", None)
        );
        assert_eq!(split_selector("modelo@sei-la"), ("modelo@sei-la", None));
    }

    #[test]
    fn cobertura_de_camadas() {
        assert!(implantacao("d-aaaaaaaa", "m", vec![no("n1", 0, 31, 0, 1)]).covers_the_model());
        assert!(implantacao(
            "d-aaaaaaaa",
            "m",
            vec![no("n1", 0, 15, 0, 2), no("n2", 16, 31, 1, 2)]
        )
        .covers_the_model());
        // Buraco em 16-19.
        assert!(!implantacao(
            "d-aaaaaaaa",
            "m",
            vec![no("n1", 0, 15, 0, 2), no("n2", 20, 31, 1, 2)]
        )
        .covers_the_model());
    }

    #[test]
    fn gravar_reler_e_esquecer() {
        let tmp =
            std::env::temp_dir().join(format!("pom-deploy-{}", uuid::Uuid::new_v4().simple()));
        let d = implantacao(
            "d-4f2a91c3",
            "qwen3:8b",
            vec![no("n1", 0, 15, 0, 2), no("n2", 16, 31, 1, 2)],
        );

        let escrito = save_in(&tmp, &d).unwrap();
        assert_eq!(load(&escrito).unwrap(), d);
        assert_eq!(load_dir(&tmp), vec![d.clone()]);

        remove_in(&tmp, &d.deploy_id).unwrap();
        assert!(load_dir(&tmp).is_empty());
        // Esquecer o que já foi esquecido não é erro.
        remove_in(&tmp, &d.deploy_id).unwrap();

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn registro_sem_no_ou_com_id_torto_e_recusado() {
        let mut d = implantacao("d-4f2a91c3", "m", vec![no("n1", 0, 31, 0, 1)]);
        d.nodes.clear();
        assert!(d.validate().is_err());

        let mut d = implantacao("d-4f2a91c3", "m", vec![no("n1", 0, 31, 0, 1)]);
        d.deploy_id = "nao-e-id".into();
        assert!(d.validate().is_err());
    }
}
