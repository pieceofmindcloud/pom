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
//! O `pom_id` é o sha256 da lista canônica de partes — o equivalente ao
//! infohash. Ele nomeia a *rodada* de fatiamento: refatiar o mesmo modelo com
//! outros cortes produz um `.pom` novo ao lado do antigo, em vez de sobrescrever
//! calado o que já estava lá.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Versão do formato. Sobe quando o significado de um campo muda.
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

/// O manifesto de uma rodada de fatiamento.
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
}

/// O identificador da rodada: sha256 sobre as partes, em ordem canônica.
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
            pom_version: POM_VERSION,
            pom_id: pom_id(&parts),
            model,
            kind,
            created_at: chrono::Utc::now().to_rfc3339(),
            created_by: created_by.into(),
            parts,
        }
    }

    /// As partes cobrem `0..total_layers` sem buraco nem sobreposição?
    ///
    /// Um manifesto incompleto é legítimo enquanto a rodada não terminou; quem
    /// implanta é que não pode subir um pipeline furado.
    pub fn complete(&self) -> bool {
        if self.kind == PomKind::TensorParallel {
            let Some(size) = self.parts.first().and_then(|p| p.tp).map(|t| t.size) else {
                return false;
            };
            let mut ranks: Vec<u32> = self
                .parts
                .iter()
                .filter_map(|p| p.tp)
                .map(|t| t.rank)
                .collect();
            ranks.sort_unstable();
            ranks.dedup();
            return ranks.len() as u32 == size;
        }
        let mut faixas: Vec<(u32, u32)> = self
            .parts
            .iter()
            .map(|p| (p.start_layer, p.end_layer))
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
        if self.parts.is_empty() {
            bail!("manifesto sem partes");
        }
        for p in &self.parts {
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
        // O `pom_id` é derivado, então um arquivo em que ele não bate foi
        // editado à mão ou truncado no meio de uma gravação.
        let esperado = pom_id(&self.parts);
        if self.pom_id != esperado {
            bail!(
                "pom_id não confere: o arquivo diz {}, as partes dão {}",
                &self.pom_id[..12.min(self.pom_id.len())],
                &esperado[..12]
            );
        }
        Ok(())
    }
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

/// A pasta de **uma rodada de fatiamento**: `<modelo>-<id da rodada>`.
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

/// A pasta da rodada que este manifesto descreve.
pub fn dir_for(shard_dir: &Path, m: &PomManifest) -> PathBuf {
    run_dir(shard_dir, &m.model.name, &m.pom_id)
}

/// O arquivo do manifesto dentro da pasta da rodada.
pub fn manifest_path(model_dir: &Path, m: &PomManifest) -> PathBuf {
    model_dir.join(format!(
        "{}-{}.{POM_EXT}",
        sanitize_name(&m.model.name),
        short_id(&m.pom_id)
    ))
}

/// Move as partes de uma pasta de trabalho para a pasta definitiva da rodada e
/// sela o manifesto lá.
///
/// O nome da pasta sai do `pom_id`, que só se conhece **depois** de cortar
/// todas as partes - o hash de cada uma entra no cálculo. Daí o rodeio: corta-se
/// num diretório temporário e, no fim, a rodada inteira se muda para o nome
/// definitivo.
///
/// Se a pasta de destino já existe, é porque esta rodada já foi produzida antes
/// (mesmo `pom_id` = mesmo conteúdo). Nesse caso as partes que já estão lá
/// ficam onde estão e as repetidas são descartadas: são byte a byte as mesmas.
pub fn finalize_run(shard_dir: &Path, temp: &Path, m: &PomManifest) -> Result<PathBuf> {
    m.validate()?;
    let destino = dir_for(shard_dir, m);
    std::fs::create_dir_all(&destino).with_context(|| format!("criar {}", destino.display()))?;

    for parte in &m.parts {
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
}
