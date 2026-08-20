//! O manifesto de um fatiamento: o `.pom`.
//!
//! Antes disto a identidade de uma fatia era o **nome do arquivo**,
//! `<base>_<inicio>_<fim>.gguf`, montado num canto e lido de volta em outro. Um
//! modelo cujo nome já terminasse em dois números deixava a faixa ambígua, o
//! nome do modelo só existia no nome do arquivo e na pasta que o continha, e
//! dois modelos diferentes cortados na mesma faixa eram indistinguíveis para
//! quem escolhe a máquina de destino. Nada, em nenhum ponto, conferia que os
//! bytes eram os que diziam ser.
//!
//! Aqui a identidade passa a ser o **conteúdo**: cada parte é gravada com o
//! próprio sha256 por nome, e o `.pom` é a lista dessas partes — o papel que um
//! `.torrent` faz. Quem implanta manda o manifesto (alguns kB) e a lista de
//! hashes que cada nó tem que carregar; o nó busca as partes que faltam de
//! **qualquer** peer que as tenha, e confere o hash que pediu contra os bytes
//! que chegaram.
//!
//! No formato v2, o `pom_id` é o sha256 do GGUF original. Uma pasta por modelo
//! preserva todas as alternativas de fatiamento e também os shards de Tensor
//! Parallelism no campo separado `tp_parts`. O formato v1 continua legível para
//! compatibilidade com manifestos antigos.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Versão do formato novo. Sobe quando o significado de um campo muda.
pub const POM_VERSION: u32 = 2;
const LEGACY_POM_VERSION: u32 = 1;

/// Extensão do manifesto.
pub const POM_EXT: &str = "pom";

/// Como o modelo foi cortado.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PomKind {
    /// Corte por camadas: cada parte cobre `start_layer..=end_layer`.
    Layers,
    /// Corte por tensor: cada parte é um rank da largura total.
    TensorParallel,
    /// O manifesto contém fatias por camadas e shards de tensor.
    Combined,
}

/// A que rank de um grupo tensor-parallel uma parte pertence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PomTp {
    pub rank: u32,
    pub size: u32,
}

/// Uma parte: um arquivo GGUF, nomeado pelo próprio hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PomPart {
    /// sha256 do arquivo, em hex minúsculo. É também o nome dele em disco.
    pub sha256: String,
    pub bytes: u64,
    pub start_layer: u32,
    pub end_layer: u32,
    /// Traz os embeddings.
    #[serde(default)]
    pub is_first: bool,
    /// Traz a cabeça de saída.
    #[serde(default)]
    pub is_last: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tp: Option<PomTp>,
}

/// De qual modelo estas partes saíram.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PomModel {
    /// Nome curto, também o nome da pasta em disco.
    pub name: String,
    /// Rótulo que o planejador de pipeline casa entre peers.
    pub label: String,
    pub total_layers: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quant: Option<String>,
}

/// O manifesto de um modelo original e das partes disponíveis localmente.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PomManifest {
    pub pom_version: u32,
    /// sha256 da lista canônica de partes. Ver [`pom_id`].
    pub pom_id: String,
    pub model: PomModel,
    pub kind: PomKind,
    pub created_at: String,
    /// `node_id` de quem coordenou a rodada.
    #[serde(default)]
    pub created_by: String,
    pub parts: Vec<PomPart>,
    /// Shards de Tensor Parallelism do mesmo GGUF original.
    #[serde(default)]
    pub tp_parts: Vec<PomPart>,
}

/// O identificador legado: sha256 sobre as partes, em ordem canônica.
///
/// Canônico quer dizer ordenado por hash e serializado sem espaço em branco, de
/// modo que duas máquinas que produziram as mesmas partes cheguem ao mesmo
/// `pom_id` mesmo tendo montado a lista em ordens diferentes — que é o caso
/// normal, já que cada nó fatia a própria parte e as respostas voltam fora de
/// ordem.
pub fn pom_id(parts: &[PomPart]) -> String {
    let mut ordenadas: Vec<&PomPart> = parts.iter().collect();
    ordenadas.sort_by(|a, b| a.sha256.cmp(&b.sha256));

    let mut h = Sha256::new();
    for p in ordenadas {
        // Campos crus em vez de JSON: o hash não pode mudar porque um campo
        // novo entrou no struct ou porque o serde mudou de ordem.
        h.update(p.sha256.as_bytes());
        h.update(b"\0");
        h.update(p.bytes.to_le_bytes());
        h.update(p.start_layer.to_le_bytes());
        h.update(p.end_layer.to_le_bytes());
        h.update([p.is_first as u8, p.is_last as u8]);
        if let Some(tp) = p.tp {
            h.update(tp.rank.to_le_bytes());
            h.update(tp.size.to_le_bytes());
        }
        h.update(b"\n");
    }
    hex::encode(h.finalize())
}

impl PomManifest {
    /// Monta um manifesto e calcula o `pom_id` a partir das partes.
    pub fn new(
        model: PomModel,
        kind: PomKind,
        created_by: impl Into<String>,
        parts: Vec<PomPart>,
    ) -> Self {
        Self {
            // Este construtor permanece para ler e produzir fixtures do formato
            // antigo. Os fluxos novos usam `new_for_source` abaixo.
            pom_version: LEGACY_POM_VERSION,
            pom_id: pom_id(&parts),
            model,
            kind,
            created_at: chrono::Utc::now().to_rfc3339(),
            created_by: created_by.into(),
            parts,
            tp_parts: Vec::new(),
        }
    }

    /// Monta um manifesto v2 identificado pelo arquivo GGUF original.
    pub fn new_for_source(
        model: PomModel,
        kind: PomKind,
        created_by: impl Into<String>,
        source_sha256: impl Into<String>,
        parts: Vec<PomPart>,
    ) -> Self {
        Self {
            pom_version: POM_VERSION,
            pom_id: source_sha256.into(),
            model,
            kind,
            created_at: chrono::Utc::now().to_rfc3339(),
            created_by: created_by.into(),
            parts,
            tp_parts: Vec::new(),
        }
    }

    /// Une o conhecimento de duas máquinas sobre o mesmo modelo original.
    ///
    /// O manifesto local é a base: uma parte que só existe nele nunca é
    /// removida porque o peer remoto ainda não a conhece.
    pub fn merge_from(&mut self, incoming: PomManifest) -> Result<()> {
        if self.pom_version != POM_VERSION || incoming.pom_version != POM_VERSION {
            bail!("merge exige manifestos v2");
        }
        if self.pom_id != incoming.pom_id {
            bail!(
                "manifestos pertencem a modelos diferentes: {} e {}",
                &self.pom_id[..12.min(self.pom_id.len())],
                &incoming.pom_id[..12.min(incoming.pom_id.len())]
            );
        }
        self.validate()?;
        incoming.validate()?;
        if self.model.total_layers != incoming.model.total_layers {
            bail!(
                "manifestos do modelo {} declaram totais de camadas diferentes: {} e {}",
                self.model.name,
                self.model.total_layers,
                incoming.model.total_layers
            );
        }
        if self.model.quant.is_some()
            && incoming.model.quant.is_some()
            && self.model.quant != incoming.model.quant
        {
            bail!(
                "manifestos do modelo {} declaram quantizações diferentes",
                self.model.name
            );
        }

        merge_parts(&mut self.parts, incoming.parts, "parts")?;
        merge_parts(&mut self.tp_parts, incoming.tp_parts, "tp_parts")?;
        if self.model.label.trim().is_empty() {
            self.model.label = incoming.model.label;
        }
        if self.created_by.trim().is_empty() {
            self.created_by = incoming.created_by;
        }
        self.kind = kind_for(&self.parts, &self.tp_parts);
        self.validate()
    }

    /// As partes cobrem `0..total_layers` sem buraco nem sobreposição?
    ///
    /// Um manifesto incompleto é legítimo enquanto a rodada não terminou; quem
    /// implanta é que não pode subir um pipeline furado.
    pub fn complete(&self) -> bool {
        if self.pom_version == LEGACY_POM_VERSION && self.kind == PomKind::TensorParallel {
            return tp_complete_from(&self.parts, None);
        }
        let mut proxima = 0u32;
        loop {
            let Some(fim) = self
                .parts
                .iter()
                .filter(|p| p.start_layer == proxima)
                .map(|p| p.end_layer)
                .max()
            else {
                return false;
            };
            proxima = fim.saturating_add(1);
            if proxima >= self.model.total_layers {
                return proxima == self.model.total_layers;
            }
        }
    }

    /// Diz se um grupo TP de tamanho `size` tem todos os ranks.
    pub fn tp_complete(&self, size: u32) -> bool {
        let partes = if self.pom_version == LEGACY_POM_VERSION {
            &self.parts
        } else {
            &self.tp_parts
        };
        tp_complete_from(partes, Some(size))
    }

    /// Confere o que dá para conferir sem ler os arquivos.
    pub fn validate(&self) -> Result<()> {
        if self.pom_version > POM_VERSION {
            bail!(
                "manifesto versão {} é mais novo que esta build (suporta até {POM_VERSION})",
                self.pom_version
            );
        }
        if self.model.name.trim().is_empty() {
            bail!("manifesto sem nome de modelo");
        }
        if self.parts.is_empty() && self.tp_parts.is_empty() {
            bail!("manifesto sem partes");
        }
        for p in self.parts.iter().chain(self.tp_parts.iter()) {
            if !is_sha256_hex(&p.sha256) {
                bail!("parte com hash inválido: {:?}", p.sha256);
            }
            if p.start_layer > p.end_layer {
                bail!(
                    "parte {} tem faixa invertida ({}-{})",
                    &p.sha256[..12],
                    p.start_layer,
                    p.end_layer
                );
            }
        }
        if self.pom_version == LEGACY_POM_VERSION {
            // O formato antigo derivava o id da lista de partes.
            let esperado = pom_id(&self.parts);
            if self.pom_id != esperado {
                bail!(
                    "pom_id não confere: o arquivo diz {}, as partes dão {}",
                    &self.pom_id[..12.min(self.pom_id.len())],
                    &esperado[..12]
                );
            }
            return Ok(());
        }
        if self.pom_version != POM_VERSION || !is_sha256_hex(&self.pom_id) {
            bail!("pom_id v2 inválido: {:?}", self.pom_id);
        }
        if self.parts.iter().any(|p| p.tp.is_some()) {
            bail!("parts não pode conter shards de Tensor Parallelism");
        }
        if self.tp_parts.iter().any(|p| p.tp.is_none()) {
            bail!("tp_parts exige rank e tamanho do grupo TP");
        }
        let esperado = kind_for(&self.parts, &self.tp_parts);
        if self.kind != esperado {
            bail!(
                "kind do manifesto não confere com as partes: {:?}, esperado {:?}",
                self.kind,
                esperado
            );
        }
        Ok(())
    }
}

fn kind_for(parts: &[PomPart], tp_parts: &[PomPart]) -> PomKind {
    match (parts.is_empty(), tp_parts.is_empty()) {
        (false, true) => PomKind::Layers,
        (true, false) => PomKind::TensorParallel,
        (false, false) => PomKind::Combined,
        (true, true) => PomKind::Layers,
    }
}

fn merge_parts(target: &mut Vec<PomPart>, incoming: Vec<PomPart>, field: &str) -> Result<()> {
    for part in incoming {
        if let Some(existing) = target.iter().find(|p| p.sha256 == part.sha256) {
            if existing != &part {
                bail!(
                    "{field} tem metadados diferentes para a parte {}",
                    &part.sha256[..12]
                );
            }
            continue;
        }
        target.push(part);
    }
    target.sort_by(|a, b| {
        (a.start_layer, a.end_layer, &a.sha256).cmp(&(b.start_layer, b.end_layer, &b.sha256))
    });
    Ok(())
}

fn tp_complete_from(parts: &[PomPart], requested_size: Option<u32>) -> bool {
    let Some(size) = requested_size.or_else(|| parts.first().and_then(|p| p.tp).map(|t| t.size))
    else {
        return false;
    };
    let mut ranks: Vec<u32> = parts
        .iter()
        .filter_map(|p| p.tp)
        .filter(|tp| tp.size == size)
        .map(|tp| tp.rank)
        .collect();
    ranks.sort_unstable();
    ranks.dedup();
    ranks == (0..size).collect::<Vec<_>>()
}

/// Um hash é um hash: 64 dígitos hex minúsculos e nada mais.
///
/// Vale como validação de entrada **e** como defesa de caminho: é isto que
/// garante que um `sha256` vindo da rede nunca vire `../` ao virar nome de
/// arquivo em [`part_path`].
pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Onde uma parte mora: `<dir do modelo>/<sha256>.gguf`.
///
/// A extensão fica para que o `catalog::find_gguf` e o carregador do llama.cpp
/// não precisem de exceção nenhuma; quem identifica é o radical.
pub fn part_path(model_dir: &Path, sha256: &str) -> Result<PathBuf> {
    if !is_sha256_hex(sha256) {
        bail!("hash de parte inválido: {sha256:?}");
    }
    Ok(model_dir.join(format!("{sha256}.gguf")))
}

/// O identificador curto de uma rodada, o que aparece em nome de pasta e de
/// arquivo. Oito dígitos hex do `pom_id`.
pub fn short_id(pom_id: &str) -> &str {
    &pom_id[..8.min(pom_id.len())]
}

/// A pasta de um manifesto legado: `<modelo>-<id da rodada>`.
///
/// O id no nome não é enfeite. Fatiar o mesmo modelo duas vezes com cortes
/// diferentes produz dois conjuntos de partes que nada têm a ver um com o
/// outro; numa pasta só eles se misturavam, e apagar uma rodada levava junto
/// as partes que por acaso a outra também usava. Uma pasta por rodada faz de
/// "apagar este fatiamento" um `rm -r` que não atinge mais ninguém.
///
/// Derivado do conteúdo, e não sorteado: refatiar exatamente o mesmo modelo do
/// mesmo jeito dá o mesmo `pom_id`, logo a mesma pasta - que é o certo, porque
/// é a mesma coisa - e a rodada não se duplica em disco. É também o que faz
/// duas máquinas guardarem o mesmo fatiamento no mesmo caminho relativo.
pub fn run_dir(shard_dir: &Path, model_name: &str, pom_id: &str) -> PathBuf {
    shard_dir.join(format!(
        "{}-{}",
        sanitize_name(model_name),
        short_id(pom_id)
    ))
}

/// A pasta v2 identificada pelo início do hash completo do GGUF original.
pub fn source_run_dir(shard_dir: &Path, model_name: &str, source_sha256: &str) -> PathBuf {
    shard_dir.join(format!(
        "{}-{}",
        sanitize_name(model_name),
        short_id(source_sha256)
    ))
}

/// Procura a pasta v2 pelo hash completo, sem confiar no nome legível.
pub fn locate_source_dir(shard_dir: &Path, source_sha256: &str) -> Option<PathBuf> {
    if !is_sha256_hex(source_sha256) {
        return None;
    }
    for entry in std::fs::read_dir(shard_dir).ok()?.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if load_dir(&path)
            .iter()
            .any(|m| m.pom_version == POM_VERSION && m.pom_id == source_sha256)
        {
            return Some(path);
        }
    }
    None
}

/// A pasta da rodada que este manifesto descreve.
pub fn dir_for(shard_dir: &Path, m: &PomManifest) -> PathBuf {
    if m.pom_version == POM_VERSION {
        source_run_dir(shard_dir, &m.model.name, &m.pom_id)
    } else {
        run_dir(shard_dir, &m.model.name, &m.pom_id)
    }
}

/// O arquivo do manifesto dentro da pasta da rodada.
pub fn manifest_path(model_dir: &Path, m: &PomManifest) -> PathBuf {
    if m.pom_version == POM_VERSION {
        model_dir.join(format!("model.{POM_EXT}"))
    } else {
        model_dir.join(format!(
            "{}-{}.{POM_EXT}",
            sanitize_name(&m.model.name),
            short_id(&m.pom_id)
        ))
    }
}

struct PomLock(PathBuf);

impl Drop for PomLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn acquire_pom_lock(shard_dir: &Path, source_sha256: &str) -> Result<PomLock> {
    std::fs::create_dir_all(shard_dir)
        .with_context(|| format!("criar diretório de fatias {}", shard_dir.display()))?;
    let path = shard_dir.join(format!(".pom-{source_sha256}.lock"));
    for _ in 0..500 {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => return Ok(PomLock(path)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => return Err(e).with_context(|| format!("criar lock {}", path.display())),
        }
    }
    bail!(
        "tempo esgotado esperando o lock do manifesto {}",
        source_sha256
    )
}

/// Mescla um manifesto v2 no inventário local do mesmo modelo original.
pub fn merge_manifest(shard_dir: &Path, incoming: &PomManifest) -> Result<(PathBuf, PomManifest)> {
    if incoming.pom_version != POM_VERSION {
        bail!("merge exige manifesto v2");
    }
    incoming.validate()?;
    let _lock = acquire_pom_lock(shard_dir, &incoming.pom_id)?;
    let destino = locate_source_dir(shard_dir, &incoming.pom_id)
        .unwrap_or_else(|| source_run_dir(shard_dir, &incoming.model.name, &incoming.pom_id));
    std::fs::create_dir_all(&destino)
        .with_context(|| format!("criar diretório do modelo {}", destino.display()))?;

    let existente = load_dir(&destino)
        .into_iter()
        .find(|m| m.pom_version == POM_VERSION && m.pom_id == incoming.pom_id);
    let mut agregado = existente.unwrap_or_else(|| incoming.clone());
    if agregado.parts != incoming.parts || agregado.tp_parts != incoming.tp_parts {
        agregado.merge_from(incoming.clone())?;
    }
    write_atomic(&destino, &agregado)?;
    Ok((destino, agregado))
}

/// Move as partes de uma pasta de trabalho para a pasta do modelo e mescla o
/// manifesto. Em v2, a identidade já vem do hash do GGUF original, então uma
/// nova forma de particionar o mesmo modelo adiciona partes à mesma pasta.
/// Partes repetidas são descartadas por hash.
pub fn finalize_run(shard_dir: &Path, temp: &Path, m: &PomManifest) -> Result<PathBuf> {
    m.validate()?;
    let destino = if m.pom_version == POM_VERSION {
        locate_source_dir(shard_dir, &m.pom_id)
            .unwrap_or_else(|| source_run_dir(shard_dir, &m.model.name, &m.pom_id))
    } else {
        dir_for(shard_dir, m)
    };
    std::fs::create_dir_all(&destino).with_context(|| format!("criar {}", destino.display()))?;

    for parte in m.parts.iter().chain(m.tp_parts.iter()) {
        let de = part_path(temp, &parte.sha256)?;
        let para = part_path(&destino, &parte.sha256)?;
        if !de.is_file() {
            // A parte foi reaproveitada de uma rodada anterior e nunca passou
            // pela pasta de trabalho: nada a mover.
            continue;
        }
        if para.is_file() {
            let _ = std::fs::remove_file(&de);
            continue;
        }
        std::fs::rename(&de, &para)
            .with_context(|| format!("mover {} para {}", de.display(), para.display()))?;
    }

    if m.pom_version == POM_VERSION {
        let (destino, _) = merge_manifest(shard_dir, m)?;
        let _ = std::fs::remove_dir(temp);
        return Ok(destino);
    }
    write_atomic(&destino, m)?;
    // A pasta de trabalho só some se estiver vazia: um arquivo inesperado ali
    // é sinal de defeito, e apagá-lo escondería a evidência.
    let _ = std::fs::remove_dir(temp);
    Ok(destino)
}

/// Uma pasta de trabalho para uma rodada que ainda não tem nome.
pub fn scratch_dir(shard_dir: &Path) -> PathBuf {
    shard_dir.join(format!(".fatiando-{}", uuid::Uuid::new_v4().simple()))
}

/// Nome que pode virar pasta sem surpresa.
pub fn sanitize_name(name: &str) -> String {
    let limpo: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let limpo = limpo.trim_matches(['-', '.', '_']).to_string();
    if limpo.is_empty() {
        "modelo".to_string()
    } else {
        limpo
    }
}

/// Grava o manifesto sem deixar arquivo pela metade se a máquina cair no meio.
///
/// Mesmo padrão do merge em `webui.rs`: escreve num temporário ao lado e só
/// então renomeia, porque `rename` no mesmo sistema de arquivos é atômico e um
/// `.pom` truncado seria lido como "modelo sem partes".
pub fn write_atomic(model_dir: &Path, m: &PomManifest) -> Result<PathBuf> {
    m.validate()?;
    std::fs::create_dir_all(model_dir).with_context(|| format!("criar {}", model_dir.display()))?;

    let destino = manifest_path(model_dir, m);
    let temp = model_dir.join(format!(".{}.{}.tmp", m.pom_id, std::process::id()));

    let json = serde_json::to_vec_pretty(m).context("serializar manifesto")?;
    {
        let mut f =
            std::fs::File::create(&temp).with_context(|| format!("criar {}", temp.display()))?;
        f.write_all(&json)?;
        // Sem isto o rename pode chegar ao disco antes do conteúdo.
        f.sync_all()?;
    }
    if let Err(e) = std::fs::rename(&temp, &destino) {
        let _ = std::fs::remove_file(&temp);
        return Err(e).with_context(|| format!("renomear para {}", destino.display()));
    }
    Ok(destino)
}

/// Lê e valida um manifesto.
pub fn load(path: &Path) -> Result<PomManifest> {
    let raw = std::fs::read(path).with_context(|| format!("ler {}", path.display()))?;
    let m: PomManifest =
        serde_json::from_slice(&raw).with_context(|| format!("interpretar {}", path.display()))?;
    m.validate()
        .with_context(|| format!("manifesto inválido em {}", path.display()))?;
    Ok(m)
}

/// Todos os manifestos de uma pasta de modelo.
pub fn load_dir(model_dir: &Path) -> Vec<PomManifest> {
    let Ok(rd) = std::fs::read_dir(model_dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some(POM_EXT) {
            continue;
        }
        match load(&p) {
            Ok(m) => out.push(m),
            // Um manifesto quebrado não pode derrubar a varredura inteira: o
            // resto da pasta continua utilizável.
            Err(e) => tracing::warn!("ignorando manifesto {}: {e:#}", p.display()),
        }
    }
    out
}

/// Localiza o manifesto v2 que conhece a parte armazenada neste caminho.
///
/// Uma parte pode ter chegado por transferência ou ter sido produzida por uma
/// junção. Em ambos os casos o diretório pai é a fonte de verdade do modelo
/// original; não devemos calcular o `pom_id` a partir da parte resultante.
pub fn manifest_for_part_path(path: &Path) -> Option<PomManifest> {
    let sha256 = path.file_stem()?.to_str()?;
    if !is_sha256_hex(sha256) {
        return None;
    }
    load_dir(path.parent()?).into_iter().find(|manifest| {
        manifest.pom_version == POM_VERSION
            && manifest
                .parts
                .iter()
                .chain(manifest.tp_parts.iter())
                .any(|part| part.sha256 == sha256)
    })
}

// ── Hash em streaming ────────────────────────────────────────────────────────

/// Um `Write` que vai calculando o sha256 do que passa por ele.
///
/// Existe para que o hash da fatia **não** custe uma segunda leitura do
/// arquivo: fatia de modelo tem dezenas de GB, e reler tudo só para hashear
/// dobrava o tempo do fatiamento. Envolvendo o writer do `splitter`, o digest
/// sai pronto no mesmo passe em que os bytes vão para o disco.
pub struct HashingWriter<W: Write> {
    inner: W,
    hasher: Sha256,
    bytes: u64,
}

impl<W: Write> HashingWriter<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            bytes: 0,
        }
    }

    /// Consome o writer e devolve `(sha256 em hex, bytes escritos)`.
    pub fn finish(self) -> (String, u64) {
        (hex::encode(self.hasher.finalize()), self.bytes)
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        // Só o que foi realmente aceito: um `write` curto que hasheasse o
        // buffer inteiro daria um digest de bytes que nunca foram gravados.
        self.hasher.update(&buf[..n]);
        self.bytes += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// O sha256 de um arquivo que já está em disco.
///
/// Caminho de conferência, não de produção: quem acabou de gravar a parte já
/// tem o digest do [`HashingWriter`]. Usado para validar o que chegou pela rede
/// e o que o operador largou na pasta na mão.
pub fn hash_file(path: &Path) -> Result<(String, u64)> {
    let mut f = std::fs::File::open(path).with_context(|| format!("abrir {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    let mut total = 0u64;
    loop {
        let n = std::io::Read::read(&mut f, &mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        total += n as u64;
    }
    Ok((hex::encode(h.finalize()), total))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(seed: &str) -> String {
        seed.repeat(64 / seed.len())
    }

    fn layer_part(seed: &str, start: u32, end: u32) -> PomPart {
        PomPart {
            sha256: hash(seed),
            bytes: 1000,
            start_layer: start,
            end_layer: end,
            is_first: start == 0,
            is_last: false,
            tp: None,
        }
    }

    fn tp_part(seed: &str, rank: u32, size: u32) -> PomPart {
        PomPart {
            sha256: hash(seed),
            bytes: 1000,
            start_layer: 0,
            end_layer: 31,
            is_first: true,
            is_last: true,
            tp: Some(PomTp { rank, size }),
        }
    }

    fn source_manifest(
        source_seed: &str,
        parts: Vec<PomPart>,
        tp_parts: Vec<PomPart>,
    ) -> PomManifest {
        let kind = match (parts.is_empty(), tp_parts.is_empty()) {
            (false, true) => PomKind::Layers,
            (true, false) => PomKind::TensorParallel,
            (false, false) => PomKind::Combined,
            (true, true) => PomKind::Layers,
        };
        let mut manifest = PomManifest::new_for_source(
            PomModel {
                name: "qwen3-8b".into(),
                label: "qwen3:8b".into(),
                total_layers: 32,
                quant: Some("Q4_K_M".into()),
            },
            kind,
            "no-1",
            hash(source_seed),
            parts,
        );
        manifest.tp_parts = tp_parts;
        manifest
    }

    fn parte(sha: &str, inicio: u32, fim: u32) -> PomPart {
        PomPart {
            sha256: sha.repeat(64 / sha.len()),
            bytes: 1000,
            start_layer: inicio,
            end_layer: fim,
            is_first: inicio == 0,
            is_last: false,
            tp: None,
        }
    }

    fn manifesto(partes: Vec<PomPart>, total: u32) -> PomManifest {
        PomManifest::new(
            PomModel {
                name: "qwen3-8b".into(),
                label: "qwen3:8b".into(),
                total_layers: total,
                quant: Some("Q4_K_M".into()),
            },
            PomKind::Layers,
            "no-1",
            partes,
        )
    }

    /// Cada nó fatia a própria parte e as respostas voltam fora de ordem. Se a
    /// ordem de chegada mudasse o `pom_id`, duas máquinas discordariam sobre o
    /// nome da mesma rodada.
    #[test]
    fn o_pom_id_nao_depende_da_ordem_das_partes() {
        let a = parte("a", 0, 15);
        let b = parte("b", 16, 31);
        assert_eq!(pom_id(&[a.clone(), b.clone()]), pom_id(&[b, a]));
    }

    /// E precisa depender do conteúdo, senão não identifica rodada nenhuma.
    #[test]
    fn cortes_diferentes_dao_pom_ids_diferentes() {
        let um = vec![parte("a", 0, 15), parte("b", 16, 31)];
        let outro = vec![parte("a", 0, 19), parte("b", 20, 31)];
        assert_ne!(pom_id(&um), pom_id(&outro));
    }

    #[test]
    fn o_manifesto_sobrevive_a_ida_e_volta_em_json() {
        let m = manifesto(vec![parte("a", 0, 15), parte("b", 16, 31)], 32);
        let json = serde_json::to_string(&m).unwrap();
        let volta: PomManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(m, volta);
        volta.validate().unwrap();
    }

    /// O `pom_id` é derivado: se ele não bate com as partes, o arquivo foi
    /// editado à mão ou truncado, e carregá-lo seria confiar em lixo.
    #[test]
    fn um_pom_id_adulterado_e_recusado() {
        let mut m = manifesto(vec![parte("a", 0, 31)], 32);
        m.pom_id = "0".repeat(64);
        assert!(m.validate().is_err());
    }

    #[test]
    fn faixa_invertida_e_hash_torto_sao_recusados() {
        let mut m = manifesto(vec![parte("a", 0, 31)], 32);
        m.parts[0].start_layer = 40; // depois do `end_layer`, que é 31
        m.pom_id = pom_id(&m.parts);
        assert!(m.validate().is_err());

        let mut m = manifesto(vec![parte("a", 0, 31)], 32);
        m.parts[0].sha256 = "nao-e-hash".into();
        m.pom_id = pom_id(&m.parts);
        assert!(m.validate().is_err());
    }

    #[test]
    fn completude_e_buraco_na_cobertura() {
        assert!(manifesto(vec![parte("a", 0, 15), parte("b", 16, 31)], 32).complete());
        // Buraco em 16-19.
        assert!(!manifesto(vec![parte("a", 0, 15), parte("b", 20, 31)], 32).complete());
        // Cobre até 31 mas o modelo tem 36 camadas.
        assert!(!manifesto(vec![parte("a", 0, 15), parte("b", 16, 31)], 36).complete());
    }

    /// Um `sha256` chega pela rede e vira nome de arquivo: se `..` passasse,
    /// um peer escolheria onde gravar no disco de quem baixa.
    #[test]
    fn hash_que_nao_e_hash_nao_vira_caminho() {
        let dir = Path::new("/tmp/modelos/qwen");
        assert!(part_path(dir, "../../etc/passwd").is_err());
        assert!(part_path(dir, "ABCD").is_err());
        assert!(part_path(dir, &"a".repeat(63)).is_err());
        let ok = part_path(dir, &"a".repeat(64)).unwrap();
        assert_eq!(ok.parent().unwrap(), dir);
    }

    #[test]
    fn o_writer_hasheia_o_que_passa_por_ele() {
        let mut w = HashingWriter::new(Vec::new());
        w.write_all(b"abc").unwrap();
        let (sha, bytes) = w.finish();
        assert_eq!(bytes, 3);
        // sha256("abc"), o vetor de teste conhecido.
        assert_eq!(
            sha,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn gravar_e_reler_um_manifesto() {
        let tmp = std::env::temp_dir().join(format!("pom-teste-{}", uuid::Uuid::new_v4().simple()));
        let _ = std::fs::remove_dir_all(&tmp);
        let m = manifesto(vec![parte("a", 0, 15), parte("b", 16, 31)], 32);
        let dir = dir_for(&tmp, &m);

        let escrito = write_atomic(&dir, &m).unwrap();
        assert_eq!(load(&escrito).unwrap(), m);
        assert_eq!(load_dir(&dir), vec![m.clone()]);
        // O `:` do rótulo não pode virar pasta, e o id da rodada entra no nome.
        let nome = dir.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(nome, format!("qwen3-8b-{}", short_id(&m.pom_id)));

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// O defeito que o id no nome da pasta fecha: fatiar o **mesmo modelo**
    /// duas vezes, com cortes diferentes, punha os dois conjuntos de partes na
    /// mesma pasta. Eles não têm nada a ver um com o outro, e apagar um levava
    /// junto as partes que o outro por acaso também usava.
    #[test]
    fn duas_fatiagens_do_mesmo_modelo_nao_dividem_pasta() {
        let raiz = Path::new("/tmp/fatias");
        let um = manifesto(vec![parte("a", 0, 15), parte("b", 16, 31)], 32);
        let outro = manifesto(vec![parte("c", 0, 19), parte("d", 20, 31)], 32);

        assert_ne!(um.pom_id, outro.pom_id);
        assert_ne!(dir_for(raiz, &um), dir_for(raiz, &outro));
        // E as duas continuam sendo o mesmo modelo, com o mesmo rótulo.
        assert_eq!(um.model.label, outro.model.label);
    }

    /// Refatiar exatamente do mesmo jeito **não** duplica nada: mesmo conteúdo,
    /// mesmo `pom_id`, mesma pasta. É a mesma rodada.
    #[test]
    fn refatiar_igual_cai_na_mesma_pasta() {
        let raiz = Path::new("/tmp/fatias");
        let um = manifesto(vec![parte("a", 0, 15), parte("b", 16, 31)], 32);
        let de_novo = manifesto(vec![parte("b", 16, 31), parte("a", 0, 15)], 32);
        assert_eq!(dir_for(raiz, &um), dir_for(raiz, &de_novo));
    }

    /// A rodada só se muda para o nome definitivo quando está inteira, e as
    /// partes vão junto.
    #[test]
    fn finalizar_muda_a_rodada_para_a_pasta_com_o_id() {
        let raiz = std::env::temp_dir().join(format!("pom-fin-{}", uuid::Uuid::new_v4().simple()));
        let trabalho = scratch_dir(&raiz);
        std::fs::create_dir_all(&trabalho).unwrap();

        let m = manifesto(vec![parte("a", 0, 15), parte("b", 16, 31)], 32);
        for p in &m.parts {
            std::fs::write(part_path(&trabalho, &p.sha256).unwrap(), b"conteudo").unwrap();
        }

        let destino = finalize_run(&raiz, &trabalho, &m).unwrap();
        assert_eq!(destino, dir_for(&raiz, &m));
        for p in &m.parts {
            assert!(part_path(&destino, &p.sha256).unwrap().is_file());
        }
        assert_eq!(load_dir(&destino), vec![m]);
        assert!(!trabalho.exists(), "a pasta de trabalho não fica para trás");

        std::fs::remove_dir_all(&raiz).ok();
    }

    #[test]
    fn v2_pom_id_e_o_hash_do_modelo_original() {
        let model_hash = hash("a");
        let manifest = PomManifest::new_for_source(
            PomModel {
                name: "qwen3-8b".into(),
                label: "qwen3:8b".into(),
                total_layers: 32,
                quant: Some("Q4_K_M".into()),
            },
            PomKind::Layers,
            "node-1",
            model_hash.clone(),
            vec![layer_part("b", 0, 7)],
        );

        assert_eq!(manifest.pom_version, 2);
        assert_eq!(manifest.pom_id, model_hash);
        manifest.validate().unwrap();
    }

    #[test]
    fn merge_preserva_alternativas_locais_e_adiciona_tp_parts() {
        let mut local = source_manifest("a", vec![layer_part("b", 0, 7)], vec![]);
        let remote = source_manifest("a", vec![layer_part("c", 0, 3)], vec![tp_part("d", 0, 2)]);

        local.merge_from(remote).unwrap();

        assert_eq!(local.parts.len(), 2);
        assert_eq!(local.tp_parts.len(), 1);
        assert!(local
            .parts
            .iter()
            .any(|p| p.start_layer == 0 && p.end_layer == 7));
        assert!(local
            .parts
            .iter()
            .any(|p| p.start_layer == 0 && p.end_layer == 3));
        assert_eq!(local.kind, PomKind::Combined);
    }

    #[test]
    fn merge_repetido_nao_duplica_sha() {
        let part = layer_part("b", 0, 7);
        let mut local = source_manifest("a", vec![part.clone()], vec![]);
        local
            .merge_from(source_manifest("a", vec![part], vec![]))
            .unwrap();
        assert_eq!(local.parts.len(), 1);
    }

    #[test]
    fn merge_manifest_reusa_a_pasta_quando_o_nome_legivel_muda() {
        let raiz =
            std::env::temp_dir().join(format!("pom-source-{}", uuid::Uuid::new_v4().simple()));
        let local = source_manifest("a", vec![layer_part("b", 0, 7)], vec![]);
        let local_dir = source_run_dir(&raiz, "nome-antigo", &local.pom_id);
        std::fs::create_dir_all(&local_dir).unwrap();
        write_atomic(&local_dir, &local).unwrap();

        let incoming = source_manifest("a", vec![layer_part("c", 0, 3)], vec![]);
        let (merged_dir, merged) = merge_manifest(&raiz, &incoming).unwrap();

        assert_eq!(merged_dir, local_dir);
        assert_eq!(merged.parts.len(), 2);
        assert_eq!(load_dir(&local_dir).len(), 1);
        std::fs::remove_dir_all(&raiz).unwrap();
    }

    #[test]
    fn merge_manifest_preserva_o_conteudo_local() {
        let raiz =
            std::env::temp_dir().join(format!("pom-merge-{}", uuid::Uuid::new_v4().simple()));
        let local = source_manifest("a", vec![layer_part("b", 0, 7)], vec![]);
        let local_dir = source_run_dir(&raiz, "qwen3-8b", &local.pom_id);
        std::fs::create_dir_all(&local_dir).unwrap();
        write_atomic(&local_dir, &local).unwrap();

        let incoming = source_manifest("a", vec![layer_part("c", 0, 3)], vec![]);
        let (_, merged) = merge_manifest(&raiz, &incoming).unwrap();

        assert!(merged.parts.iter().any(|p| p.sha256 == hash("b")));
        assert!(merged.parts.iter().any(|p| p.sha256 == hash("c")));
        std::fs::remove_dir_all(&raiz).unwrap();
    }
}
