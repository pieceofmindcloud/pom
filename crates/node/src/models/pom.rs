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
//! O `pom_id` é o sha256 do GGUF original. Uma pasta por modelo preserva todas
//! as alternativas de fatiamento e também os shards de Tensor Parallelism no
//! campo separado `tp_parts`.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Versão do formato. O produto ainda não foi lançado, então este é o único
/// contrato aceito: identidade pelo hash do GGUF original.
pub const POM_VERSION: u32 = 1;

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
    /// sha256 do GGUF original que deu origem a todas as partes.
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

impl PomManifest {
    /// Monta o manifesto único do GGUF original.
    pub fn new(
        mut model: PomModel,
        kind: PomKind,
        created_by: impl Into<String>,
        source_sha256: impl Into<String>,
        parts: Vec<PomPart>,
    ) -> Self {
        model.name = canonical_model_name(&model.name);
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
            bail!("merge requires the manifest of version {POM_VERSION}");
        }
        if self.pom_id != incoming.pom_id {
            bail!(
                "manifests belong to different models: {} and {}",
                &self.pom_id[..12.min(self.pom_id.len())],
                &incoming.pom_id[..12.min(incoming.pom_id.len())]
            );
        }
        self.validate()?;
        incoming.validate()?;
        if self.model.total_layers != incoming.model.total_layers {
            bail!(
                "manifests of model {} declare different layer totals: {} and {}",
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
                "manifests of model {} declare different quantizations",
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
        let mut next = 0u32;
        loop {
            let Some(end) = self
                .parts
                .iter()
                .filter(|p| p.start_layer == next)
                .map(|p| p.end_layer)
                .max()
            else {
                return false;
            };
            next = end.saturating_add(1);
            if next >= self.model.total_layers {
                return next == self.model.total_layers;
            }
        }
    }

    /// Diz se um grupo TP de tamanho `size` tem todos os ranks.
    pub fn tp_complete(&self, size: u32) -> bool {
        tp_complete_from(&self.tp_parts, Some(size))
    }

    /// Confere o que dá para conferir sem ler os arquivos.
    pub fn validate(&self) -> Result<()> {
        if self.pom_version != POM_VERSION {
            bail!(
                "manifest version {} is not the active version {POM_VERSION}",
                self.pom_version
            );
        }
        if self.model.name.trim().is_empty() {
            bail!("manifest without a model name");
        }
        if self.parts.is_empty() && self.tp_parts.is_empty() {
            bail!("manifest without parts");
        }
        for p in self.parts.iter().chain(self.tp_parts.iter()) {
            if !is_sha256_hex(&p.sha256) {
                bail!("part with an invalid hash: {:?}", p.sha256);
            }
            if p.start_layer > p.end_layer {
                bail!(
                    "part {} has an inverted range ({}-{})",
                    &p.sha256[..12],
                    p.start_layer,
                    p.end_layer
                );
            }
        }
        if !is_sha256_hex(&self.pom_id) {
            bail!("invalid pom_id of the original GGUF: {:?}", self.pom_id);
        }
        if self.parts.iter().any(|p| p.tp.is_some()) {
            bail!("parts cannot contain Tensor Parallelism shards");
        }
        if self.tp_parts.iter().any(|p| p.tp.is_none()) {
            bail!("tp_parts requires the rank and size of the TP group");
        }
        let expected = kind_for(&self.parts, &self.tp_parts);
        if self.kind != expected {
            bail!(
                "manifest kind does not match the parts: {:?}, expected {:?}",
                self.kind,
                expected
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
                    "{field} has different metadata for part {}",
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
        bail!("invalid part hash: {sha256:?}");
    }
    Ok(model_dir.join(format!("{sha256}.gguf")))
}

/// O identificador curto do arquivo original, o que aparece no nome da pasta.
pub fn short_id(pom_id: &str) -> &str {
    &pom_id[..8.min(pom_id.len())]
}

/// Nome canônico usado na pasta e no manifesto, sem o sufixo do arquivo GGUF.
pub fn canonical_model_name(name: &str) -> String {
    let stem = name
        .strip_suffix(".gguf")
        .or_else(|| name.strip_suffix(".GGUF"))
        .unwrap_or(name);
    sanitize_name(stem)
}

/// A pasta identificada pelo início do hash completo do GGUF original.
pub fn source_run_dir(shard_dir: &Path, model_name: &str, source_sha256: &str) -> PathBuf {
    shard_dir.join(format!(
        "{}-{}",
        canonical_model_name(model_name),
        short_id(source_sha256)
    ))
}

/// Procura a pasta pelo hash completo, sem confiar no nome legível.
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

/// A pasta que este manifesto descreve.
pub fn dir_for(shard_dir: &Path, m: &PomManifest) -> PathBuf {
    source_run_dir(shard_dir, &m.model.name, &m.pom_id)
}

/// O arquivo único do manifesto dentro da pasta do modelo.
pub fn manifest_path(model_dir: &Path, m: &PomManifest) -> PathBuf {
    let folder = model_dir
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            format!(
                "{}-{}",
                canonical_model_name(&m.model.name),
                short_id(&m.pom_id)
            )
        });
    model_dir.join(format!("{folder}.{POM_EXT}"))
}

struct PomLock(PathBuf);

impl Drop for PomLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn acquire_pom_lock(shard_dir: &Path, source_sha256: &str) -> Result<PomLock> {
    std::fs::create_dir_all(shard_dir)
        .with_context(|| format!("create the slices directory {}", shard_dir.display()))?;
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
            Err(e) => return Err(e).with_context(|| format!("create lock {}", path.display())),
        }
    }
    bail!(
        "timed out waiting for the manifest lock {}",
        source_sha256
    )
}

/// Mescla o manifesto no inventário local do mesmo modelo original.
pub fn merge_manifest(shard_dir: &Path, incoming: &PomManifest) -> Result<(PathBuf, PomManifest)> {
    if incoming.pom_version != POM_VERSION {
        bail!("merge requires the manifest of version {POM_VERSION}");
    }
    incoming.validate()?;
    let _lock = acquire_pom_lock(shard_dir, &incoming.pom_id)?;
    let destination = locate_source_dir(shard_dir, &incoming.pom_id)
        .unwrap_or_else(|| source_run_dir(shard_dir, &incoming.model.name, &incoming.pom_id));
    std::fs::create_dir_all(&destination)
        .with_context(|| format!("create the model directory {}", destination.display()))?;

    let existing = load_dir(&destination)
        .into_iter()
        .find(|m| m.pom_version == POM_VERSION && m.pom_id == incoming.pom_id);
    let mut aggregate = existing.unwrap_or_else(|| incoming.clone());
    if aggregate.parts != incoming.parts || aggregate.tp_parts != incoming.tp_parts {
        aggregate.merge_from(incoming.clone())?;
    }
    write_atomic(&destination, &aggregate)?;
    Ok((destination, aggregate))
}

/// Move as partes de uma pasta de trabalho para a pasta do modelo e mescla o
/// manifesto. A identidade vem do hash do GGUF original, então uma nova forma
/// de particionar o mesmo modelo adiciona partes à mesma pasta.
/// Partes repetidas são descartadas por hash.
pub fn finalize_run(shard_dir: &Path, temp: &Path, m: &PomManifest) -> Result<PathBuf> {
    m.validate()?;
    let destination = locate_source_dir(shard_dir, &m.pom_id)
        .unwrap_or_else(|| source_run_dir(shard_dir, &m.model.name, &m.pom_id));
    std::fs::create_dir_all(&destination).with_context(|| format!("create {}", destination.display()))?;

    for part in m.parts.iter().chain(m.tp_parts.iter()) {
        let from = part_path(temp, &part.sha256)?;
        let to = part_path(&destination, &part.sha256)?;
        if !from.is_file() {
            // A parte foi reaproveitada de uma rodada anterior e nunca passou
            // pela pasta de trabalho: nada a mover.
            continue;
        }
        if to.is_file() {
            let _ = std::fs::remove_file(&from);
            continue;
        }
        std::fs::rename(&from, &to)
            .with_context(|| format!("move {} to {}", from.display(), to.display()))?;
    }

    let (destination, _) = merge_manifest(shard_dir, m)?;
    // A pasta de trabalho só some se estiver vazia: um arquivo inesperado ali
    // é sinal de defeito, e apagá-lo escondería a evidência.
    let _ = std::fs::remove_dir(temp);
    Ok(destination)
}

/// Uma pasta de trabalho para uma rodada que ainda não tem nome.
pub fn scratch_dir(shard_dir: &Path) -> PathBuf {
    shard_dir.join(format!(".slicing-{}", uuid::Uuid::new_v4().simple()))
}

/// Nome que pode virar pasta sem surpresa.
pub fn sanitize_name(name: &str) -> String {
    let clean: String = name
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
    let clean = clean.trim_matches(['-', '.', '_']).to_string();
    if clean.is_empty() {
        "model".to_string()
    } else {
        clean
    }
}

/// Grava o manifesto sem deixar arquivo pela metade se a máquina cair no meio.
///
/// Mesmo padrão do merge em `webui.rs`: escreve num temporário ao lado e só
/// então renomeia, porque `rename` no mesmo sistema de arquivos é atômico e um
/// `.pom` truncado seria lido como "modelo sem partes".
pub fn write_atomic(model_dir: &Path, m: &PomManifest) -> Result<PathBuf> {
    m.validate()?;
    std::fs::create_dir_all(model_dir).with_context(|| format!("create {}", model_dir.display()))?;

    let destination = manifest_path(model_dir, m);
    let temp = model_dir.join(format!(".{}.{}.tmp", m.pom_id, std::process::id()));

    let json = serde_json::to_vec_pretty(m).context("serialize manifest")?;
    {
        let mut f =
            std::fs::File::create(&temp).with_context(|| format!("create {}", temp.display()))?;
        f.write_all(&json)?;
        // Sem isto o rename pode chegar ao disco antes do conteúdo.
        f.sync_all()?;
    }
    if let Err(e) = std::fs::rename(&temp, &destination) {
        let _ = std::fs::remove_file(&temp);
        return Err(e).with_context(|| format!("rename to {}", destination.display()));
    }
    Ok(destination)
}

/// Lê e valida um manifesto.
pub fn load(path: &Path) -> Result<PomManifest> {
    let raw = std::fs::read(path).with_context(|| format!("ler {}", path.display()))?;
    let m: PomManifest =
        serde_json::from_slice(&raw).with_context(|| format!("parse {}", path.display()))?;
    m.validate()
        .with_context(|| format!("invalid manifest in {}", path.display()))?;
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
            Err(e) => tracing::warn!("ignoring manifest {}: {e:#}", p.display()),
        }
    }
    out
}

/// Localiza o manifesto v1 que conhece a parte armazenada neste caminho.
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
    let mut f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
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
        let mut manifest = PomManifest::new(
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

    fn part(sha: &str, start: u32, end: u32) -> PomPart {
        PomPart {
            sha256: sha.repeat(64 / sha.len()),
            bytes: 1000,
            start_layer: start,
            end_layer: end,
            is_first: start == 0,
            is_last: false,
            tp: None,
        }
    }

    fn manifesto(parts: Vec<PomPart>, total: u32) -> PomManifest {
        PomManifest::new(
            PomModel {
                name: "qwen3-8b".into(),
                label: "qwen3:8b".into(),
                total_layers: total,
                quant: Some("Q4_K_M".into()),
            },
            PomKind::Layers,
            "no-1",
            hash("a"),
            parts,
        )
    }

    #[test]
    fn the_pom_id_is_the_hash_of_the_original_file() {
        let source_sha256 = hash("a");
        let manifest = source_manifest("a", vec![part("b", 0, 31)], vec![]);
        assert_eq!(manifest.pom_version, POM_VERSION);
        assert_eq!(manifest.pom_id, source_sha256);
        manifest.validate().unwrap();
    }

    #[test]
    fn the_manifest_survives_a_json_round_trip() {
        let m = manifesto(vec![part("a", 0, 15), part("b", 16, 31)], 32);
        let json = serde_json::to_string(&m).unwrap();
        let restored: PomManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(m, restored);
        restored.validate().unwrap();
    }

    #[test]
    fn a_pom_id_that_is_not_a_hash_is_refused() {
        let mut m = manifesto(vec![part("a", 0, 31)], 32);
        m.pom_id = "nao-e-hash".into();
        assert!(m.validate().is_err());
    }

    #[test]
    fn an_inverted_range_and_a_malformed_hash_are_refused() {
        let mut m = manifesto(vec![part("a", 0, 31)], 32);
        m.parts[0].start_layer = 40; // depois do `end_layer`, que é 31
        assert!(m.validate().is_err());

        let mut m = manifesto(vec![part("a", 0, 31)], 32);
        m.parts[0].sha256 = "nao-e-hash".into();
        assert!(m.validate().is_err());
    }

    #[test]
    fn completeness_and_a_hole_in_the_coverage() {
        assert!(manifesto(vec![part("a", 0, 15), part("b", 16, 31)], 32).complete());
        // Buraco em 16-19.
        assert!(!manifesto(vec![part("a", 0, 15), part("b", 20, 31)], 32).complete());
        // Cobre até 31 mas o modelo tem 36 camadas.
        assert!(!manifesto(vec![part("a", 0, 15), part("b", 16, 31)], 36).complete());
    }

    /// Um `sha256` chega pela rede e vira nome de arquivo: se `..` passasse,
    /// um peer escolheria onde gravar no disco de quem baixa.
    #[test]
    fn a_hash_that_is_not_a_hash_does_not_become_a_path() {
        let dir = Path::new("/tmp/models/qwen");
        assert!(part_path(dir, "../../etc/passwd").is_err());
        assert!(part_path(dir, "ABCD").is_err());
        assert!(part_path(dir, &"a".repeat(63)).is_err());
        let ok = part_path(dir, &"a".repeat(64)).unwrap();
        assert_eq!(ok.parent().unwrap(), dir);
    }

    #[test]
    fn the_writer_hashes_what_goes_through_it() {
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
    fn write_and_reread_a_manifest() {
        let tmp = std::env::temp_dir().join(format!("pom-teste-{}", uuid::Uuid::new_v4().simple()));
        let _ = std::fs::remove_dir_all(&tmp);
        let m = manifesto(vec![part("a", 0, 15), part("b", 16, 31)], 32);
        let dir = dir_for(&tmp, &m);

        let written = write_atomic(&dir, &m).unwrap();
        assert_eq!(load(&written).unwrap(), m);
        assert_eq!(load_dir(&dir), vec![m.clone()]);
        assert_eq!(
            written.file_name().unwrap().to_string_lossy(),
            "qwen3-8b-aaaaaaaa.pom"
        );
        // O `:` do rótulo não pode virar pasta, e o hash da fonte entra no nome.
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(name, format!("qwen3-8b-{}", short_id(&m.pom_id)));

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// Fatiamentos diferentes do **mesmo arquivo original** compartilham a
    /// pasta e o manifesto, porque o hash da fonte é a identidade do modelo.
    #[test]
    fn two_slicings_of_the_same_file_share_a_folder() {
        let root = Path::new("/tmp/slices");
        let um = manifesto(vec![part("a", 0, 15), part("b", 16, 31)], 32);
        let other = manifesto(vec![part("c", 0, 19), part("d", 20, 31)], 32);

        assert_eq!(um.pom_id, other.pom_id);
        assert_eq!(dir_for(root, &um), dir_for(root, &other));
    }

    /// Refatiar exatamente do mesmo jeito **não** duplica nada: mesmo conteúdo,
    /// mesmo `pom_id`, mesma pasta. É a mesma rodada.
    #[test]
    fn reslicing_the_same_way_lands_in_the_same_folder() {
        let root = Path::new("/tmp/slices");
        let um = manifesto(vec![part("a", 0, 15), part("b", 16, 31)], 32);
        let again = manifesto(vec![part("b", 16, 31), part("a", 0, 15)], 32);
        assert_eq!(dir_for(root, &um), dir_for(root, &again));
    }

    /// A rodada só se muda para o nome definitivo quando está inteira, e as
    /// partes vão junto.
    #[test]
    fn finalizing_moves_the_round_to_the_folder_with_the_id() {
        let root = std::env::temp_dir().join(format!("pom-fin-{}", uuid::Uuid::new_v4().simple()));
        let work = scratch_dir(&root);
        std::fs::create_dir_all(&work).unwrap();

        let m = manifesto(vec![part("a", 0, 15), part("b", 16, 31)], 32);
        for p in &m.parts {
            std::fs::write(part_path(&work, &p.sha256).unwrap(), b"conteudo").unwrap();
        }

        let destination = finalize_run(&root, &work, &m).unwrap();
        assert_eq!(destination, dir_for(&root, &m));
        for p in &m.parts {
            assert!(part_path(&destination, &p.sha256).unwrap().is_file());
        }
        assert_eq!(load_dir(&destination), vec![m]);
        assert!(!work.exists(), "the work folder is not left behind");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_model_name_drops_the_gguf_suffix() {
        let model_hash = hash("a");
        let manifest = PomManifest::new(
            PomModel {
                name: "Llama-3.1-8B-Instruct-Q4_K_M.gguf".into(),
                label: "qwen3:8b".into(),
                total_layers: 32,
                quant: Some("Q4_K_M".into()),
            },
            PomKind::Layers,
            "node-1",
            model_hash.clone(),
            vec![layer_part("b", 0, 7)],
        );

        assert_eq!(manifest.pom_version, POM_VERSION);
        assert_eq!(manifest.pom_id, model_hash);
        assert_eq!(manifest.model.name, "Llama-3.1-8B-Instruct-Q4_K_M");
        manifest.validate().unwrap();
    }

    #[test]
    fn merge_preserves_local_alternatives_and_adds_tp_parts() {
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
    fn a_repeated_merge_does_not_duplicate_the_sha() {
        let part = layer_part("b", 0, 7);
        let mut local = source_manifest("a", vec![part.clone()], vec![]);
        local
            .merge_from(source_manifest("a", vec![part], vec![]))
            .unwrap();
        assert_eq!(local.parts.len(), 1);
    }

    #[test]
    fn merge_manifest_reuses_the_folder_when_the_readable_name_changes() {
        let root =
            std::env::temp_dir().join(format!("pom-source-{}", uuid::Uuid::new_v4().simple()));
        let local = source_manifest("a", vec![layer_part("b", 0, 7)], vec![]);
        let local_dir = source_run_dir(&root, "nome-antigo", &local.pom_id);
        std::fs::create_dir_all(&local_dir).unwrap();
        write_atomic(&local_dir, &local).unwrap();

        let incoming = source_manifest("a", vec![layer_part("c", 0, 3)], vec![]);
        let (merged_dir, merged) = merge_manifest(&root, &incoming).unwrap();

        assert_eq!(merged_dir, local_dir);
        assert_eq!(merged.parts.len(), 2);
        assert_eq!(load_dir(&local_dir).len(), 1);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn merge_manifest_preserves_the_local_content() {
        let root =
            std::env::temp_dir().join(format!("pom-merge-{}", uuid::Uuid::new_v4().simple()));
        let local = source_manifest("a", vec![layer_part("b", 0, 7)], vec![]);
        let local_dir = source_run_dir(&root, "qwen3-8b", &local.pom_id);
        std::fs::create_dir_all(&local_dir).unwrap();
        write_atomic(&local_dir, &local).unwrap();

        let incoming = source_manifest("a", vec![layer_part("c", 0, 3)], vec![]);
        let (_, merged) = merge_manifest(&root, &incoming).unwrap();

        assert!(merged.parts.iter().any(|p| p.sha256 == hash("b")));
        assert!(merged.parts.iter().any(|p| p.sha256 == hash("c")));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
