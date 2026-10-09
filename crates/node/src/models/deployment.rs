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
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Versão do formato. Sobe quando o significado de um campo muda.
///
/// v2 acrescentou `versions`/`active_version`: um deployment passou a guardar
/// o histórico de configurações que já rodou sob o mesmo `deploy_id`. Um
/// registro v1 (sem esses campos) é lido com `versions` vazio e
/// [`Deployment::ensure_versions`] sintetiza a v1 a partir de `nodes`.
pub const DEPLOYMENT_VERSION: u32 = 2;

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
        Some(rest) => {
            rest.len() == 8
                && rest
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
    /// Um deployment com uma rota de modelo inteiro e outra de fatias.
    Mixed,
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
    /// O arquivo que este nó carregou, no disco dele. Num modelo inteiro é o
    /// próprio GGUF; numa fatia sem `part_sha256` (rodada antiga) é o único
    /// ponteiro para o arquivo. Registros antigos não têm este campo e o
    /// `redeploy` resolve o arquivo pelo catálogo local nesse caso.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_model_path: Option<String>,
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

/// Uma configuração que este deployment já rodou, sob o mesmo `deploy_id`.
///
/// O que distingue uma versão de outra são **os parâmetros do deployment
/// inteiro** (`params`). Ajustes por máquina (`n_gpu_layers`/`offload_kqv`)
/// entram no snapshot `nodes` mas não forkam uma versão. Reimplantar com
/// parâmetros iguais aos de uma versão existente reativa aquela versão em vez
/// de anexar outra.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeploymentVersion {
    /// Monotônico dentro do deployment, começa em 1.
    pub version: u32,
    pub created_at: String,
    #[serde(default)]
    pub created_by: String,
    /// Os parâmetros do deployment inteiro com que esta versão subiu.
    pub params: crate::webui::RuntimeParams,
    /// Quem recebeu qual faixa e com que configuração efetiva, nesta versão.
    pub nodes: Vec<DeploymentNode>,
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
    /// A lista achatada da versão ativa - mantida em sincronia com
    /// `versions[active_version]` por [`Deployment::sync_active_nodes`]. Todo o
    /// runtime (redeploy, undeploy, roteamento, tela) lê daqui.
    pub nodes: Vec<DeploymentNode>,
    /// O histórico de configurações deste `deploy_id`. Um registro v1 chega
    /// aqui vazio e [`Deployment::ensure_versions`] sintetiza a v1.
    #[serde(default)]
    pub versions: Vec<DeploymentVersion>,
    /// Qual versão está no ar. `1` quando `versions` ainda não foi sintetizado.
    #[serde(default = "one")]
    pub active_version: u32,
}

fn one() -> u32 {
    1
}

impl Deployment {
    pub fn new(
        deploy_id: String,
        kind: DeploymentKind,
        model: DeploymentModel,
        created_by: impl Into<String>,
        nodes: Vec<DeploymentNode>,
    ) -> Self {
        let now = chrono::Utc::now().to_rfc3339();
        let created_by = created_by.into();
        Self {
            deployment_version: DEPLOYMENT_VERSION,
            deploy_id,
            kind,
            model,
            created_at: now.clone(),
            created_by: created_by.clone(),
            activated_at: now.clone(),
            versions: vec![DeploymentVersion {
                version: 1,
                created_at: now,
                created_by,
                params: crate::webui::RuntimeParams::default(),
                nodes: nodes.clone(),
            }],
            active_version: 1,
            nodes,
        }
    }

    /// Fixa os parâmetros da v1. Usado por `build_record`, que só conhece os
    /// parâmetros depois de montar os nós.
    pub fn with_params(mut self, params: crate::webui::RuntimeParams) -> Self {
        if let Some(v1) = self.versions.first_mut() {
            v1.params = params;
        }
        self
    }

    pub fn node(&self, node_id: &str) -> Option<&DeploymentNode> {
        self.nodes.iter().find(|n| n.node_id == node_id)
    }

    /// A versão que está no ar.
    pub fn active(&self) -> Option<&DeploymentVersion> {
        self.versions
            .iter()
            .find(|v| v.version == self.active_version)
    }

    /// Escreve `self.nodes` de volta no snapshot da versão ativa. Usado depois
    /// que o redeploy grava, por nó, a configuração que a máquina de fato
    /// aceitou.
    pub fn commit_active_nodes(&mut self) {
        let active = self.active_version;
        if let Some(v) = self.versions.iter_mut().find(|v| v.version == active) {
            v.nodes = self.nodes.clone();
        }
    }

    /// Copia os nós da versão ativa para `self.nodes`, a lista que o runtime lê.
    fn sync_active_nodes(&mut self) {
        if let Some(nodes) = self
            .versions
            .iter()
            .find(|v| v.version == self.active_version)
            .map(|v| v.nodes.clone())
        {
            self.nodes = nodes;
        }
    }

    /// Sintetiza a v1 de um registro v1 legado (ou vindo de um peer antigo),
    /// que chega com `versions` vazio. Idempotente.
    pub fn ensure_versions(&mut self) {
        if !self.versions.is_empty() {
            return;
        }
        self.versions.push(DeploymentVersion {
            version: 1,
            created_at: self.created_at.clone(),
            created_by: self.created_by.clone(),
            params: self
                .nodes
                .first()
                .map(|n| n.config.clone())
                .unwrap_or_default(),
            nodes: self.nodes.clone(),
        });
        self.active_version = 1;
    }

    /// Torna a versão `v` a ativa, sincroniza `self.nodes` e recarimba
    /// `activated_at`. Erro se `v` não existe.
    pub fn set_active_version(&mut self, v: u32) -> Result<()> {
        if !self.versions.iter().any(|ver| ver.version == v) {
            let available: Vec<u32> = self.versions.iter().map(|ver| ver.version).collect();
            bail!("version {v} does not exist (available: {available:?})");
        }
        self.active_version = v;
        self.activated_at = chrono::Utc::now().to_rfc3339();
        self.sync_active_nodes();
        Ok(())
    }

    /// Reimplanta com os `params` do deployment inteiro.
    ///
    /// Se já existe uma versão com esses `params`, reaproveita o número dela,
    /// aplica os `nodes` novos (que carregam os ajustes por máquina) e a torna
    /// ativa. Senão anexa `max(version) + 1`. Sempre recarimba `activated_at` e
    /// sincroniza `self.nodes`. Devolve o número da versão ativa.
    pub fn upsert_version(
        &mut self,
        params: crate::webui::RuntimeParams,
        nodes: Vec<DeploymentNode>,
        created_by: impl Into<String>,
    ) -> u32 {
        self.ensure_versions();
        let now = chrono::Utc::now().to_rfc3339();
        let target = match self.versions.iter().position(|v| v.params == params) {
            Some(i) => {
                self.versions[i].nodes = nodes;
                self.versions[i].version
            }
            None => {
                let number = self.versions.iter().map(|v| v.version).max().unwrap_or(0) + 1;
                self.versions.push(DeploymentVersion {
                    version: number,
                    created_at: now.clone(),
                    created_by: created_by.into(),
                    params,
                    nodes,
                });
                number
            }
        };
        self.active_version = target;
        self.activated_at = now;
        self.sync_active_nodes();
        target
    }

    /// Anexa uma versão mesmo quando os parâmetros não mudaram.
    ///
    /// Alterações de topologia, como adicionar ou remover uma máquina, são
    /// eventos de histórico por si só. Reusar a versão por igualdade de
    /// parâmetros apagaria justamente o conjunto de máquinas que a operação
    /// mudou.
    pub fn append_version(
        &mut self,
        params: crate::webui::RuntimeParams,
        nodes: Vec<DeploymentNode>,
        created_by: impl Into<String>,
    ) -> u32 {
        self.ensure_versions();
        let now = chrono::Utc::now().to_rfc3339();
        let number = self.versions.iter().map(|v| v.version).max().unwrap_or(0) + 1;
        self.versions.push(DeploymentVersion {
            version: number,
            created_at: now.clone(),
            created_by: created_by.into(),
            params,
            nodes,
        });
        self.active_version = number;
        self.activated_at = now;
        self.sync_active_nodes();
        number
    }

    /// As faixas dos nós cobrem `0..total_layers` sem buraco?
    ///
    /// Num grupo de tensor parallelism todo mundo cobre o modelo inteiro, então
    /// a pergunta é outra: basta haver nó.
    pub fn covers_the_model(&self) -> bool {
        if self.kind == DeploymentKind::TensorParallel {
            return !self.nodes.is_empty();
        }
        let mut ranges: Vec<(u32, u32)> = self
            .nodes
            .iter()
            .map(|n| (n.start_layer, n.end_layer))
            .collect();
        ranges.sort_unstable();
        let mut next = 0u32;
        for (start, end) in ranges {
            if start > next {
                return false;
            }
            next = next.max(end.saturating_add(1));
        }
        next == self.model.total_layers
    }

    pub fn validate(&self) -> Result<()> {
        if self.deployment_version > DEPLOYMENT_VERSION {
            bail!(
                "record version {} is newer than this build (supports up to {DEPLOYMENT_VERSION})",
                self.deployment_version
            );
        }
        if !is_deploy_id(&self.deploy_id) {
            bail!("invalid deploy_id: {:?}", self.deploy_id);
        }
        if self.model.label.trim().is_empty() {
            bail!("deployment without a model label");
        }
        if self.nodes.is_empty()
            && self
                .active()
                .is_none_or(|version| !version.nodes.is_empty())
        {
            bail!("deployment without nodes");
        }
        for n in &self.nodes {
            if n.start_layer > n.end_layer {
                bail!(
                    "{} has an inverted range ({}-{})",
                    n.node_id,
                    n.start_layer,
                    n.end_layer
                );
            }
        }
        if !self.versions.is_empty() {
            let mut seen = HashSet::new();
            for v in &self.versions {
                if v.version == 0 {
                    bail!("version 0 is not valid");
                }
                if !seen.insert(v.version) {
                    bail!("version {} repeated", v.version);
                }
                for n in &v.nodes {
                    if n.start_layer > n.end_layer {
                        bail!(
                            "version {}: {} has an inverted range ({}-{})",
                            v.version,
                            n.node_id,
                            n.start_layer,
                            n.end_layer
                        );
                    }
                }
            }
            if !seen.contains(&self.active_version) {
                bail!(
                    "active_version {} is not among the versions {:?}",
                    self.active_version,
                    seen
                );
            }
        }
        Ok(())
    }
}

/// A implantação de mesma **forma** que uma nova requisição: mesmo `kind`,
/// mesma identidade de modelo provada e o mesmo conjunto exato de
/// `(node_id, start_layer, end_layer, tp_rank)`. É o que decide se um novo
/// `POST /api/ui/deploy` reaproveita um `deploy_id` (anexando uma versão) ou
/// cunha um id novo.
///
/// Função pura como `active`: nada de rede, para ser testável e para os nós
/// convergirem sem protocolo.
pub fn matching_shape<'a>(
    deployments: &'a [Deployment],
    kind: DeploymentKind,
    identity: &DeploymentIdentity,
    ranges: &[(String, u32, u32, Option<u32>)],
) -> Option<&'a Deployment> {
    let mut target: Vec<(String, u32, u32, Option<u32>)> = ranges.to_vec();
    target.sort();
    deployments.iter().find(|d| {
        if d.kind != kind || d.model.identity.as_ref() != Some(identity) {
            return false;
        }
        let mut own: Vec<(String, u32, u32, Option<u32>)> = d
            .nodes
            .iter()
            .map(|n| (n.node_id.clone(), n.start_layer, n.end_layer, n.tp_rank))
            .collect();
        own.sort();
        own == target
    })
}

/// Qual implantação atende uma requisição que só diz o nome do modelo.
///
/// A mais recentemente ativada entre as daquele rótulo. Empate resolvido pelo
/// `deploy_id`, para que a resposta não dependa da ordem em que os registros
/// foram lidos do disco - duas máquinas têm que chegar à mesma conclusão.
pub fn active<'a>(deployments: &'a [Deployment], label: &str) -> Option<&'a Deployment> {
    deployments
        .iter()
        .filter(|d| d.model.label == label && !d.nodes.is_empty())
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
        Some((name, id)) if is_deploy_id(id) => (name, Some(id)),
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
        bail!("invalid deploy_id: {deploy_id:?}");
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
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;

    let destination = path_in(dir, &d.deploy_id)?;
    let temp = dir.join(format!(".{}.{}.tmp", d.deploy_id, std::process::id()));
    let json = serde_json::to_vec_pretty(d).context("serialize deployment")?;
    {
        let mut f =
            std::fs::File::create(&temp).with_context(|| format!("create {}", temp.display()))?;
        f.write_all(&json)?;
        f.sync_all()?;
    }
    if let Err(e) = std::fs::rename(&temp, &destination) {
        let _ = std::fs::remove_file(&temp);
        return Err(e).with_context(|| format!("rename to {}", destination.display()));
    }
    Ok(destination)
}

#[allow(dead_code)]
pub fn save(d: &Deployment) -> Result<PathBuf> {
    save_in(&dir(), d)
}

#[allow(dead_code)]
pub fn load(path: &Path) -> Result<Deployment> {
    let raw = std::fs::read(path).with_context(|| format!("ler {}", path.display()))?;
    let d: Deployment =
        serde_json::from_slice(&raw).with_context(|| format!("parse {}", path.display()))?;
    d.validate()
        .with_context(|| format!("invalid record in {}", path.display()))?;
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
            Err(e) => tracing::warn!("ignoring record {}: {e:#}", p.display()),
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

    fn no(id: &str, start: u32, end: u32, index: u32, total: u32) -> DeploymentNode {
        DeploymentNode {
            node_id: id.into(),
            hostname: id.into(),
            start_layer: start,
            end_layer: end,
            peer_index: index,
            peer_count: total,
            primary: index == 0,
            part_sha256: None,
            source_model_path: None,
            tp_rank: None,
            config: crate::webui::RuntimeParams::default(),
        }
    }

    fn deployment(id: &str, label: &str, nos: Vec<DeploymentNode>) -> Deployment {
        Deployment::new(
            id.into(),
            DeploymentKind::Shards,
            DeploymentModel {
                label: label.into(),
                total_layers: 32,
                pom_id: None,
                identity: None,
            },
            "coordenador",
            nos,
        )
    }

    #[test]
    fn a_new_id_is_valid_and_does_not_repeat() {
        let a = new_deploy_id();
        let b = new_deploy_id();
        assert!(is_deploy_id(&a), "{a}");
        assert_ne!(a, b);
    }

    /// Um id vindo da rede vira nome de arquivo: se `..` passasse, um peer
    /// escolheria onde gravar no disco de quem recebe.
    #[test]
    fn an_id_that_is_not_an_id_does_not_become_a_path() {
        assert!(!is_deploy_id("../../etc/passwd"));
        assert!(!is_deploy_id("d-../../x"));
        assert!(!is_deploy_id("d-ABCDEF12"), "hex is lowercase");
        assert!(!is_deploy_id("d-123"), "too short");
        assert!(!is_deploy_id("4f2a91c3"), "without a prefix");
        assert!(path_in(Path::new("/tmp/d"), "../x").is_err());
    }

    /// O caso que não existia: **o mesmo modelo**, duas implantações.
    #[test]
    fn two_deployments_of_the_same_model_coexist() {
        let one = deployment("d-aaaaaaaa", "qwen3:8b", vec![no("n1", 0, 31, 0, 1)]);
        let mut other = deployment(
            "d-bbbbbbbb",
            "qwen3:8b",
            vec![no("n2", 0, 15, 0, 2), no("n3", 16, 31, 1, 2)],
        );
        other.activated_at = "2030-01-01T00:00:00Z".into();

        let all = vec![one, other];
        assert_eq!(
            active(&all, "qwen3:8b").unwrap().deploy_id,
            "d-bbbbbbbb",
            "the active one is the one with the most recent activation"
        );
        assert!(active(&all, "nao-existe").is_none());
    }

    /// Empate tem que ser resolvido igual nas duas máquinas, senão uma manda a
    /// requisição para uma implantação e a outra para a outra.
    #[test]
    fn an_activation_tie_is_resolved_stably() {
        let mut a = deployment("d-aaaaaaaa", "m", vec![no("n1", 0, 31, 0, 1)]);
        let mut b = deployment("d-bbbbbbbb", "m", vec![no("n2", 0, 31, 0, 1)]);
        a.activated_at = "2026-01-01T00:00:00Z".into();
        b.activated_at = "2026-01-01T00:00:00Z".into();

        let in_one_order = vec![a.clone(), b.clone()];
        let on_the_other = vec![b, a];
        assert_eq!(
            active(&in_one_order, "m").unwrap().deploy_id,
            active(&on_the_other, "m").unwrap().deploy_id
        );
    }

    #[test]
    fn the_selector_separates_model_from_deployment() {
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
    fn layer_coverage() {
        assert!(deployment("d-aaaaaaaa", "m", vec![no("n1", 0, 31, 0, 1)]).covers_the_model());
        assert!(deployment(
            "d-aaaaaaaa",
            "m",
            vec![no("n1", 0, 15, 0, 2), no("n2", 16, 31, 1, 2)]
        )
        .covers_the_model());
        // Buraco em 16-19.
        assert!(!deployment(
            "d-aaaaaaaa",
            "m",
            vec![no("n1", 0, 15, 0, 2), no("n2", 20, 31, 1, 2)]
        )
        .covers_the_model());
    }

    #[test]
    fn write_reread_and_forget() {
        let tmp =
            std::env::temp_dir().join(format!("pom-deploy-{}", uuid::Uuid::new_v4().simple()));
        let d = deployment(
            "d-4f2a91c3",
            "qwen3:8b",
            vec![no("n1", 0, 15, 0, 2), no("n2", 16, 31, 1, 2)],
        );

        let written = save_in(&tmp, &d).unwrap();
        assert_eq!(load(&written).unwrap(), d);
        assert_eq!(load_dir(&tmp), vec![d.clone()]);

        remove_in(&tmp, &d.deploy_id).unwrap();
        assert!(load_dir(&tmp).is_empty());
        // Esquecer o que já foi esquecido não é erro.
        remove_in(&tmp, &d.deploy_id).unwrap();

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn a_record_without_a_node_or_with_a_malformed_id_is_refused() {
        let mut d = deployment("d-4f2a91c3", "m", vec![no("n1", 0, 31, 0, 1)]);
        d.nodes.clear();
        assert!(d.validate().is_err());

        let mut d = deployment("d-4f2a91c3", "m", vec![no("n1", 0, 31, 0, 1)]);
        d.deploy_id = "nao-e-id".into();
        assert!(d.validate().is_err());
    }

    fn params_with_ctx(n_ctx: u32) -> crate::webui::RuntimeParams {
        crate::webui::RuntimeParams {
            n_ctx,
            ..Default::default()
        }
    }

    /// Um registro v1 legado chega sem `versions`; a v1 é sintetizada de `nodes`.
    #[test]
    fn ensure_versions_synthesizes_the_v1_of_a_legacy_record() {
        let mut d = deployment("d-aaaaaaaa", "m", vec![no("n1", 0, 31, 0, 1)]);
        d.versions.clear();
        d.active_version = 7; // valor lixo de um registro antigo

        d.ensure_versions();

        assert_eq!(d.versions.len(), 1);
        assert_eq!(d.versions[0].version, 1);
        assert_eq!(d.active_version, 1);
        assert_eq!(d.versions[0].nodes, d.nodes);
        // Idempotente.
        d.ensure_versions();
        assert_eq!(d.versions.len(), 1);
    }

    #[test]
    fn upsert_version_reuses_the_version_with_equal_params_and_appends_when_they_differ() {
        let mut d = deployment("d-aaaaaaaa", "m", vec![no("n1", 0, 31, 0, 1)]);
        d.versions[0].params = params_with_ctx(8192);
        d.sync_active_nodes();
        let enabled_v1 = d.activated_at.clone();

        // Mesmos params -> mesma versão, sem anexar.
        std::thread::sleep(std::time::Duration::from_millis(2));
        let v = d.upsert_version(params_with_ctx(8192), d.nodes.clone(), "n1");
        assert_eq!(v, 1);
        assert_eq!(d.versions.len(), 1);
        assert_ne!(d.activated_at, enabled_v1, "activated_at restamped");

        // Params diferentes -> v2, ativa, nodes sincronizados.
        let nodes_v2 = vec![no("n1", 0, 31, 0, 1)];
        let v = d.upsert_version(params_with_ctx(16384), nodes_v2.clone(), "n1");
        assert_eq!(v, 2);
        assert_eq!(d.versions.len(), 2);
        assert_eq!(d.active_version, 2);
        assert_eq!(d.nodes, nodes_v2);
        assert_eq!(d.active().unwrap().params.n_ctx, 16384);
    }

    #[test]
    fn append_version_records_a_topology_change_even_with_equal_params() {
        let mut d = deployment("d-aaaaaaaa", "m", vec![no("n1", 0, 31, 0, 1)]);
        let params = d.active().unwrap().params.clone();
        let nodes = vec![no("n1", 0, 31, 0, 1), no("n2", 0, 31, 0, 1)];

        let version = d.append_version(params.clone(), nodes.clone(), "operador");

        assert_eq!(version, 2);
        assert_eq!(d.active_version, 2);
        assert_eq!(d.nodes, nodes);
        assert_eq!(d.versions.len(), 2);
        assert_eq!(d.versions[1].params, params);
        assert_eq!(d.versions[1].created_by, "operador");
    }

    #[test]
    fn a_mixed_deployment_covers_both_heads_and_the_last_removal_is_valid() {
        let mut d = deployment(
            "d-aaaaaaaa",
            "m",
            vec![no("slices", 0, 15, 0, 2), no("cauda", 16, 31, 1, 2)],
        );
        d.kind = DeploymentKind::Mixed;
        let whole = no("inteiro", 0, 31, 0, 1);

        let version = d.append_version(
            d.active().unwrap().params.clone(),
            vec![whole, d.nodes[0].clone(), d.nodes[1].clone()],
            "operador",
        );
        assert_eq!(version, 2);
        assert!(d.covers_the_model());

        let version = d.append_version(
            d.active().unwrap().params.clone(),
            vec![d.nodes[0].clone(), d.nodes[1].clone()],
            "operador",
        );
        assert_eq!(version, 3);
        assert!(d.validate().is_ok());
    }

    #[test]
    fn a_deployment_without_machines_is_only_inactive_in_the_default_selection() {
        let mut d = deployment("d-aaaaaaaa", "m", vec![no("n1", 0, 31, 0, 1)]);
        d.append_version(d.active().unwrap().params.clone(), Vec::new(), "operador");

        assert!(d.nodes.is_empty());
        assert!(d.validate().is_ok());
        assert!(active(&[d], "m").is_none());
    }

    #[test]
    fn set_active_version_swaps_the_nodes_and_refuses_a_missing_version() {
        let mut d = deployment("d-aaaaaaaa", "m", vec![no("n1", 0, 31, 0, 1)]);
        d.upsert_version(params_with_ctx(16384), vec![no("n1", 0, 15, 0, 1)], "n1");
        assert_eq!(d.active_version, 2);

        d.set_active_version(1).unwrap();
        assert_eq!(d.active_version, 1);
        assert_eq!(d.nodes[0].end_layer, 31);

        assert!(d.set_active_version(9).is_err());
    }

    #[test]
    fn validate_refuses_a_repeated_version_and_an_orphan_active() {
        let mut d = deployment("d-aaaaaaaa", "m", vec![no("n1", 0, 31, 0, 1)]);
        d.upsert_version(params_with_ctx(16384), d.nodes.clone(), "n1");

        let mut repeated = d.clone();
        repeated.versions[1].version = 1;
        assert!(repeated.validate().is_err());

        let mut orphan = d.clone();
        orphan.active_version = 99;
        assert!(orphan.validate().is_err());
    }

    #[test]
    fn matching_shape_matches_by_identity_and_ranges() {
        let mut d = deployment(
            "d-aaaaaaaa",
            "m",
            vec![no("n1", 0, 15, 0, 2), no("n2", 16, 31, 1, 2)],
        );
        d.model.identity = Some(DeploymentIdentity::PomId {
            pom_id: "pom-1".into(),
        });
        let all = vec![d];
        let id = DeploymentIdentity::PomId {
            pom_id: "pom-1".into(),
        };

        // Mesma forma, ordem dos alvos trocada -> casa.
        let ranges = vec![
            ("n2".to_string(), 16, 31, None),
            ("n1".to_string(), 0, 15, None),
        ];
        assert!(matching_shape(&all, DeploymentKind::Shards, &id, &ranges).is_some());

        // Uma faixa diferente -> não casa.
        let others = vec![
            ("n1".to_string(), 0, 19, None),
            ("n2".to_string(), 20, 31, None),
        ];
        assert!(matching_shape(&all, DeploymentKind::Shards, &id, &others).is_none());

        // Identidade diferente -> não casa.
        let other_id = DeploymentIdentity::PomId {
            pom_id: "pom-2".into(),
        };
        assert!(matching_shape(&all, DeploymentKind::Shards, &other_id, &ranges).is_none());
    }

    /// Um registro sem `identity` (rodada anterior à prova) nunca deduplica.
    #[test]
    fn matching_shape_ignores_a_record_without_identity() {
        let d = deployment("d-aaaaaaaa", "m", vec![no("n1", 0, 31, 0, 1)]);
        assert!(d.model.identity.is_none());
        let all = vec![d];
        let id = DeploymentIdentity::PomId {
            pom_id: "pom-1".into(),
        };
        let ranges = vec![("n1".to_string(), 0, 31, None)];
        assert!(matching_shape(&all, DeploymentKind::Shards, &id, &ranges).is_none());
    }

    #[test]
    fn versions_survive_serde() {
        let mut d = deployment("d-aaaaaaaa", "m", vec![no("n1", 0, 31, 0, 1)]);
        d.upsert_version(params_with_ctx(16384), d.nodes.clone(), "n1");
        let json = serde_json::to_string(&d).unwrap();
        let loaded: Deployment = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded, d);
        assert_eq!(loaded.versions.len(), 2);
        assert_eq!(loaded.active_version, 2);
    }
}
