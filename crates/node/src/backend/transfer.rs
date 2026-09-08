//! Moving model slices between machines.
//!
//! Two directions, both ending in the same download:
//!
//! * **pull** — this node asks a peer for a slice it does not have;
//! * **push** — a node *offers* a slice to a peer, the peer's owner accepts,
//!   and the peer then pulls it. The sender never writes to the receiver's
//!   disk on its own; acceptance is what starts the transfer. A development
//!   machine can waive that click with `POM_DEV_AUTO_ACCEPT_TRANSFERS`,
//!   which only the *receiver* can set and which is off everywhere else —
//!   see [`dev_auto_accept_env`].
//!
//! The bytes travel over the peers' existing HTTP gateway rather than the
//! libp2p bitswap behaviour: that is the path already proven to carry traffic
//! between these machines (the inference pipeline uses it), and it gives byte
//! ranges — so an interrupted multi-gigabyte transfer resumes instead of
//! starting over.
//!
//! What may leave a machine depends on who chose the file. A **pull** names a
//! path on a machine where nobody chose anything, so it is confined to the
//! models directory. A **push** is this machine's own operator choosing one
//! filename from the catalog; the backend resolves that basename against the
//! configured roots and the peer receives an opaque grant id. See
//! [`TransferState::grants`].
//!
//! Onde o arquivo cai depende do que ele é:
//!
//! * uma **parte** de um fatiamento vai para
//!   `<pasta de fatias>/<modelo>-<hash curto do original>/`, com o nome sendo o
//!   próprio sha256 e o manifesto nomeado como a pasta, gravado na mesma pasta
//!   antes dos bytes. O id vem do
//!   hash do modelo original, e novas alternativas são mescladas no manifesto;
//!   É o hash pedido que confere o que chegou - não um digest que o remetente
//!   declarou;
//! * qualquer outro arquivo - um modelo inteiro, que não pertence a manifesto
//!   nenhum - cai em `<pasta de fatias>/recebidos/`, e só toma o nome final
//!   quando o MD5 confere.
//!
//! **O `.pom` é obrigatório para uma parte, nos dois sentidos.** Quem manda
//! recusa oferecer uma fatia órfã; quem serve recusa entregá-la; quem recebe
//! recusa aceitá-la. Uma parte sem manifesto é um arquivo que ninguém sabe de
//! que modelo é nem que camadas traz, e que a tela mostraria como um modelo
//! inteiro - falhar dizendo o que falta é melhor que espalhar o problema.
//!
//! A pasta de fatias é a configurada em `pom.json`, `models/` até alguém mudar.

use crate::auth::ConnectionAuth;
use crate::gateway::GatewayState;
use axum::{
    body::Body,
    extract::{Path as AxumPath, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::RwLock;
use tracing::{info, warn};

/// Read/written in chunks — a slice is gigabytes, it never sits in memory.
const CHUNK: usize = 1024 * 1024;

/// Development-only: take every incoming offer without asking a human.
///
/// The whole point of an offer is that nobody writes gigabytes to somebody
/// else's disk without their consent, so this is **off unless the receiving
/// machine sets it**, and it is the receiver's own variable — a sender cannot
/// turn it on remotely. It exists because testing the push direction by hand
/// means driving two browsers to move one file. Never set it on a machine whose
/// disk you care about.
fn dev_auto_accept_env() -> String {
    crate::brand::env_var("DEV_AUTO_ACCEPT_TRANSFERS")
}

/// Whether this process was started with the auto-accept gate open.
///
/// Read once: a variable flipped after start would make two offers in the same
/// session behave differently, and "why did that one need a click?" is not a
/// question worth having.
fn dev_auto_accept() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        auto_accept_set(
            crate::brand::env("DEV_AUTO_ACCEPT_TRANSFERS")
                .ok()
                .as_deref(),
        )
    })
}

/// A gate this dangerous does not open by accident: unset, empty, `0` and
/// `false` all mean off, so `export …=` in a stale shell profile cannot leave a
/// machine silently taking whatever the network offers it.
fn auto_accept_set(raw: Option<&str>) -> bool {
    match raw.map(str::trim) {
        None | Some("") | Some("0") => false,
        Some(v) => !v.eq_ignore_ascii_case("false"),
    }
}

/// Headroom demanded on top of the file size before a download starts.
const DISK_HEADROOM_MB: u64 = 512;

/// Stable wire values consumed by the web UI. Keep these as protocol terms;
/// presentation is localized in the client.
const TRANSFER_RUNNING: &str = "running";
const TRANSFER_VERIFYING: &str = "verifying";
const TRANSFER_DONE: &str = "done";
const TRANSFER_FAILED: &str = "failed";
const TRANSFER_CANCELLED: &str = "cancelled";

/// Direções do mesmo job. O id é um só nas duas máquinas; o que muda é de que
/// lado dele cada uma está.
const TRANSFER_SENDING: &str = "sending";
const TRANSFER_RECEIVING: &str = "receiving";

/// Um estado do qual não se sai. A tela usa a mesma regra (`transferStatus.ts`).
fn is_finished(state: &str) -> bool {
    matches!(state, TRANSFER_DONE | TRANSFER_FAILED | TRANSFER_CANCELLED)
}

pub fn routes() -> Router<GatewayState> {
    Router::new()
        // Serving side
        .route("/api/transfer/manifest", get(manifest))
        .route("/api/transfer/pom", get(serve_pom))
        .route("/api/transfer/download", get(download))
        // Receiving side
        .route("/api/transfer/pull", post(start_pull))
        .route("/api/transfer/offer", post(receive_offer))
        .route("/api/transfer/offers", get(list_offers))
        .route("/api/transfer/offers/:id/accept", post(accept_offer))
        .route("/api/transfer/offers/:id/reject", post(reject_offer))
        // O veredito volta para quem ofereceu. Ver `receive_answer`.
        .route("/api/transfer/offers/answer", post(receive_answer))
        .route("/api/transfer/jobs", get(list_jobs))
        .route("/api/transfer/jobs/:id/cancel", post(cancel_job))
        // Push direction, driven from this node's screen
        .route("/api/transfer/send", post(send_to_peer))
}

// ── Shared state ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct TransferJob {
    pub id: String,
    pub name: String,
    /// `sending` ou `receiving`. O mesmo id existe nas duas máquinas, com
    /// direções opostas — é assim que os dois lados falam da mesma
    /// transferência sem inventar um segundo vocabulário.
    pub direction: &'static str,
    /// A outra máquina. Substitui `from_host`, que mentia na direção de envio.
    pub peer_host: String,
    /// Arquivo local envolvido. A exclusão precisa saber o que está em trânsito.
    pub path: String,
    pub state: String,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub bytes_per_sec: f64,
    pub eta_secs: Option<u64>,
    pub detail: String,
    /// De que modelo esta transferência é, e que camadas ela traz.
    ///
    /// **O nome de uma parte é o sha256 dela**, e hash não diz de que modelo é
    /// nem que pedaço dele traz. Quem sabe as duas coisas é o `.pom`, que já
    /// viaja com a oferta e é gravado antes do primeiro byte; sem estes campos
    /// a notificação só tinha o hash para mostrar. O `name` continua aqui como
    /// identificação secundária - é ele que nomeia o arquivo em disco.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_layer: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_layer: Option<u32>,
    /// Quando o job entrou num estado terminal, em RFC 3339.
    ///
    /// O sino lista transferência em andamento; sem esta marca ele não tem como
    /// mostrar a que **acabou de** terminar, e o desfecho - concluída, falhada,
    /// recusada - some da tela no instante em que acontece. Era esse o buraco:
    /// o operador só reencontrava o resultado na tela de Transferências.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferOffer {
    pub id: String,
    /// Node offering the file — the receiver pulls from here once accepted.
    pub from_node_id: String,
    pub from_host: String,
    pub from_gateway: String,
    /// Shown to the human deciding, so they can see *what* is being offered.
    pub remote_path: String,
    /// How we actually fetch it. Absent only for offers from an older peer,
    /// which can still be pulled by path if the file lives in `models/`.
    pub grant: Option<String>,
    pub name: String,
    pub size_bytes: u64,
    /// Vazio, e de propósito: **a oferta não carrega digest**.
    ///
    /// Quem confere o arquivo é o receptor, com o MD5 que ele mesmo pede em
    /// `/api/transfer/manifest` depois de aceitar — este campo nunca foi lido
    /// por ninguém. Calculá-lo aqui custava uma leitura do arquivo inteiro
    /// *antes* de a oferta sair, e era isso que segurava a notificação do
    /// outro lado por minutos. O campo continua na estrutura porque um peer de
    /// versão anterior o exige no corpo do pedido.
    pub md5: String,
    /// O manifesto do fatiamento a que este arquivo pertence, quando ele é uma
    /// parte.
    ///
    /// Viaja **junto com a oferta**, e não depois: sem ele o outro lado recebe
    /// um `<sha256>.gguf` que não sabe de que modelo é nem que camadas traz, e
    /// acaba guardando como se fosse um modelo inteiro. São alguns kB de JSON
    /// contra gigabytes de peso - mandar junto é de graça.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pom: Option<crate::pom::PomManifest>,
    pub start_layer: Option<u32>,
    pub end_layer: Option<u32>,
}

#[derive(Default)]
pub struct TransferState {
    pub jobs: RwLock<HashMap<String, TransferJob>>,
    pub offers: RwLock<Vec<TransferOffer>>,
    /// path → (mtime, size, md5), so a 3 GB digest is computed once.
    digests: RwLock<HashMap<String, (u64, u64, String)>>,
    /// Files this machine's own operator deliberately offered: id → path.
    ///
    /// A grant is what lets a file outside `models/` leave this machine. It is
    /// created only by [`send_to_peer`], which is driven from *this* node's
    /// screen — so the path is always someone's local choice, never a remote
    /// request. See [`grant_path`].
    grants: RwLock<HashMap<String, PathBuf>>,
    /// Ids cancelados. Guardados mesmo para job que ainda não existe aqui: o
    /// aviso do outro lado pode chegar antes de o download começar, e sem isso
    /// a transferência arrancaria já cancelada e ninguém a pararia.
    cancelled: RwLock<HashSet<String>>,
    /// O que esta máquina ofereceu e ainda não teve resposta: `grant` → oferta.
    ///
    /// Sem isto o remetente não tem como saber de que arquivo o outro lado
    /// está falando quando o veredito chega - o `grant` é o único id que as
    /// duas pontas compartilham antes de existir um job. Ver [`receive_answer`].
    offered: RwLock<HashMap<String, OfferedFile>>,
}

/// Como uma fatia se apresenta na tela: o modelo e a faixa de camadas.
///
/// Sai do `.pom`, e só de lá. O nome do arquivo é o sha256 e não afirma nada
/// sobre camada nenhuma - deduzir a faixa do nome foi um defeito real, e o
/// manifesto é quem sabe.
#[derive(Debug, Clone)]
pub struct SliceLabel {
    pub model_label: String,
    pub start_layer: u32,
    pub end_layer: u32,
}

/// O descritor da parte `sha256` dentro de um manifesto, se ele a descreve.
pub fn slice_label(m: &crate::pom::PomManifest, sha256: &str) -> Option<SliceLabel> {
    m.parts
        .iter()
        .chain(m.tp_parts.iter())
        .find(|p| p.sha256 == sha256)
        .map(|p| SliceLabel {
            model_label: m.model.label.clone(),
            start_layer: p.start_layer,
            end_layer: p.end_layer,
        })
}

/// O descritor de um arquivo em disco, quando ele é parte de um fatiamento.
///
/// O que identifica a parte é o **caminho**: o `.pom` mora ao lado dela, e o
/// nome com que alguém a está mandando não decide de que rodada ela é.
fn slice_label_for_path(path: &Path) -> Option<SliceLabel> {
    let sha = path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| crate::pom::is_sha256_hex(s))?;
    crate::pom::load_dir(path.parent()?)
        .iter()
        .find_map(|m| slice_label(m, sha))
}

/// Uma oferta que saiu daqui, guardada para dar nome ao veredito que voltar.
#[derive(Debug, Clone)]
pub struct OfferedFile {
    pub to_node_id: String,
    /// Nome legível do destino, para a notificação não falar em hash de nó.
    pub to_host: String,
    pub name: String,
    pub path: String,
    pub size_bytes: u64,
    /// Modelo e camadas, para o veredito não voltar falando em hash.
    pub fatia: Option<SliceLabel>,
}

impl TransferState {
    /// Registra uma transferência em curso. Chamado pelos dois lados, com o
    /// mesmo id.
    #[allow(clippy::too_many_arguments)]
    pub async fn start_job(
        &self,
        id: &str,
        name: &str,
        direction: &'static str,
        peer_host: &str,
        path: &str,
        bytes_total: u64,
        fatia: Option<&SliceLabel>,
    ) {
        self.jobs.write().await.insert(
            id.to_string(),
            TransferJob {
                id: id.to_string(),
                name: name.to_string(),
                direction,
                peer_host: peer_host.to_string(),
                path: path.to_string(),
                state: TRANSFER_RUNNING.into(),
                bytes_done: 0,
                bytes_total,
                bytes_per_sec: 0.0,
                eta_secs: None,
                detail: String::new(),
                model_label: fatia.map(|f| f.model_label.clone()),
                start_layer: fatia.map(|f| f.start_layer),
                end_layer: fatia.map(|f| f.end_layer),
                finished_at: None,
            },
        );
    }

    /// Fecha um job num estado terminal, carimbando a hora.
    ///
    /// Passa por aqui todo desfecho, e por uma razão só: um estado terminal
    /// nunca é reescrito por outro. "Cancelado por ksia" é decisão de alguém e
    /// não pode virar um "falhou" anônimo quando o loop percebe que o stream
    /// morreu - eram dois lugares diferentes fazendo essa checagem à mão, e o
    /// lado que envia não fazia nenhuma.
    pub async fn finish(&self, id: &str, state: &str, detail: impl Into<String>) {
        let mut jobs = self.jobs.write().await;
        let Some(j) = jobs.get_mut(id) else { return };
        if is_finished(&j.state) {
            return;
        }
        j.state = state.to_string();
        j.detail = detail.into();
        j.finished_at = Some(chrono::Utc::now().to_rfc3339());
    }

    /// Progresso, com a mesma aritmética nos dois lados.
    ///
    /// `moved` é o que andou nesta tentativa, que não é `done` quando a
    /// transferência retomou de um `.part`: usar `done` inflaria a velocidade
    /// com bytes que já estavam no disco.
    pub async fn progress(&self, id: &str, done: u64, moved: u64, elapsed: Duration) {
        let secs = elapsed.as_secs_f64().max(0.001);
        let rate = moved as f64 / secs;
        let mut jobs = self.jobs.write().await;
        if let Some(j) = jobs.get_mut(id) {
            j.bytes_done = done;
            j.bytes_per_sec = rate;
            j.eta_secs = if rate > 1.0 {
                Some(((j.bytes_total.saturating_sub(done)) as f64 / rate) as u64)
            } else {
                None
            };
        }
    }

    pub async fn is_cancelled(&self, id: &str) -> bool {
        self.cancelled.read().await.contains(id)
    }

    /// Marca o cancelamento e rotula o job com quem pediu.
    ///
    /// "Cancelado por ksia" e "falhou" são coisas diferentes para quem olha a
    /// tela: a primeira foi decisão de alguém, a segunda é motivo para tentar
    /// de novo.
    pub async fn cancel(&self, id: &str, by: &str) {
        self.cancelled.write().await.insert(id.to_string());
        self.finish(id, TRANSFER_CANCELLED, format!("cancelado por {by}"))
            .await;
    }
}

pub type Transfers = Arc<TransferState>;

// ── Path safety ──────────────────────────────────────────────────────────────

/// Assinatura de um arquivo GGUF: os 4 primeiros bytes.
const GGUF_MAGIC: &[u8; 4] = b"GGUF";

/// O único tipo de arquivo que pode sair desta máquina ou ser apagado por ela.
///
/// O conteúdo e a extensão precisam confirmar que é um GGUF.
pub(crate) fn ensure_gguf(path: &Path) -> Result<(), String> {
    use std::io::Read;

    if !path.is_file() {
        return Err(format!("não é um arquivo: {}", path.display()));
    }

    if !path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("gguf"))
    {
        return Err(format!(
            "só arquivos com extensão .gguf podem ser transferidos: {}",
            path.display()
        ));
    }

    let mut head = [0u8; 4];
    let mut file = std::fs::File::open(path)
        .map_err(|e| format!("não deu para abrir {}: {e}", path.display()))?;
    file.read_exact(&mut head)
        .map_err(|_| format!("{} é curto demais para ser um GGUF", path.display()))?;
    if &head != GGUF_MAGIC {
        return Err(format!(
            "{} não é um GGUF (assinatura {head:02x?})",
            path.display()
        ));
    }
    Ok(())
}

fn valid_transfer_filename(raw: &str) -> Result<&str, String> {
    let filename = raw.trim();
    let path = Path::new(filename);
    if filename.is_empty()
        || path.is_absolute()
        || path.components().count() != 1
        || path.file_name().and_then(|name| name.to_str()) != Some(filename)
    {
        return Err("a transferência aceita apenas o nome de um arquivo, sem caminho".to_string());
    }
    if !path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("gguf"))
    {
        return Err("só arquivos com extensão .gguf podem ser transferidos".to_string());
    }
    Ok(filename)
}

/// Resolve a requested path inside the models directory, or refuse.
///
/// The path arrives from another machine, so it is untrusted: without this a
/// peer could ask for any file the process can read.
fn safe_model_path(models_dir: &Path, requested: &str) -> Result<PathBuf, String> {
    let root = models_dir
        .canonicalize()
        .map_err(|e| format!("models dir ilegível: {e}"))?;
    let candidate = Path::new(requested);
    let full = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };
    let full = full
        .canonicalize()
        .map_err(|_| format!("arquivo não encontrado: {requested}"))?;
    if !full.starts_with(&root) {
        return Err("caminho fora do diretório de modelos".to_string());
    }
    Ok(full)
}

/// Resolve a file this node is willing to serve to a peer.
///
/// Two ways in, and the difference is who chose the file:
///
/// - `grant` — our own operator picked this exact file on this machine's screen,
///   so it may live anywhere the process can read. The peer never names a path;
///   it only replays an opaque id we handed it.
/// - `path` — nobody here chose anything (the pull direction, where a peer asks
///   us for a file on its own initiative), so `models/` is the whole world.
async fn servable_path(state: &GatewayState, q: &PathQuery) -> Result<PathBuf, String> {
    let path = if let Some(id) = q.grant.as_deref() {
        grant_path(state, id).await?
    } else if let Some(sha) = q.sha.as_deref() {
        let curto = &sha[..12.min(sha.len())];
        let encontrada = find_part(&state.shard_dir().await, sha)
            .ok_or_else(|| format!("esta máquina não tem a parte {curto}"))?;
        // Uma parte só é servível junto com o manifesto que a explica. Servir
        // os bytes e não ter o `.pom` para acompanhar entrega ao vizinho um
        // arquivo que ele não vai saber nomear - e o outro lado recusaria
        // depois de mover gigabytes. A recusa cabe aqui, antes deles.
        let descrita = encontrada.parent().is_some_and(|pasta| {
            crate::pom::load_dir(pasta).iter().any(|m| {
                m.parts
                    .iter()
                    .chain(m.tp_parts.iter())
                    .any(|p| p.sha256 == sha)
            })
        });
        if !descrita {
            return Err(format!(
                "a parte {curto} está aqui mas sem manifesto; uma fatia não é servida sem o `.pom` que a descreve"
            ));
        }
        encontrada
    } else {
        let requested = q
            .path
            .as_deref()
            .ok_or_else(|| "informe ?grant=, ?sha= ou ?path=".to_string())?;
        safe_model_path(&state.shard_dir().await, requested)?
    };
    // Também no caminho da concessão: é por ali que sai um arquivo de fora de
    // `models/`, e era a única porta sem checagem de tipo.
    ensure_gguf(&path)?;
    if let Ok(Some(count)) = crate::multipart::declared_split_count(&path) {
        return Err(format!(
            "este arquivo pertence a um GGUF multipart com {count} partes; transfira o conjunto completo ou fatie-o pelo POM antes de distribuir"
        ));
    }
    Ok(path)
}

/// Onde está a parte com este hash, se esta máquina a tem.
///
/// Procura `<sha256>.gguf` em cada pasta de modelo sob o diretório de fatias.
/// É de propósito que a busca não use o nome do modelo: o mesmo conteúdo pode
/// estar arquivado sob rodadas ou nomes diferentes em cada máquina, e o que
/// identifica a parte é o hash, não onde ela foi parar.
///
/// A validação do hash em `pom::part_path` é o que impede um `sha` vindo da
/// rede de virar `../`: sem ela isto seria travessia de diretório servida por
/// HTTP.
pub fn find_part(shard_dir: &Path, sha256: &str) -> Option<PathBuf> {
    if !crate::pom::is_sha256_hex(sha256) {
        return None;
    }
    for entrada in std::fs::read_dir(shard_dir).ok()?.flatten() {
        let pasta = entrada.path();
        if !pasta.is_dir() {
            continue;
        }
        if let Ok(p) = crate::pom::part_path(&pasta, sha256) {
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// Look up a grant. A stale or invented id is simply not a file here.
async fn grant_path(state: &GatewayState, id: &str) -> Result<PathBuf, String> {
    state
        .transfers
        .grants
        .read()
        .await
        .get(id)
        .cloned()
        .ok_or_else(|| "concessão desconhecida ou expirada".to_string())
}

async fn file_digest(state: &GatewayState, path: &Path) -> Result<(u64, String), String> {
    let meta = tokio::fs::metadata(path)
        .await
        .map_err(|e| format!("stat falhou: {e}"))?;
    let size = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let key = path.to_string_lossy().to_string();

    if let Some((m, s, digest)) = state.transfers.digests.read().await.get(&key) {
        if *m == mtime && *s == size {
            return Ok((size, digest.clone()));
        }
    }

    let p = path.to_path_buf();
    let digest = tokio::task::spawn_blocking(move || -> Result<String, String> {
        use std::io::Read;
        let mut f = std::fs::File::open(&p).map_err(|e| e.to_string())?;
        let mut hasher = Md5::new();
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = f.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(hex::encode(hasher.finalize()))
    })
    .await
    .map_err(|e| e.to_string())??;

    state
        .transfers
        .digests
        .write()
        .await
        .insert(key, (mtime, size, digest.clone()));
    Ok((size, digest))
}

// ── Serving side ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct PathQuery {
    /// Only meaningful inside `models/` — see [`servable_path`].
    #[serde(default)]
    pub path: Option<String>,
    /// Opaque id of a file our operator offered; may point anywhere.
    #[serde(default)]
    pub grant: Option<String>,
    /// sha256 de uma parte. É o localizador que não depende de caminho nenhum:
    /// quem pede não precisa saber como esta máquina organizou o disco, e o que
    /// chega ou tem esse hash ou é descartado. Ver [`find_part`].
    #[serde(default)]
    pub sha: Option<String>,
    /// Id cunhado pelo receptor. Ausente quando o peer é de uma versão antiga:
    /// a transferência funciona igual, só sem visão deste lado.
    #[serde(default)]
    pub job: Option<String>,
    /// `node_id` do receptor. Resolvido para nome pela tabela de peers local —
    /// nada que descreva o outro lado vem escrito no pedido.
    #[serde(default)]
    pub peer: Option<String>,
}

/// O manifesto do fatiamento que contém a parte pedida.
///
/// Existe para o caminho de **puxar**: quem oferece manda o `.pom` junto com a
/// oferta, mas quem puxa por conta própria (é o que a implantação faz ao mover
/// fatias) não recebe oferta nenhuma. Sem isto o arquivo chega com nome de
/// hash e nada que o explique, e o catálogo do destino o mostra como um modelo
/// inteiro - que era o defeito.
async fn serve_pom(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Query(q): Query<PathQuery>,
) -> Response {
    let Some(sha) = q.sha.as_deref().filter(|s| crate::pom::is_sha256_hex(s)) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "informe ?sha=<sha256>" })),
        )
            .into_response();
    };
    let shard_dir = state.shard_dir().await;
    let Some(caminho) = find_part(&shard_dir, sha) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "esta máquina não tem esta parte" })),
        )
            .into_response();
    };
    let manifesto = caminho.parent().and_then(|pasta| {
        crate::pom::load_dir(pasta).into_iter().find(|m| {
            m.parts
                .iter()
                .chain(m.tp_parts.iter())
                .any(|p| p.sha256 == sha)
        })
    });
    match manifesto {
        Some(m) => Json(m).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "a parte existe mas não há manifesto que a descreva" })),
        )
            .into_response(),
    }
}

/// What the receiver needs before it commits to a multi-gigabyte download.
async fn manifest(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Query(q): Query<PathQuery>,
) -> Response {
    let path = match servable_path(&state, &q).await {
        Ok(p) => p,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    };
    match file_digest(&state, &path).await {
        Ok((size, md5)) => Json(json!({
            "name": path.file_name().and_then(|n| n.to_str()).unwrap_or_default(),
            "path": path.to_string_lossy(),
            "size_bytes": size,
            "md5": md5,
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
    }
}

/// Stream a slice, honouring `Range` so an interrupted transfer can resume.
async fn download(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Query(q): Query<PathQuery>,
) -> Response {
    let path = match servable_path(&state, &q).await {
        Ok(p) => p,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    };

    let Ok(meta) = tokio::fs::metadata(&path).await else {
        return (StatusCode::NOT_FOUND, "arquivo não encontrado").into_response();
    };
    let total = meta.len();

    // "bytes=<start>-" is all the client ever sends; anything else is ignored.
    let start = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes="))
        .and_then(|v| v.split('-').next())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0)
        .min(total);

    let Ok(mut file) = tokio::fs::File::open(&path).await else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "não deu para abrir").into_response();
    };
    if start > 0 && file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
        return (StatusCode::INTERNAL_SERVER_ERROR, "seek falhou").into_response();
    }

    // O lado que envia entra na própria lista de transferências. Ele é quem
    // transmite os bytes, então já sabe o progresso: não há relato de volta
    // para se perder, e o número continua certo com a tela do outro fechada.
    if let Some(job_id) = q.job.as_deref() {
        let peer_host = peer_label(&state, q.peer.as_deref()).await;
        // O `.pom` está ao lado do arquivo: é dele que sai o "qwen3:8b · L0-15"
        // que a tela mostra no lugar do hash.
        let fatia = slice_label_for_path(&path);
        state
            .transfers
            .start_job(
                job_id,
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("fatia.gguf"),
                TRANSFER_SENDING,
                &peer_host,
                &path.to_string_lossy(),
                total,
                fatia.as_ref(),
            )
            .await;
    }

    let reader = tokio_util::io::ReaderStream::with_capacity(file, CHUNK);
    let body = match q.job.clone() {
        Some(job_id) => {
            Body::from_stream(served_stream(state.clone(), job_id, reader, start, total))
        }
        None => Body::from_stream(reader),
    };
    let remaining = total - start;

    let status = if start > 0 {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let mut resp = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, remaining.to_string())
        .header(header::ACCEPT_RANGES, "bytes");
    if start > 0 {
        resp = resp.header(
            header::CONTENT_RANGE,
            format!("bytes {}-{}/{}", start, total.saturating_sub(1), total),
        );
    }
    resp.body(body).unwrap().into_response()
}

/// Nome legível do peer que está do outro lado, a partir do `node_id` que ele
/// mandou. A tradução sai da tabela local: o pedido nunca traz o nome pronto.
async fn peer_label(state: &GatewayState, node_id: Option<&str>) -> String {
    let Some(node_id) = node_id else {
        return "desconhecido".to_string();
    };
    let short = |id: &str| id[..12.min(id.len())].to_string();
    state
        .all_peers()
        .await
        .iter()
        .find(|p| p.node_id == node_id)
        .map(|p| {
            if p.hostname.is_empty() {
                short(&p.node_id)
            } else {
                p.hostname.clone()
            }
        })
        .unwrap_or_else(|| short(node_id))
}

/// Envolve o stream do arquivo para contar o que já saiu e para parar cedo
/// quando o job é cancelado.
///
/// **É aqui que o lado que envia descobre que acabou.** Sem esse fecho o job do
/// remetente ficava `running` para sempre, com `bytes_done` congelado no
/// último relato de 500 ms - numa LAN rápida, zero - e o sino da máquina que
/// mandou o arquivo mostrava uma transferência eterna que já tinha terminado
/// do outro lado.
///
/// O fecho é o **último byte**, e não o fim do stream: quando o corpo satisfaz
/// o `Content-Length` prometido, o hyper para de puxar e larga o stream sem
/// nunca pedir o `None` que marcaria o fim. Contar os bytes é o único sinal
/// que sempre chega. Cancelamento não passa por aqui: `cancel` já fechou o job
/// com o nome de quem pediu, e `finish` respeita o que é terminal.
fn served_stream(
    state: GatewayState,
    job_id: String,
    inner: tokio_util::io::ReaderStream<tokio::fs::File>,
    already: u64,
    total: u64,
) -> impl futures::Stream<Item = std::io::Result<axum::body::Bytes>> {
    use futures::StreamExt;

    let started = Instant::now();
    futures::stream::unfold(
        (
            inner,
            state,
            job_id,
            already,
            already,
            started,
            Instant::now(),
        ),
        move |(mut inner, state, job_id, mut done, already, started, mut last)| async move {
            if state.transfers.is_cancelled(&job_id).await {
                // Terminar o corpo aqui corta a transferência: o receptor vê o
                // stream acabar antes do Content-Length prometido.
                return None;
            }
            let chunk = inner.next().await?;
            if let Ok(ref b) = chunk {
                done += b.len() as u64;
                let acabou = done >= total;
                if acabou || last.elapsed() >= Duration::from_millis(500) {
                    last = Instant::now();
                    state
                        .transfers
                        .progress(
                            &job_id,
                            done,
                            done.saturating_sub(already),
                            started.elapsed(),
                        )
                        .await;
                }
                if acabou {
                    state
                        .transfers
                        .finish(&job_id, TRANSFER_DONE, "enviado por inteiro")
                        .await;
                }
            }
            Some((chunk, (inner, state, job_id, done, already, started, last)))
        },
    )
}

// ── Offers (push direction) ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct OfferRequest {
    /// Node that will serve the bytes.
    pub from_node_id: String,
    pub from_host: String,
    pub from_gateway: String,
    pub remote_path: String,
    #[serde(default)]
    pub grant: Option<String>,
    pub name: String,
    pub size_bytes: u64,
    /// Opcional: um remetente atualizado não calcula digest para oferecer, e um
    /// antigo ainda manda o dele. Nenhum dos dois é lido — ver
    /// [`TransferOffer::md5`].
    #[serde(default)]
    pub md5: String,
    /// O manifesto do fatiamento, quando o que está sendo oferecido é uma
    /// parte. Ausente numa oferta de peer antigo ou de arquivo avulso.
    #[serde(default)]
    pub pom: Option<crate::pom::PomManifest>,
    #[serde(default)]
    pub start_layer: Option<u32>,
    #[serde(default)]
    pub end_layer: Option<u32>,
}

/// Onde uma oferta aceita vai parar, e o que precisa existir antes.
///
/// Com manifesto, a oferta é uma **parte**: ela cai em
/// `<shard_dir>/<modelo>/<sha>.gguf`, e o `.pom` é gravado ao lado dela antes
/// de o primeiro byte chegar. Era este o defeito: sem o manifesto, o arquivo
/// caía solto na raiz da pasta de fatias e o catálogo daqui o exibia como um
/// modelo inteiro de nome ilegível.
///
/// Gravar o manifesto **antes** é de propósito. Ele é a única coisa que
/// explica o arquivo, e um download interrompido no meio deixa um `.part` que
/// ninguém reclama; um manifesto órfão, por outro lado, é inofensivo - descreve
/// partes que o índice simplesmente não encontra.
///
/// Sem manifesto, o caminho antigo: um arquivo avulso em `recebidos/`.
async fn alvo_da_oferta(state: &GatewayState, offer: &TransferOffer) -> Result<Alvo, String> {
    let shard_dir = state.shard_dir().await;
    if let Some(m) = &offer.pom {
        let sha = std::path::Path::new(&offer.name)
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| crate::pom::is_sha256_hex(s))
            .map(|s| s.to_string())
            // O nome pode vir de um peer antigo; o manifesto ainda diz qual
            // parte é, desde que ele descreva exatamente uma.
            .or_else(|| {
                m.parts
                    .iter()
                    .chain(m.tp_parts.iter())
                    .next()
                    .map(|p| p.sha256.clone())
            });

        if let Some(sha) = sha {
            if m.parts
                .iter()
                .chain(m.tp_parts.iter())
                .any(|p| p.sha256 == sha)
            {
                // O manifesto recebido é apenas conhecimento remoto. Mescle-o
                // ao manifesto local para não apagar partes que este peer já
                // tinha do mesmo modelo original.
                let pasta = match crate::pom::merge_manifest(&shard_dir, m) {
                    Ok((pasta, _)) => pasta,
                    Err(e) => {
                        return Err(format!(
                            "não deu para mesclar o manifesto de {}: {e:#}",
                            m.model.label
                        ));
                    }
                };
                info!(
                    "manifesto de {} guardado; a parte vai para {}",
                    m.model.label,
                    pasta.display()
                );
                return Ok(Alvo::Parte {
                    sha256: sha,
                    model_dir: Some(pasta),
                    model_hint: m.model.name.clone(),
                });
            }
        }
        return Err(format!(
            "a oferta trouxe o manifesto de {} mas o arquivo não é parte dele",
            m.model.label
        ));
    }

    // Sem manifesto, o arquivo só é aceitável se **não** for uma parte. Um
    // nome de hash sem `.pom` é uma fatia órfã, e aceitá-la seria guardar um
    // arquivo que ninguém sabe nomear - a regra é a mesma dos dois lados.
    if std::path::Path::new(&offer.name)
        .file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(crate::pom::is_sha256_hex)
    {
        return Err(format!(
            "{} ofereceu uma fatia sem o `.pom` que a descreve",
            offer.from_host
        ));
    }

    Ok(Alvo::Avulso {
        remote_path: offer.remote_path.clone(),
        grant: offer.grant.clone(),
        name: offer.name.clone(),
    })
}

/// A peer offers us a slice. Nothing is downloaded until someone accepts.
async fn receive_offer(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<OfferRequest>,
) -> Response {
    if let Err(e) = valid_transfer_filename(&req.name) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
    }
    let offer = TransferOffer {
        id: uuid::Uuid::new_v4().simple().to_string(),
        from_node_id: req.from_node_id,
        from_host: req.from_host,
        from_gateway: req.from_gateway,
        remote_path: req.remote_path,
        grant: req.grant,
        name: req.name.clone(),
        size_bytes: req.size_bytes,
        md5: req.md5,
        pom: req.pom,
        start_layer: req.start_layer,
        end_layer: req.end_layer,
    };
    info!(
        "📥 oferta de fatia recebida: {} ({} bytes)",
        offer.name, offer.size_bytes
    );
    let id = offer.id.clone();

    // Development gate, off by default and read from *this* machine's
    // environment: the sender has no way to ask for it. See `dev_auto_accept_env`.
    if dev_auto_accept() {
        warn!(
            "⚙️  {} ligado — aceitando {} sem confirmação",
            dev_auto_accept_env(),
            offer.name
        );
        let alvo = match alvo_da_oferta(&state, &offer).await {
            Ok(a) => a,
            Err(e) => {
                warn!("aceite automático de {} recusado: {e}", offer.name);
                state.transfers.offers.write().await.push(offer);
                return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
            }
        };
        return match spawn_download(
            state.clone(),
            offer.from_node_id.clone(),
            offer.from_gateway.clone(),
            alvo,
            offer.from_host.clone(),
        )
        .await
        {
            Ok(job_id) => {
                notify_offerer(&state, &offer, true, Some(job_id.clone()));
                Json(json!({
                    "accepted_for_review": false,
                    "auto_accepted": true,
                    "offer_id": id,
                    "job_id": job_id,
                }))
                .into_response()
            }
            // The offer still goes on the shelf: an auto-accept that could not
            // start is a transfer the operator can still take by hand, and
            // swallowing it would lose the file with only a log line to say so.
            Err(e) => {
                warn!("aceite automático de {} falhou: {e}", offer.name);
                state.transfers.offers.write().await.push(offer);
                Json(json!({
                    "accepted_for_review": true,
                    "auto_accept_failed": e,
                    "offer_id": id,
                }))
                .into_response()
            }
        };
    }

    state.transfers.offers.write().await.push(offer);
    Json(json!({ "accepted_for_review": true, "offer_id": id })).into_response()
}

async fn list_offers(
    _auth: ConnectionAuth,
    headers: HeaderMap,
    State(state): State<GatewayState>,
) -> Response {
    let slice_offers = state.transfers.offers.read().await.clone();
    let mut offers = slice_offers
        .into_iter()
        .map(|offer| serde_json::to_value(offer).expect("oferta de fatia serializável"))
        .collect::<Vec<_>>();
    if let Some(offer) = state.admin_trust.pending_transfer() {
        if offer.successor == state.authority_principal {
            let peers = state.network.alive_peers().await;
            let from_host = peers
                .iter()
                .find(|peer| peer.node_id == offer.issuer.node_id)
                .map(|peer| peer.hostname.clone())
                .filter(|hostname| !hostname.is_empty())
                .unwrap_or_else(|| offer.issuer.node_id.clone());
            let target_host = state
                .auth
                .read()
                .await
                .node_name
                .clone()
                .unwrap_or_else(|| state.network.node_id.clone());
            offers.push(json!({
                "kind": "principal_transfer",
                "id": offer.offer_id,
                "from_node_id": offer.issuer.node_id,
                "from_host": from_host,
                "target_node_id": offer.successor.node_id,
                "target_host": target_host,
                "status": "pending",
                "issued_at": offer.command_id,
            }));
        }
    }
    crate::webui::conditional_json(&headers, &json!({ "offers": offers }))
}

async fn sysmon_disk_free_mb() -> u64 {
    crate::sysmon::sample().await.disk.free_mb
}

/// O veredito do outro lado sobre uma oferta que saiu daqui.
///
/// Bater na porta e nunca saber se abriram era metade do defeito: o operador
/// mandava uma fatia e a tela dele não dizia mais nada - nem "aceitou", nem
/// "recusou". O receptor agora responde, e o remetente registra o desfecho
/// como um job, que é o vocabulário que o sino já lê.
#[derive(Debug, Deserialize)]
pub struct AnswerRequest {
    /// A concessão que viajou na oferta. É o único id que as duas pontas
    /// compartilham antes de existir um job.
    pub grant: String,
    /// Quem está respondendo. Confere com o destino registrado: um veredito
    /// que não vem de quem recebeu a oferta descreve outra conversa.
    #[serde(default)]
    pub from_node_id: String,
    pub accepted: bool,
    /// O id que o receptor cunhou para o download, quando aceitou. Registrar o
    /// mesmo id de um lado e do outro é o que faz o job do remetente ser a
    /// mesma transferência, e não uma segunda linha na lista.
    #[serde(default)]
    pub job_id: Option<String>,
    /// Por que recusou, quando há motivo além da vontade de quem decidiu.
    #[serde(default)]
    pub reason: Option<String>,
}

async fn receive_answer(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<AnswerRequest>,
) -> Response {
    // A oferta sai do registro em qualquer veredito: ela foi respondida.
    let oferta = {
        let mut registro = state.transfers.offered.write().await;
        // Um `grant` que esta máquina não ofereceu não descreve nada que ela
        // possa mostrar. Não é erro do outro lado - pode ser um reinício aqui.
        let Some(o) = registro.get(&req.grant) else {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "oferta desconhecida" })),
            )
                .into_response();
        };
        if !req.from_node_id.is_empty() && req.from_node_id != o.to_node_id {
            warn!(
                "veredito sobre {} veio de {} e não de {}",
                o.name, req.from_node_id, o.to_node_id
            );
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": "esta oferta não foi feita a este nó" })),
            )
                .into_response();
        }
        registro.remove(&req.grant).expect("acabou de ser lida")
    };

    if req.accepted {
        // O aceite chega **antes** do primeiro byte, e a distância entre os
        // dois é o tempo de o receptor pedir o digest - minutos num volume
        // lento. Registrar o job já aqui é o que tira a tela do remetente do
        // silêncio nesse intervalo; quando os bytes começarem, `download`
        // reescreve este mesmo id com o tamanho real.
        let job_id = req
            .job_id
            .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
        info!("👍 {} aceitou {}", oferta.to_host, oferta.name);
        state
            .transfers
            .start_job(
                &job_id,
                &oferta.name,
                TRANSFER_SENDING,
                &oferta.to_host,
                &oferta.path,
                oferta.size_bytes,
                oferta.fatia.as_ref(),
            )
            .await;
        set_detail(&state, &job_id, "aceito; aguardando o início do envio").await;
        return Json(json!({ "noted": true, "job_id": job_id })).into_response();
    }

    // Recusa não é falha: foi decisão de alguém, e a tela precisa dizer de
    // quem. `cancelled` é o estado que já carrega esse sentido.
    let job_id = req
        .job_id
        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
    info!("👎 {} recusou {}", oferta.to_host, oferta.name);
    state
        .transfers
        .start_job(
            &job_id,
            &oferta.name,
            TRANSFER_SENDING,
            &oferta.to_host,
            &oferta.path,
            oferta.size_bytes,
            oferta.fatia.as_ref(),
        )
        .await;
    let motivo = match req
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        Some(r) => format!("recusado por {}: {r}", oferta.to_host),
        None => format!("recusado por {}", oferta.to_host),
    };
    state
        .transfers
        .finish(&job_id, TRANSFER_CANCELLED, motivo)
        .await;
    Json(json!({ "noted": true, "job_id": job_id })).into_response()
}

/// Conta a quem ofereceu o que foi decidido aqui.
///
/// Best-effort e em segundo plano: o clique de quem decidiu não pode esperar a
/// outra máquina responder, e um remetente que saiu do ar não é motivo para a
/// recusa falhar na tela de quem recusou.
fn notify_offerer(
    state: &GatewayState,
    offer: &TransferOffer,
    accepted: bool,
    job_id: Option<String>,
) {
    let Some(grant) = offer.grant.clone() else {
        // Oferta de um peer anterior a `grant`: não há id compartilhado para
        // nomear o veredito, e inventar um só produziria ruído do outro lado.
        return;
    };
    let state = state.clone();
    let from_node_id = offer.from_node_id.clone();
    let base_url = normaliza_esquema(&offer.from_gateway);
    let from_host = offer.from_host.clone();
    tokio::spawn(async move {
        let key = state.auth.read().await.connection_key.clone();
        let cliente = match state.peer_http.for_peer(&from_node_id) {
            Ok(c) => c,
            Err(e) => {
                warn!("sem cliente para avisar {from_host} do veredito: {e}");
                return;
            }
        };
        let body = json!({
            "grant": grant,
            "from_node_id": state.network.node_id,
            "accepted": accepted,
            "job_id": job_id,
        });
        match cliente
            .post(format!("{base_url}/api/transfer/offers/answer"))
            .bearer_auth(&key)
            .json(&body)
            .timeout(Duration::from_secs(30))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {}
            Ok(r) => warn!("{from_host} respondeu {} ao veredito", r.status()),
            Err(e) => warn!("não deu para avisar {from_host} do veredito: {e}"),
        }
    });
}

async fn reject_offer(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if state
        .admin_trust
        .pending_transfer()
        .is_some_and(|offer| offer.offer_id == id && offer.successor == state.authority_principal)
    {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "a transferência de principal só termina com aceitação assinada ou com uma nova época"
            })),
        )
            .into_response();
    }
    let recusada = {
        let mut offers = state.transfers.offers.write().await;
        offers
            .iter()
            .position(|o| o.id == id)
            .map(|i| offers.remove(i))
    };
    if let Some(offer) = &recusada {
        notify_offerer(&state, offer, false, None);
    }
    Json(json!({ "rejected": true })).into_response()
}

async fn accept_offer(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if state
        .admin_trust
        .pending_transfer()
        .is_some_and(|offer| offer.offer_id == id && offer.successor == state.authority_principal)
    {
        return crate::admin::accept_authority_transfer(&state, &id).await;
    }
    let offer = {
        let mut offers = state.transfers.offers.write().await;
        match offers.iter().position(|o| o.id == id) {
            Some(i) => offers.remove(i),
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "oferta não existe" })),
                )
                    .into_response()
            }
        }
    };

    let alvo = match alvo_da_oferta(&state, &offer).await {
        Ok(a) => a,
        Err(e) => {
            // A oferta não pôde nem começar: quem ofereceu precisa saber, ou
            // fica esperando um download que nunca vai acontecer.
            notify_offerer(&state, &offer, false, None);
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
        }
    };
    match spawn_download(
        state.clone(),
        offer.from_node_id.clone(),
        offer.from_gateway.clone(),
        alvo,
        offer.from_host.clone(),
    )
    .await
    {
        Ok(job_id) => {
            notify_offerer(&state, &offer, true, Some(job_id.clone()));
            Json(json!({ "started": true, "job_id": job_id })).into_response()
        }
        Err(e) => {
            notify_offerer(&state, &offer, false, None);
            (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response()
        }
    }
}

// ── Pull direction ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct PullRequest {
    /// Hash da parte que se quer. Presente, é ele que localiza o arquivo do
    /// outro lado, e `remote_path` deixa de importar.
    #[serde(default)]
    pub sha256: Option<String>,
    /// Nome do modelo, para saber em que pasta a parte vai cair aqui. Só é lido
    /// junto com `sha256`.
    #[serde(default)]
    pub model: Option<String>,
    /// Peer holding the file.
    pub node_id: String,
    /// Só para o pedido por caminho; um pedido por `sha256` não tem o que
    /// dizer aqui, porque não sabe (nem precisa saber) como o outro lado
    /// organizou o disco.
    #[serde(default)]
    pub remote_path: String,
    #[serde(default)]
    pub name: String,
}

async fn start_pull(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<PullRequest>,
) -> Response {
    // A validação do nome vale para o pedido **por caminho**, em que `name` é
    // o que vira arquivo aqui. Um pedido por `sha256` não manda nome nenhum: o
    // dele é derivado do hash (`<sha256>.gguf`) e conferido dentro do
    // `spawn_download`, junto com o resto. Exigir `name` aqui recusava toda
    // cópia endereçada por conteúdo - inclusive as que a implantação faz.
    if req.sha256.is_none() {
        if let Err(e) = valid_transfer_filename(&req.name) {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
        }
    }
    let peers = state.all_peers().await;
    let Some(peer) = peers.iter().find(|p| p.node_id == req.node_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "peer desconhecido" })),
        )
            .into_response();
    };
    let Some(base) = peer.gateway_url() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "peer não anuncia porta de gateway" })),
        )
            .into_response();
    };
    let host = if peer.hostname.is_empty() {
        peer.node_id[..12].to_string()
    } else {
        peer.hostname.clone()
    };

    // Duas formas de pedir, e a diferença é quem garante o quê. Com `sha256` a
    // máquina pede um **conteúdo**: o peer procura quem tem aquele hash, o
    // arquivo cai direto na pasta do modelo e só recebe o nome depois de
    // conferido. Sem ele, é o caminho antigo - um arquivo avulso, pelo caminho
    // que o outro lado expõe, com o digest que ele declarar.
    let alvo = match req.sha256.as_deref() {
        Some(sha) if crate::pom::is_sha256_hex(sha) => {
            let modelo = req.model.as_deref().unwrap_or("partes");
            Alvo::Parte {
                sha256: sha.to_string(),
                // A pasta depende do id da rodada, que vem no manifesto que o
                // download vai buscar na origem.
                model_dir: None,
                model_hint: modelo.to_string(),
            }
        }
        Some(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "sha256 inválido" })),
            )
                .into_response();
        }
        None => Alvo::Avulso {
            remote_path: req.remote_path,
            grant: None,
            name: req.name,
        },
    };

    match spawn_download(state.clone(), peer.node_id.clone(), base, alvo, host).await {
        Ok(job_id) => Json(json!({ "started": true, "job_id": job_id })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

async fn list_jobs(_auth: ConnectionAuth, State(state): State<GatewayState>) -> Response {
    let mut jobs: Vec<TransferJob> = state
        .transfers
        .jobs
        .read()
        .await
        .values()
        .cloned()
        .collect();
    jobs.sort_by(|a, b| a.name.cmp(&b.name));
    Json(json!({ "jobs": jobs })).into_response()
}

#[derive(Debug, Deserialize)]
pub struct CancelRequest {
    /// Máquina que pediu o cancelamento, para o outro lado poder dizer quem foi.
    #[serde(default)]
    pub by: Option<String>,
    /// `true` quando a chamada veio do peer. Evita o eco: quem foi avisado não
    /// avisa de volta.
    #[serde(default)]
    pub from_peer: bool,
}

/// Cancela uma transferência.
///
/// A mesma rota serve para a tela local e para o aviso que o outro lado manda —
/// um verbo só, com a mesma semântica nas duas pontas.
async fn cancel_job(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    AxumPath(id): AxumPath<String>,
    body: Option<Json<CancelRequest>>,
) -> Response {
    let req = body.map(|Json(b)| b).unwrap_or(CancelRequest {
        by: None,
        from_peer: false,
    });
    let by = req.by.unwrap_or_else(|| state.network.hostname.clone());

    let peer_host = state
        .transfers
        .jobs
        .read()
        .await
        .get(&id)
        .map(|j| j.peer_host.clone());

    state.transfers.cancel(&id, &by).await;
    info!("🛑 transferência {id} cancelada por {by}");

    // Avisar é o que separa "cancelado" de "caiu a rede" na tela do outro lado.
    // Se o aviso não chegar, o cancelamento local continua valendo e o outro
    // lado cai no caminho de conexão morta, que reporta o motivo real.
    if !req.from_peer {
        if let Some(host) = peer_host {
            notify_peer_cancel(&state, &id, &by, &host).await;
        }
    }

    Json(json!({ "cancelled": true })).into_response()
}

/// Repassa o cancelamento ao peer que está do outro lado desta transferência.
async fn notify_peer_cancel(state: &GatewayState, id: &str, by: &str, peer_host: &str) {
    let peers = state.all_peers().await;
    let Some(peer) = peers
        .iter()
        .find(|p| p.hostname == peer_host || p.node_id.starts_with(peer_host))
    else {
        warn!("não deu para avisar {peer_host} do cancelamento de {id}: peer desconhecido");
        return;
    };
    let Some(base) = peer.gateway_url() else {
        warn!("não deu para avisar {peer_host}: peer não anuncia porta de gateway");
        return;
    };
    let key = state.auth.read().await.connection_key.clone();
    let Ok(cliente) = state.peer_http.for_peer(&peer.node_id) else {
        warn!("não deu para avisar {peer_host}: sem cliente para o peer");
        return;
    };
    let sent = cliente
        .post(format!("{base}/api/transfer/jobs/{id}/cancel"))
        .bearer_auth(&key)
        .json(&json!({ "by": by, "from_peer": true }))
        .timeout(Duration::from_secs(10))
        .send()
        .await;
    if let Err(e) = sent {
        warn!("aviso de cancelamento não chegou em {peer_host}: {e}");
    }
}

#[derive(Debug, Deserialize)]
pub struct SendRequest {
    /// Peer that should receive the file.
    pub to_node_id: String,
    /// Basename from this node's catalog. Paths are deliberately not accepted.
    pub filename: String,
}

/// Resolve the basename the local operator selected against this node's catalog.
///
/// The browser never gets permission to choose a path. A duplicate basename is
/// rejected rather than guessed, because guessing would make a transfer depend
/// on scan order and could send a different model than the operator saw.
async fn catalog_send_path(state: &GatewayState, filename: &str) -> Result<PathBuf, String> {
    let filename = valid_transfer_filename(filename)?;
    let model_dirs = state.model_dirs().await;
    let shard_dir = state.shard_dir().await;
    let catalog =
        tokio::task::spawn_blocking(move || crate::catalog::discover(&model_dirs, &shard_dir))
            .await
            .map_err(|e| format!("falha ao consultar o catálogo local: {e}"))?;
    let matches: Vec<PathBuf> = catalog
        .models
        .into_iter()
        .filter(|model| {
            Path::new(&model.path)
                .file_name()
                .and_then(|name| name.to_str())
                == Some(filename)
        })
        .map(|model| PathBuf::from(model.path))
        .collect();
    match matches.as_slice() {
        [] => Err(format!("arquivo não encontrado no catálogo local: {filename}")),
        [path] => local_send_path(path.to_string_lossy().as_ref()),
        _ => Err(format!(
            "há mais de um arquivo chamado {filename} no catálogo local; renomeie um deles antes de transferir"
        )),
    }
}

/// Validate a path already resolved by the local catalog before granting it.
fn local_send_path(requested: &str) -> Result<PathBuf, String> {
    let full = Path::new(requested)
        .canonicalize()
        .map_err(|_| format!("arquivo não encontrado: {requested}"))?;
    if !full.is_file() {
        return Err(format!("não é um arquivo: {requested}"));
    }
    ensure_gguf(&full)?;
    Ok(full)
}

/// O manifesto do fatiamento a que este arquivo pertence, se ele for uma parte.
///
/// Procura pelo caminho: uma parte mora em `<shard_dir>/<modelo>/<sha>.gguf`, e
/// os `.pom` que a descrevem estão na mesma pasta. Um arquivo fora da pasta de
/// fatias, ou dentro dela mas não listado em manifesto nenhum, não é parte de
/// coisa alguma e vai sozinho, como sempre foi.
async fn manifest_for(state: &GatewayState, path: &Path) -> Option<crate::pom::PomManifest> {
    let sha = path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| crate::pom::is_sha256_hex(s))?;
    let pasta = path.parent()?;
    // Só vale um manifesto que esteja na pasta do próprio arquivo: um `.pom`
    // de outro lugar poderia descrever uma parte homônima de outra rodada.
    let _ = state;
    crate::pom::load_dir(pasta).into_iter().find(|m| {
        m.parts
            .iter()
            .chain(m.tp_parts.iter())
            .any(|p| p.sha256 == sha)
    })
}

/// Offer one of our slices to a peer.
///
/// This only knocks on the door: the peer stores the offer and its owner
/// decides. We never write to another machine's disk uninvited.
///
/// **Bater na porta não pode custar o preço de ler o arquivo.** Este caminho já
/// calculou o MD5 da fatia antes de postar a oferta, e como o digest é uma
/// leitura do arquivo inteiro, o sino do outro lado só acendia quando ela
/// terminava: medidos 3 min 23 s para uma fatia de 4,2 GB no cartão SD
/// (~20 MB/s). O cache por (caminho, mtime, tamanho) mascarava o atraso no
/// segundo envio do mesmo arquivo, e era daí que vinha o "às vezes" do relato.
/// O número nem era usado: quem confere o download é o receptor, com o MD5 que
/// ele pede em `/api/transfer/manifest` depois de aceitar. Aqui basta o
/// `stat` — tamanho e nome, que é o que a notificação mostra.
async fn send_to_peer(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<SendRequest>,
) -> Response {
    let path = match catalog_send_path(&state, &req.filename).await {
        Ok(p) => p,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    };

    if let Ok(Some(count)) = crate::multipart::declared_split_count(&path) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!(
                    "o arquivo escolhido pertence a um GGUF multipart com {count} partes; mantenha o conjunto junto ou fatie-o pelo POM antes de transferir"
                )
            })),
        )
            .into_response();
    }

    let size = match tokio::fs::metadata(&path).await {
        Ok(m) => m.len(),
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("stat falhou: {e}") })),
            )
                .into_response()
        }
    };

    let peers = state.all_peers().await;
    let Some(peer) = peers.iter().find(|p| p.node_id == req.to_node_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "peer desconhecido" })),
        )
            .into_response();
    };
    let Some(peer_base) = peer.gateway_url() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "peer não anuncia porta de gateway" })),
        )
            .into_response();
    };

    // O nome vem do pedido: o `main` passou a deixar quem envia nomeá-lo.
    let name = req.filename.trim().to_string();

    // O manifesto vai junto com a oferta. É ele que diz ao outro lado de que
    // modelo esta parte é e que camadas ela traz - sem isso o arquivo chega
    // com nome de hash e sem dono, e o catálogo de lá o mostra como um modelo
    // inteiro qualquer. Quem identifica a parte é o **caminho** do arquivo que
    // está sendo mandado, não o nome que o pedido escolheu para ele.
    let pom = manifest_for(&state, &path).await;
    let sha = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();

    // **Uma parte não sai daqui sem o `.pom` dela.** Um arquivo chamado
    // `<sha256>.gguf` é, por construção, parte de um fatiamento; se o manifesto
    // que o descreve não está ao lado dele, esta máquina tem um órfão em disco
    // e mandá-lo adiante só espalharia o problema. Recusar aqui diz onde
    // consertar; mandar mesmo assim entrega ao vizinho um arquivo que ele não
    // vai saber nomear.
    if pom.is_none() && crate::pom::is_sha256_hex(sha) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!(
                    "a parte {} não tem manifesto nesta máquina; uma fatia não viaja sem o `.pom` que a descreve",
                    &sha[..12]
                )
            })),
        )
            .into_response();
    }

    // A faixa de camadas saía daqui lida do nome do arquivo. Agora sai do
    // manifesto, que é quem sabe; sem manifesto, não se inventa.
    let fatia = pom.as_ref().and_then(|m| slice_label(m, sha));
    let shard: Option<(u32, u32)> = fatia.as_ref().map(|f| (f.start_layer, f.end_layer));

    // Where the peer should come and fetch it from — our own gateway, on the
    // address the peer already reaches us at.
    let my_host = state.network.hostname.clone();

    // The peer fetches by grant id, never by path: that is what lets this file
    // sit outside `models/` without opening the disk to whoever holds the key.
    let grant = uuid::Uuid::new_v4().simple().to_string();
    state
        .transfers
        .grants
        .write()
        .await
        .insert(grant.clone(), path.clone());

    // Guardado antes de a oferta sair: o veredito pode voltar antes de esta
    // função retornar, e sem o registro o remetente receberia um "aceitei" sem
    // saber de qual arquivo. O nome do destino vem da tabela local de peers,
    // que é quem sabe traduzir `node_id` em nome de máquina.
    state.transfers.offered.write().await.insert(
        grant.clone(),
        OfferedFile {
            to_node_id: req.to_node_id.clone(),
            to_host: if peer.hostname.is_empty() {
                peer.node_id[..12.min(peer.node_id.len())].to_string()
            } else {
                peer.hostname.clone()
            },
            name: name.clone(),
            path: path.to_string_lossy().into_owned(),
            size_bytes: size,
            fatia: fatia.clone(),
        },
    );

    let body = json!({
        "from_node_id": state.network.node_id,
        "from_host": my_host,
        // A pinned address beats one read off the routing table: with privacy
        // extensions the outbound source is a temporary IPv6 that rotates, and a
        // multi-gigabyte download outlives it.
        // O ESQUEMA em vigor, nunca "http" fixo. Com o cluster em TLS, quem
        // aceitava discava http e o gateway respondia 403 em texto puro
        // ("este cluster exige TLS entre nós; refaça a chamada em https"); o
        // `.json()` do outro lado caía nessa frase e o operador via
        // "manifesto inválido: error decoding response body" (18/08).
        "from_gateway": crate::peer_client::peer_base_url(
            &state
                .network
                .advertised_host()
                .unwrap_or_else(|| local_ip_for(&peer_base)),
            state.gateway_port,
            crate::peer_client::tls_ativo(),
        ),
        "grant": grant,
        "remote_path": path.to_string_lossy(),
        "name": name,
        "size_bytes": size,
        // Sempre vazio. Viaja só porque um peer de versão anterior exige o
        // campo — ver `TransferOffer::md5`.
        "md5": "",
        "pom": pom,
        "start_layer": shard.map(|(a, _)| a),
        "end_layer": shard.map(|(_, b)| b),
    });

    let key = state.auth.read().await.connection_key.clone();
    let cliente = match state.peer_http.for_peer(&req.to_node_id) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": format!("sem cliente para o peer: {e}") })),
            )
                .into_response()
        }
    };
    match cliente
        .post(format!("{peer_base}/api/transfer/offer"))
        .bearer_auth(&key)
        .json(&body)
        .timeout(Duration::from_secs(30))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => {
            info!("📤 fatia {name} oferecida a {}", peer.node_id);
            Json(json!({ "offered": true })).into_response()
        }
        Ok(r) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("peer respondeu {}", r.status()) })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("não deu para oferecer: {e}") })),
        )
            .into_response(),
    }
}

/// Our address as seen from the peer we are talking to, ready to drop into a URL.
///
/// The peer will open a connection back to us, so the offer has to carry an
/// address that works from *its* side — `127.0.0.1` would send it to itself.
/// That is exactly what happened to every IPv6 peer: the host was pulled out
/// with `split(':')`, which on `http://[2804:...]:8090` yields `"[2804"`, fails
/// to parse as an address, and fell back to loopback. The receiver then
/// downloaded from its own machine.
fn local_ip_for(peer_base: &str) -> String {
    match url_host(peer_base).parse::<std::net::IpAddr>() {
        Ok(ip) => local_ip_towards(ip),
        Err(_) => "127.0.0.1".to_string(),
    }
}

/// Host part of `http://host:port`, with IPv6 brackets stripped.
fn url_host(base: &str) -> &str {
    let rest = base
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    if let Some(inner) = rest.strip_prefix('[') {
        return inner.split(']').next().unwrap_or("127.0.0.1");
    }
    rest.split(':').next().unwrap_or("127.0.0.1")
}

/// Ask the OS which local address it would use to reach `dest`.
///
/// Returned bracketed when it is IPv6, because the only caller interpolates it
/// straight into a URL.
pub(crate) fn local_ip_towards(dest: std::net::IpAddr) -> String {
    // A connected UDP socket performs no traffic but fixes the route. It has to
    // be bound in the destination's family: connecting a v4 socket to a v6
    // address fails outright.
    let bind: &str = if dest.is_ipv6() {
        "[::]:0"
    } else {
        "0.0.0.0:0"
    };
    std::net::UdpSocket::bind(bind)
        .and_then(|s| {
            s.connect((dest, 9))?;
            s.local_addr()
        })
        .map(|a| match a.ip() {
            std::net::IpAddr::V6(v6) => format!("[{v6}]"),
            ip => ip.to_string(),
        })
        .unwrap_or_else(|_| {
            if dest.is_ipv6() {
                "[::1]".to_string()
            } else {
                "127.0.0.1".to_string()
            }
        })
}

// ── The download itself ──────────────────────────────────────────────────────

/// Prazo do manifesto.
///
/// O manifesto é onde o MD5 do arquivo é finalmente calculado, e um digest é
/// uma leitura do arquivo inteiro: no cartão SD do captain, 4,2 GB levam
/// 3 min 23 s. Os 120 s de antes só passavam porque o remetente já tinha
/// hasheado ao oferecer — o que era exatamente o defeito. Com o hash de volta
/// para onde ele é preciso, o prazo tem que caber num volume lento; ele corre
/// em segundo plano, então esperar aqui não prende ninguém.
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Registra a transferência e faz o resto — manifesto, espaço em disco e
/// download — em segundo plano.
///
/// Consultar o outro lado **não** acontece antes de devolver: o manifesto pode
/// levar minutos (ver [`MANIFEST_TIMEOUT`]), e aceitar uma oferta é um clique,
/// não uma espera. O que o operador ganha em troca é o job já na lista de
/// transferências, dizendo em que etapa está; um erro de manifesto vira job
/// `failed` com o motivo, no mesmo lugar onde ele já procuraria.
/// `peer_id` é o node_id de quem serve os bytes: é o que fixa o certificado do
/// outro lado quando o cluster está em TLS. Vem da oferta (`from_node_id`) ou
/// do `PeerInfo` que o operador escolheu.
/// O esquema que **esta** máquina vai discar, seja qual for o que a oferta traz.
///
/// A oferta é escrita pelo remetente, e um nó de versão anterior grava `http://`
/// fixo. Confiar nisso com o cluster em TLS rende 403 em texto puro
/// ("este cluster exige TLS entre nós"), que o `.json()` transforma em
/// "manifesto inválido: error decoding response body" - uma mensagem que não
/// aponta para lugar nenhum. A política local é a que vale para discar, então é
/// ela que decide.
fn normaliza_esquema(url: &str) -> String {
    let scheme = crate::peer_client::peer_scheme();
    match url.split_once("://") {
        Some((atual, resto)) if atual != scheme => format!("{scheme}://{resto}"),
        _ => url.to_string(),
    }
}

/// O que se está buscando e onde ele vai parar.
///
/// Duas formas: um arquivo avulso, localizado por caminho ou concessão e
/// largado em `recebidos/`; ou uma **parte**, localizada pelo hash e gravada
/// direto na pasta do modelo com o hash por nome. A segunda dispensa a
/// quarentena: o nome só é dado depois da conferência, então um arquivo com
/// nome de parte é, por construção, uma parte conferida.
enum Alvo {
    Avulso {
        remote_path: String,
        grant: Option<String>,
        name: String,
    },
    Parte {
        sha256: String,
        /// A pasta, quando quem pediu já tinha o manifesto em mãos (é o caso de
        /// uma oferta, que traz o `.pom` junto).
        ///
        /// `None` quando só se sabe o hash: aí a pasta sai do manifesto que o
        /// download busca na origem, porque o nome dela carrega o id da rodada
        /// e sem o manifesto não há id.
        model_dir: Option<PathBuf>,
        /// Nome do modelo para a mensagem, quando não há manifesto.
        model_hint: String,
    },
}

async fn spawn_download(
    state: GatewayState,
    peer_id: String,
    base_url: String,
    alvo: Alvo,
    from_host: String,
) -> Result<String, String> {
    // A oferta traz o esquema que o REMETENTE gravou; quem disca é esta
    // máquina, então a política local é a que vale. Ver `normaliza_esquema`.
    let base_url = normaliza_esquema(&base_url);
    let key = state.auth.read().await.connection_key.clone();

    // O id nasce aqui e viaja no pedido: o outro lado registra o mesmo id, e é
    // isso que faz "cancelar" valer para a transferência inteira, dos dois
    // lados, em vez de só para a metade local.
    let job_id = uuid::Uuid::new_v4().simple().to_string();

    // A grant, when we have one, is the only locator the serving side needs —
    // and the only one that reaches a file outside its `models/`.
    let (mut locator, dir, name, expected_sha): (
        Vec<(&str, String)>,
        PathBuf,
        String,
        Option<String>,
    ) = match &alvo {
        Alvo::Avulso {
            remote_path,
            grant,
            name,
        } => {
            let loc = match grant {
                Some(g) => vec![("grant", g.clone())],
                None => vec![("path", remote_path.clone())],
            };
            (
                loc,
                state.shard_dir().await.join("recebidos"),
                name.clone(),
                None,
            )
        }
        Alvo::Parte {
            sha256,
            model_dir,
            model_hint,
        } => {
            // **O manifesto é obrigatório.** A pasta de uma parte leva o id da
            // rodada, e o id vem do `.pom`; sem ele não há onde a parte caiba.
            //
            // Antes daqui havia um plano B - largar a parte em `recebidos/` -
            // e ele estava errado: uma parte solta, sem nada que diga de que
            // modelo é nem que camadas traz, é um arquivo que ninguém sabe
            // usar e que a tela mostra como se fosse um modelo inteiro. É
            // melhor a transferência falhar dizendo o que faltou do que
            // terminar bem e deixar lixo com cara de fatia.
            //
            // Quem chegou por oferta já traz o manifesto; quem puxa por hash
            // pede à origem aqui, antes de qualquer byte de peso - um `.pom`
            // são alguns kB, e descobrir agora custa muito menos que descobrir
            // depois de mover gigabytes.
            let pasta = match model_dir {
                Some(p) => p.clone(),
                None => {
                    let m = fetch_pom(&state, &peer_id, &base_url, &key, sha256)
                        .await
                        .ok_or_else(|| {
                            format!(
                                "{from_host} não tem o manifesto da parte {} ({model_hint}); \
                                 uma parte não viaja sem o `.pom` que a descreve",
                                &sha256[..12]
                            )
                        })?;
                    if !m
                        .parts
                        .iter()
                        .chain(m.tp_parts.iter())
                        .any(|p| &p.sha256 == sha256)
                    {
                        return Err(format!(
                            "o manifesto que {from_host} mandou não descreve a parte {}",
                            &sha256[..12]
                        ));
                    }
                    // Gravar o manifesto é parte da transferência, não um
                    // extra. O merge preserva partes que já existiam neste
                    // peer e adiciona apenas o conhecimento remoto que falta.
                    let p = crate::pom::merge_manifest(&state.shard_dir().await, &m)
                        .map(|(path, _)| path)
                        .map_err(|e| {
                            format!(
                                "não deu para mesclar o manifesto de {}: {e:#}",
                                m.model.label
                            )
                        })?;
                    p
                }
            };
            (
                vec![("sha", sha256.clone())],
                pasta,
                format!("{sha256}.gguf"),
                Some(sha256.clone()),
            )
        }
    };
    // O nome vem do outro lado e vira arquivo aqui: a checagem tem que
    // acontecer com ele em mãos. Ficava no topo da função, quando `name` era
    // parâmetro; agora ele é calculado acima, conforme o alvo. Uma parte se
    // chama `<sha256>.gguf`, que passa pela mesma régua de qualquer outro nome.
    valid_transfer_filename(&name)?;

    // Opcionais de propósito: um peer que ainda não tenha esta versão ignora os
    // dois e a transferência funciona como antes, só sem visão do lado que envia.
    locator.push(("job", job_id.clone()));
    locator.push(("peer", state.network.node_id.clone()));

    // O diretório é a única coisa que precisa dar certo já: sem ele não há
    // transferência nenhuma para pôr na lista.
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("não deu para criar {}: {e}", dir.display()))?;

    // **O `.pom` já está no lugar, e é dele que sai o nome da tela.** Quem
    // chegou por oferta trouxe o manifesto junto e ele acabou de ser mesclado
    // na pasta; quem puxa por hash pediu o dele à origem logo acima - nos dois
    // casos antes de qualquer byte de peso. Ler daqui é o que troca o
    // `<sha256>.gguf` da notificação por "qwen3:8b · L0-15".
    let destino = dir.join(&name);
    let fatia = slice_label_for_path(&destino);

    // `bytes_total` 0 até o manifesto responder — a barra fica indeterminada, o
    // que é a verdade: ainda não se sabe o tamanho.
    state
        .transfers
        .start_job(
            &job_id,
            &name,
            TRANSFER_RECEIVING,
            &from_host,
            &destino.to_string_lossy(),
            0,
            fatia.as_ref(),
        )
        .await;
    set_detail(&state, &job_id, "conferindo o arquivo na origem").await;

    let task_state = state.clone();
    let task_job_id = job_id.clone();
    tokio::spawn(async move {
        let job_id = task_job_id;
        let dest = dir.join(&name);
        let part = dir.join(format!("{name}.part"));
        let expected_sha = expected_sha;
        let outcome = async {
            let (total, md5) = fetch_manifest(
                &task_state,
                &peer_id,
                &base_url,
                &key,
                &locator,
                &from_host,
                &name,
            )
            .await?;
            {
                let mut jobs = task_state.transfers.jobs.write().await;
                if let Some(j) = jobs.get_mut(&job_id) {
                    j.bytes_total = total;
                    j.detail = String::new();
                }
            }
            run_download(
                &task_state,
                &job_id,
                &peer_id,
                &base_url,
                &key,
                &locator,
                &part,
                &dest,
                total,
                &md5,
                expected_sha.as_deref(),
            )
            .await
        }
        .await;

        if let Err(e) = outcome {
            warn!("transferência de {name} falhou: {e}");
            // A partial file must never look like a usable slice.
            let _ = tokio::fs::remove_file(&part).await;
            // `finish` respeita o estado terminal que já estiver lá: `cancel`
            // rotulou este job com quem pediu, e sobrescrever trocaria uma
            // decisão de alguém por um "falhou" anônimo.
            task_state
                .transfers
                .finish(&job_id, TRANSFER_FAILED, e)
                .await;
        }
    });

    Ok(job_id)
}

/// Pede ao outro lado o manifesto do fatiamento a que uma parte pertence.
async fn fetch_pom(
    state: &GatewayState,
    peer_id: &str,
    base_url: &str,
    key: &str,
    sha256: &str,
) -> Option<crate::pom::PomManifest> {
    let resp = state
        .peer_http
        // Pinado no node_id do destino, como todo o resto: com o cluster em
        // TLS um cliente genérico falharia calado.
        .for_peer(peer_id)
        .ok()?
        .get(format!("{base_url}/api/transfer/pom?sha={sha256}"))
        .bearer_auth(key)
        .timeout(MANIFEST_TIMEOUT)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json().await.ok()
}

/// Etapa corrente de um job, para a tela ter o que dizer enquanto se espera.
async fn set_detail(state: &GatewayState, job_id: &str, detail: &str) {
    let mut jobs = state.transfers.jobs.write().await;
    if let Some(j) = jobs.get_mut(job_id) {
        j.detail = detail.to_string();
    }
}

/// Tamanho e digest do arquivo do outro lado — o que decide se vale começar.
async fn fetch_manifest(
    state: &GatewayState,
    peer_id: &str,
    base_url: &str,
    key: &str,
    locator: &[(&str, String)],
    from_host: &str,
    name: &str,
) -> Result<(u64, String), String> {
    let resp = state
        .peer_http
        .for_peer(peer_id)
        .map_err(|e| e.to_string())?
        .get(format!("{base_url}/api/transfer/manifest"))
        .bearer_auth(key)
        .query(locator)
        .timeout(MANIFEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("não deu para falar com {from_host}: {e}"))?;

    // Conferir o status ANTES de tentar ler JSON. Um erro do gateway costuma
    // vir em texto puro - o 403 da catraca de TLS é literalmente a frase "este
    // cluster exige TLS entre nós; refaça a chamada em https" -, e o `.json()`
    // em cima dele produzia "manifesto inválido: error decoding response body",
    // que não aponta para lugar nenhum. A explicação do outro lado é a melhor
    // que existe: repetir ela.
    let status = resp.status();
    if !status.is_success() {
        let corpo = resp.text().await.unwrap_or_default();
        let corpo = corpo.trim();
        return Err(if corpo.is_empty() {
            format!("{from_host} recusou o manifesto: HTTP {status}")
        } else {
            format!("{from_host} recusou o manifesto (HTTP {status}): {corpo}")
        });
    }

    let manifest: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("manifesto inválido de {from_host}: {e}"))?;

    let total = manifest["size_bytes"].as_u64().unwrap_or(0);
    let md5 = manifest["md5"].as_str().unwrap_or_default().to_string();
    if total == 0 || md5.is_empty() {
        return Err(format!("{from_host} não soube descrever {name}"));
    }

    // Refusing here beats filling the disk and failing halfway.
    let free_mb = sysmon_disk_free_mb().await;
    let needed_mb = total / 1_048_576 + DISK_HEADROOM_MB;
    if free_mb > 0 && free_mb < needed_mb {
        return Err(format!(
            "espaço insuficiente: {name} precisa de ~{needed_mb} MB (com folga) e só há {free_mb} MB livres"
        ));
    }

    Ok((total, md5))
}

#[allow(clippy::too_many_arguments)]
async fn run_download(
    state: &GatewayState,
    job_id: &str,
    peer_id: &str,
    base_url: &str,
    key: &str,
    locator: &[(&str, String)],
    part: &Path,
    dest: &Path,
    total: u64,
    expected_md5: &str,
    // `expected_sha256`, quando presente, é o que manda: o receptor confere o
    // hash que **pediu**, e não um digest que o remetente declarou. É a
    // diferença entre "o outro lado disse que está certo" e "eu conferi".
    expected_sha256: Option<&str>,
) -> Result<(), String> {
    use futures::StreamExt;

    // Resume whatever a previous attempt already wrote.
    let already = tokio::fs::metadata(part)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    let already = already.min(total);

    let mut req = state
        .peer_http
        .for_peer(peer_id)
        .map_err(|e| e.to_string())?
        .get(format!("{base_url}/api/transfer/download"))
        .bearer_auth(key)
        .query(locator)
        .timeout(Duration::from_secs(60 * 60 * 6));
    if already > 0 {
        info!("retomando {} a partir de {} bytes", dest.display(), already);
        req = req.header(header::RANGE, format!("bytes={already}-"));
    }

    let resp = req.send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        // Junto com o status vai a frase do outro lado: ver a nota gêmea em
        // `fetch_manifest`.
        let status = resp.status();
        let corpo = resp.text().await.unwrap_or_default();
        let corpo = corpo.trim();
        return Err(if corpo.is_empty() {
            format!("peer respondeu {status}")
        } else {
            format!("peer respondeu {status}: {corpo}")
        });
    }

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(part)
        .await
        .map_err(|e| e.to_string())?;
    file.seek(std::io::SeekFrom::Start(already))
        .await
        .map_err(|e| e.to_string())?;

    let mut done = already;
    let started = Instant::now();
    let mut last_report = Instant::now();
    let mut stream = resp.bytes_stream();

    while let Some(chunk) = stream.next().await {
        if state.transfers.is_cancelled(job_id).await {
            drop(file);
            // Cancelar é desistir: o parcial sai do disco. Guardá-lo ocuparia
            // gigabytes que a tela não mostra.
            let _ = tokio::fs::remove_file(part).await;
            return Err("cancelado".to_string());
        }
        let chunk = chunk.map_err(|e| e.to_string())?;
        file.write_all(&chunk).await.map_err(|e| e.to_string())?;
        done += chunk.len() as u64;

        if last_report.elapsed() >= Duration::from_millis(500) {
            last_report = Instant::now();
            state
                .transfers
                .progress(
                    job_id,
                    done,
                    done.saturating_sub(already),
                    started.elapsed(),
                )
                .await;
        }
    }
    file.flush().await.map_err(|e| e.to_string())?;
    drop(file);

    // Verify before the file gets its real name: a truncated or corrupted
    // slice that passes for a good one only shows up much later, as an
    // incomprehensible inference error.
    {
        let mut jobs = state.transfers.jobs.write().await;
        if let Some(j) = jobs.get_mut(job_id) {
            j.state = TRANSFER_VERIFYING.into();
            j.bytes_done = done;
        }
    }

    if let Some(esperado) = expected_sha256 {
        let p = part.to_path_buf();
        let esperado = esperado.to_string();
        let got = tokio::task::spawn_blocking(move || crate::pom::hash_file(&p))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?
            .0;
        if got != esperado {
            // Não é "falhou o download": é conteúdo que não é o que foi
            // pedido. O arquivo parcial some no tratamento de erro de
            // `spawn_download`, e nada com este nome chega ao disco bom.
            return Err(format!(
                "sha256 não confere (pedido {}, obtido {})",
                &esperado[..12],
                &got[..12]
            ));
        }
    } else {
        let p = part.to_path_buf();
        let got = tokio::task::spawn_blocking(move || -> Result<String, String> {
            use std::io::Read;
            let mut f = std::fs::File::open(&p).map_err(|e| e.to_string())?;
            let mut hasher = Md5::new();
            let mut buf = vec![0u8; CHUNK];
            loop {
                let n = f.read(&mut buf).map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
            Ok(hex::encode(hasher.finalize()))
        })
        .await
        .map_err(|e| e.to_string())??;

        if got != expected_md5 {
            return Err(format!(
                "MD5 não confere (esperado {expected_md5}, obtido {got})"
            ));
        }
    }

    // No Windows o rename não substitui: se o destino existe, ele falha com
    // "acesso negado" em vez de trocar o arquivo. Chegar aqui com o destino
    // ocupado significa que o arquivo foi rebaixado enquanto este download
    // corria, e o que acabou de passar pelo MD5 é o bom.
    #[cfg(windows)]
    if dest.exists() {
        let _ = tokio::fs::remove_file(dest).await;
    }

    tokio::fs::rename(part, dest)
        .await
        .map_err(|e| e.to_string())?;

    {
        let mut jobs = state.transfers.jobs.write().await;
        if let Some(j) = jobs.get_mut(job_id) {
            j.bytes_done = total;
            j.eta_secs = Some(0);
        }
    }
    state
        .transfers
        .finish(
            job_id,
            TRANSFER_DONE,
            format!("MD5 conferido · {}", dest.display()),
        )
        .await;
    info!("✅ fatia recebida e verificada: {}", dest.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// O aceite automático escreve gigabytes no disco de quem recebe sem
    /// perguntar. Ele só liga quando alguém escreveu um valor de verdade na
    /// variável desta máquina - variável ausente, vazia ou desligada continuam
    /// exigindo o clique, que é o padrão em produção.
    #[test]
    fn auto_accept_stays_shut_unless_explicitly_opened() {
        assert!(
            !auto_accept_set(None),
            "sem a variável, ninguém aceita nada"
        );
        assert!(!auto_accept_set(Some("")));
        assert!(!auto_accept_set(Some("  ")));
        assert!(!auto_accept_set(Some("0")));
        assert!(!auto_accept_set(Some("false")));
        assert!(!auto_accept_set(Some("FALSE")));

        assert!(auto_accept_set(Some("1")));
        assert!(auto_accept_set(Some("true")));
        assert!(auto_accept_set(Some(" 1 ")));
    }

    /// O remetente precisa aparecer na própria lista, com direção e o mesmo id
    /// que o receptor cunhou - é o id compartilhado que torna o cancelamento
    /// simétrico possível.
    #[tokio::test]
    async fn a_served_download_registers_a_sending_job() {
        let transfers = TransferState::default();

        transfers
            .start_job(
                "job-1",
                "fatia_0_15.gguf",
                TRANSFER_SENDING,
                "ksia",
                "/models/fatia_0_15.gguf",
                4_096,
                Some(&SliceLabel {
                    model_label: "qwen3:8b".into(),
                    start_layer: 0,
                    end_layer: 15,
                }),
            )
            .await;

        let jobs = transfers.jobs.read().await;
        let job = jobs.get("job-1").expect("job registrado");
        assert_eq!(job.direction, TRANSFER_SENDING);
        assert_eq!(job.peer_host, "ksia");
        assert_eq!(job.bytes_total, 4_096);
        assert_eq!(job.state, TRANSFER_RUNNING);
        assert_eq!(job.path, "/models/fatia_0_15.gguf");
        assert!(job.finished_at.is_none(), "ainda não terminou");
        // E a tela tem o que mostrar além do nome do arquivo.
        assert_eq!(job.model_label.as_deref(), Some("qwen3:8b"));
        assert_eq!((job.start_layer, job.end_layer), (Some(0), Some(15)));
    }

    /// O descritor de uma parte sai do `.pom`, e só de lá.
    ///
    /// O nome do arquivo é o sha256 e não afirma nada sobre camada nenhuma;
    /// deduzir a faixa dele foi um defeito real. Um hash que o manifesto não
    /// descreve não ganha rótulo inventado.
    #[test]
    fn a_slice_describes_itself_through_its_manifest() {
        use crate::pom::{PomKind, PomManifest, PomModel, PomPart};

        let sha = "a".repeat(64);
        let outro = "b".repeat(64);
        let m = PomManifest::new(
            PomModel {
                name: "qwen3-8b".into(),
                label: "qwen3:8b".into(),
                total_layers: 32,
                quant: Some("Q4_K_M".into()),
            },
            PomKind::Layers,
            "no-1",
            "a".repeat(64),
            vec![PomPart {
                sha256: sha.clone(),
                bytes: 10,
                start_layer: 16,
                end_layer: 31,
                is_first: false,
                is_last: true,
                tp: None,
            }],
        );

        let f = slice_label(&m, &sha).expect("o manifesto descreve esta parte");
        assert_eq!(f.model_label, "qwen3:8b");
        assert_eq!((f.start_layer, f.end_layer), (16, 31));

        assert!(
            slice_label(&m, &outro).is_none(),
            "parte que o manifesto não descreve não ganha rótulo inventado"
        );
    }

    /// Um job que terminou carrega **quando** terminou.
    ///
    /// É o que deixa o sino mostrar o desfecho: sem a marca ele só sabe listar
    /// o que está em andamento, e "concluída"/"recusada" sumia da tela no
    /// instante em que acontecia - o operador reencontrava o resultado só na
    /// tela de Transferências, que foi exatamente o relato.
    #[tokio::test]
    async fn a_finished_job_records_when_it_finished() {
        let transfers = TransferState::default();
        transfers
            .start_job(
                "job-1",
                "f.gguf",
                TRANSFER_RECEIVING,
                "ksia",
                "/m/f.gguf",
                10,
                None,
            )
            .await;

        transfers
            .finish("job-1", TRANSFER_DONE, "MD5 conferido")
            .await;

        let jobs = transfers.jobs.read().await;
        let job = jobs.get("job-1").unwrap();
        assert_eq!(job.state, TRANSFER_DONE);
        assert_eq!(job.detail, "MD5 conferido");
        assert!(
            job.finished_at.is_some(),
            "sem a hora o sino não sabe que isto acabou de acontecer"
        );
    }

    /// Estado terminal não é reescrito por outro.
    ///
    /// "Cancelado por ksia" foi decisão de alguém; o loop que percebe o stream
    /// morto depois não pode trocar isso por um "falhou" anônimo.
    #[tokio::test]
    async fn a_finished_job_is_never_relabelled() {
        let transfers = TransferState::default();
        transfers
            .start_job(
                "job-1",
                "f.gguf",
                TRANSFER_SENDING,
                "ksia",
                "/m/f.gguf",
                10,
                None,
            )
            .await;

        transfers.cancel("job-1", "atena").await;
        let quando = {
            let jobs = transfers.jobs.read().await;
            jobs.get("job-1").unwrap().finished_at.clone()
        };
        assert!(quando.is_some(), "cancelar também carimba a hora");

        transfers
            .finish("job-1", TRANSFER_FAILED, "conexão morreu")
            .await;
        transfers.finish("job-1", TRANSFER_DONE, "enviado").await;

        let jobs = transfers.jobs.read().await;
        let job = jobs.get("job-1").unwrap();
        assert_eq!(job.state, TRANSFER_CANCELLED, "a decisão de alguém venceu");
        assert!(job.detail.contains("atena"), "detail: {}", job.detail);
        assert_eq!(job.finished_at, quando, "e a hora é a do cancelamento");
    }

    /// Cancelar é idempotente e não depende de o job existir: o aviso do outro
    /// lado pode chegar antes de a transferência ter sido registrada aqui.
    #[tokio::test]
    async fn cancelling_marks_the_job_and_holds_for_a_job_not_yet_known() {
        let transfers = TransferState::default();
        transfers
            .start_job(
                "job-1",
                "f.gguf",
                TRANSFER_RECEIVING,
                "ksia",
                "/m/f.gguf",
                10,
                None,
            )
            .await;

        transfers.cancel("job-1", "macbook").await;
        assert!(transfers.is_cancelled("job-1").await);
        {
            let jobs = transfers.jobs.read().await;
            let job = jobs.get("job-1").unwrap();
            assert_eq!(job.state, TRANSFER_CANCELLED);
            assert!(job.detail.contains("macbook"), "detail: {}", job.detail);
        }

        // Duas vezes é igual a uma.
        transfers.cancel("job-1", "macbook").await;
        assert!(transfers.is_cancelled("job-1").await);

        // Um id que ainda não chegou aqui fica marcado assim mesmo, para o loop
        // que começar depois já nascer cancelado.
        transfers.cancel("job-desconhecido", "ksia").await;
        assert!(transfers.is_cancelled("job-desconhecido").await);
    }

    /// A regra é conjunção: assinatura no cabeçalho E extensão `.gguf`.
    #[test]
    fn only_real_gguf_files_pass_the_gate() {
        let dir = std::env::temp_dir().join(format!("pom-gguf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let ok = dir.join("fatia.gguf");
        std::fs::write(&ok, b"GGUF\x03\x00\x00\x00").unwrap();
        assert!(ensure_gguf(&ok).is_ok());

        // Blob sem extensão não é uma transferência permitida.
        let blob = dir.join("sha256-abc123");
        std::fs::write(&blob, b"GGUF\x03\x00\x00\x00").unwrap();
        assert!(ensure_gguf(&blob).is_err());

        // O nome mente sobre o conteúdo.
        let fake = dir.join("mentira.gguf");
        std::fs::write(&fake, b"NOTG____").unwrap();
        assert!(ensure_gguf(&fake).is_err());

        // Conteúdo certo, extensão errada: ainda é não.
        let wrong_ext = dir.join("modelo.txt");
        std::fs::write(&wrong_ext, b"GGUF\x03\x00\x00\x00").unwrap();
        assert!(ensure_gguf(&wrong_ext).is_err());

        // Maiúsculas não importam.
        let upper = dir.join("FATIA.GGUF");
        std::fs::write(&upper, b"GGUF\x03\x00\x00\x00").unwrap();
        assert!(ensure_gguf(&upper).is_ok());

        // Menor que a própria assinatura.
        let tiny = dir.join("curto.gguf");
        std::fs::write(&tiny, b"GG").unwrap();
        assert!(ensure_gguf(&tiny).is_err());

        assert!(ensure_gguf(&dir).is_err(), "diretório não é arquivo");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// O que o operador escolhe na própria tela ainda precisa ser um GGUF.
    #[test]
    fn the_operator_cannot_send_a_non_gguf() {
        let dir = std::env::temp_dir().join(format!("pom-xfer-nogguf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("anotacoes.txt");
        std::fs::write(&f, b"nada a ver").unwrap();

        assert!(local_send_path(f.to_str().unwrap()).is_err());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_peer_cannot_ask_for_files_outside_the_models_dir() {
        let dir = std::env::temp_dir().join(format!("pom-xfer-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/ok.gguf"), b"GGUF\x03\x00\x00\x00").unwrap();

        assert!(safe_model_path(&dir, "sub/ok.gguf").is_ok());
        assert!(safe_model_path(&dir, "../../../etc/passwd").is_err());
        assert!(safe_model_path(&dir, "/etc/passwd").is_err());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_absolute_path_inside_the_models_dir_is_allowed() {
        let dir = std::env::temp_dir().join(format!("pom-xfer-abs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("slice.gguf");
        std::fs::write(&f, b"GGUF\x03\x00\x00\x00").unwrap();

        let resolved = safe_model_path(&dir, f.to_str().unwrap()).unwrap();
        assert_eq!(resolved.file_name().unwrap(), "slice.gguf");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A path is never accepted from the transfer API, even when it points to
    /// a valid GGUF outside the configured catalog.
    #[test]
    fn a_path_cannot_be_used_as_a_transfer_filename() {
        let dir = std::env::temp_dir().join(format!("pom-xfer-any-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("solto.gguf");
        std::fs::write(&f, b"GGUF\x03\x00\x00\x00").unwrap();

        assert!(valid_transfer_filename(f.to_str().unwrap()).is_err());
        assert!(valid_transfer_filename("../solto.gguf").is_err());
        assert!(valid_transfer_filename("solto.txt").is_err());
        assert_eq!(valid_transfer_filename("solto.gguf").unwrap(), "solto.gguf");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Choosing a file is one thing; a directory is not a transfer.
    #[test]
    fn a_directory_is_not_sendable() {
        let dir = std::env::temp_dir().join(format!("pom-xfer-dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        assert!(local_send_path(dir.to_str().unwrap()).is_err());
        assert!(local_send_path("/nao/existe/em/lugar/nenhum.gguf").is_err());

        std::fs::remove_dir_all(&dir).ok();
    }
}

/// Quanto tempo a oferta leva para sair desta máquina.
///
/// O defeito que estes testes guardam: a oferta era postada **depois** do MD5
/// do arquivo inteiro, então o sino do outro lado só acendia quando a leitura
/// terminava. Medido no cartão SD do captain, `md5` de uma fatia de 4,2 GB
/// custa 3 min 23 s (~20 MB/s) - exatamente o "às vezes 1 ou 2 minutos" do
/// relato, com o cache de digest por (caminho, mtime, tamanho) mascarando o
/// atraso no segundo envio do mesmo arquivo.
#[cfg(test)]
mod offer_latency_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Um GGUF de verdade, grande o bastante para que lê-lo inteiro apareça no
    /// relógio se alguém voltar a ler.
    fn write_gguf(path: &Path, mib: usize) {
        let mut body = Vec::with_capacity(mib * 1024 * 1024);
        body.extend_from_slice(GGUF_MAGIC);
        body.resize(mib * 1024 * 1024, 0x5a);
        std::fs::write(path, body).unwrap();
    }

    /// O peer que recebe a oferta, reduzido ao que importa: ele anota o corpo
    /// que chegou e responde. É um servidor HTTP de verdade, na loopback.
    async fn fake_peer() -> (u16, Arc<RwLock<Vec<serde_json::Value>>>) {
        let seen: Arc<RwLock<Vec<serde_json::Value>>> = Arc::new(RwLock::new(Vec::new()));
        let captured = seen.clone();
        let app = Router::new().route(
            "/api/transfer/offer",
            post(move |Json(body): Json<serde_json::Value>| {
                let captured = captured.clone();
                async move {
                    captured.write().await.push(body);
                    Json(json!({ "accepted_for_review": true }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (port, seen)
    }

    /// Um nó remetente completo, com um peer já na tabela apontando para
    /// `peer_port`. Nada de UDP sai daqui: a tabela é semeada à mão.
    async fn sender_state(peer_port: u16) -> GatewayState {
        crate::gateway::testkit::state_with(
            "eu",
            vec![crate::gateway::testkit::peer("peer-b", "peer-b", peer_port)],
        )
        .await
    }

    /// A oferta é um convite, não a transferência: ela só precisa dizer o que é
    /// o arquivo e onde buscá-lo. O MD5 quem confere é o receptor, depois de
    /// Quem disca decide o esquema, não a oferta.
    ///
    /// A oferta é escrita pelo remetente e um nó de versão anterior grava
    /// `http://` fixo. Com o cluster em TLS isso rendia 403 em texto puro, e o
    /// `.json()` em cima dessa frase virava "manifesto inválido: error decoding
    /// response body" - erro que não aponta para nada (18/08).
    #[test]
    fn the_dialled_scheme_comes_from_local_policy_not_from_the_offer() {
        let _trava = crate::peer_client::POLITICA_GLOBAL.lock();

        crate::peer_client::set_tls_ativo(true);
        assert_eq!(
            normaliza_esquema("http://ksia:8080"),
            "https://ksia:8080",
            "oferta antiga em http tem que ser promovida"
        );
        assert_eq!(normaliza_esquema("https://ksia:8080"), "https://ksia:8080");
        assert_eq!(
            normaliza_esquema("http://[2804:d55::1]:8080"),
            "https://[2804:d55::1]:8080",
            "IPv6 entre colchetes sobrevive"
        );

        crate::peer_client::set_tls_ativo(false);
        assert_eq!(
            normaliza_esquema("https://ksia:8080"),
            "http://ksia:8080",
            "cluster em claro rebaixa a oferta"
        );

        crate::peer_client::set_tls_ativo(false);
    }

    /// aceitar, com o número que ele mesmo pede em `/api/transfer/manifest` -
    /// hashear aqui era trabalho que ninguém consumia e que segurava o sino do
    /// outro lado pelo tempo de ler o arquivo inteiro.
    #[tokio::test]
    async fn an_offer_leaves_without_reading_the_whole_file() {
        // Serializa com os testes que mexem na política de TLS, e fixa "em
        // claro": este teste disca um peer falso em HTTP puro. Sem isto, um
        // teste paralelo que liga a política fazia o cliente tentar TLS contra
        // o servidor falso, a oferta não era entregue, o `let _ =` engolia o
        // erro e a asserção estourava em `offers[0]` com a lista vazia - a
        // intermitência que perseguiu a suíte o dia 18/08 inteiro.
        let _trava = crate::peer_client::POLITICA_GLOBAL.lock();
        crate::peer_client::set_tls_ativo(false);

        let dir = std::env::temp_dir().join(format!("pom-offer-fast-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("fatia_0_15.gguf");
        write_gguf(&file, 24);

        let (port, seen) = fake_peer().await;
        let state = sender_state(port).await;
        state.auth.write().await.shard_dir = Some(dir.to_string_lossy().into());

        let started = Instant::now();
        let resp = send_to_peer(
            crate::auth::ConnectionAuth,
            State(state.clone()),
            Json(SendRequest {
                to_node_id: "peer-b".into(),
                filename: "fatia_0_15.gguf".into(),
            }),
        )
        .await;
        let elapsed = started.elapsed();
        assert_eq!(resp.status(), StatusCode::OK, "a oferta tem que ser aceita");

        let offers = seen.read().await;
        assert_eq!(offers.len(), 1, "o peer tem que ter recebido a oferta");
        let offer = &offers[0];
        assert_eq!(offer["name"], "fatia_0_15.gguf");
        assert_eq!(offer["size_bytes"], 24 * 1024 * 1024);
        // A faixa vinha do nome do arquivo. Não vem mais: `fatia_0_15.gguf`
        // deixou de ser uma afirmação sobre camada nenhuma, e adivinhar a
        // partir dele era justamente o defeito que o `.pom` fecha.
        assert!(offer["start_layer"].is_null());
        assert!(offer["end_layer"].is_null());

        // A prova de que nada foi lido: o cache de digest desta máquina segue
        // vazio. Ele é populado por `file_digest` e por mais nada, então uma
        // entrada aqui significa que alguém leu o arquivo inteiro antes de
        // bater na porta do vizinho.
        assert!(
            state.transfers.digests.read().await.is_empty(),
            "a oferta não pode hashear o arquivo: é o que fazia o sino do outro lado demorar"
        );

        // Cinturão e suspensórios: com o hash fora do caminho, o convite sai em
        // milissegundos mesmo aqui, onde o arquivo é pequeno.
        assert!(
            elapsed < Duration::from_secs(2),
            "oferta demorou {elapsed:?}; ela não pode depender do tamanho do arquivo"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// O tamanho anunciado tem que ser o real: ele é o que o outro lado mostra
    /// na notificação antes de decidir aceitar gigabytes.
    #[tokio::test]
    async fn the_offer_carries_the_real_size_without_a_digest() {
        // Serializa com os testes que mexem na política de TLS, e fixa "em
        // claro": este teste disca um peer falso em HTTP puro. Sem isto, um
        // teste paralelo que liga a política fazia o cliente tentar TLS contra
        // o servidor falso, a oferta não era entregue, o `let _ =` engolia o
        // erro e a asserção estourava em `offers[0]` com a lista vazia - a
        // intermitência que perseguiu a suíte o dia 18/08 inteiro.
        let _trava = crate::peer_client::POLITICA_GLOBAL.lock();
        crate::peer_client::set_tls_ativo(false);

        let dir = std::env::temp_dir().join(format!("pom-offer-size-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("solto.gguf");
        write_gguf(&file, 3);

        let (port, seen) = fake_peer().await;
        let state = sender_state(port).await;
        state.auth.write().await.shard_dir = Some(dir.to_string_lossy().into());

        let _ = send_to_peer(
            crate::auth::ConnectionAuth,
            State(state.clone()),
            Json(SendRequest {
                to_node_id: "peer-b".into(),
                filename: "solto.gguf".into(),
            }),
        )
        .await;

        let offers = seen.read().await;
        assert_eq!(offers[0]["size_bytes"], 3 * 1024 * 1024);
        // Compatibilidade: um peer antigo exige o campo, então ele continua
        // viajando - vazio, que é como se diz "ainda não sei".
        assert_eq!(offers[0]["md5"], "");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Aceitar não pode ficar preso ao outro lado. O manifesto é onde o MD5
    /// finalmente é calculado, e num volume lento isso são minutos: se o clique
    /// esperasse por ele, o operador teria trocado uma espera por outra.
    #[tokio::test]
    async fn accepting_returns_before_the_other_side_answers() {
        // Serializa com os testes que mexem na política de TLS, e fixa "em
        // claro": este teste disca um peer falso em HTTP puro. Sem isto, um
        // teste paralelo que liga a política fazia o cliente tentar TLS contra
        // o servidor falso, a oferta não era entregue, o `let _ =` engolia o
        // erro e a asserção estourava em `offers[0]` com a lista vazia - a
        // intermitência que perseguiu a suíte o dia 18/08 inteiro.
        let _trava = crate::peer_client::POLITICA_GLOBAL.lock();
        crate::peer_client::set_tls_ativo(false);

        let slow_hits = Arc::new(AtomicU64::new(0));
        let hits = slow_hits.clone();
        let app = Router::new().route(
            "/api/transfer/manifest",
            get(move || {
                let hits = hits.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    // O outro lado está lendo um arquivo de gigabytes.
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Json(json!({ "size_bytes": 1, "md5": "x" }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let state = sender_state(port).await;
        let dir = std::env::temp_dir().join(format!("pom-accept-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        state.auth.write().await.shard_dir = Some(dir.to_string_lossy().into());

        let started = Instant::now();
        let job_id = spawn_download(
            state.clone(),
            "peer-b".into(),
            format!("http://127.0.0.1:{port}"),
            Alvo::Avulso {
                remote_path: "/qualquer/fatia.gguf".into(),
                grant: None,
                name: "fatia.gguf".into(),
            },
            "peer-b".into(),
        )
        .await
        .expect("aceitar registra a transferência na hora");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "aceitar esperou o manifesto: {:?}",
            started.elapsed()
        );

        // E o trabalho existe de verdade: o job está na lista, com o outro lado
        // já sendo consultado em segundo plano.
        let jobs = state.transfers.jobs.read().await;
        assert!(jobs.contains_key(&job_id), "o job tem que aparecer na tela");
        drop(jobs);
        for _ in 0..50 {
            if slow_hits.load(Ordering::SeqCst) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(
            slow_hits.load(Ordering::SeqCst),
            1,
            "o manifesto tem que ter sido pedido em segundo plano"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod ipv6_tests {
    use super::*;

    /// An offer carries the address the receiver will fetch from. For an IPv6
    /// peer it used to carry `127.0.0.1`, pointing the receiver at itself.
    #[test]
    fn the_host_of_a_url_survives_ipv6_brackets() {
        assert_eq!(url_host("http://192.168.100.138:8090"), "192.168.100.138");
        assert_eq!(
            url_host("http://[2804:d55:5579:1000:89f3:3f94:7c07:e183]:8090"),
            "2804:d55:5579:1000:89f3:3f94:7c07:e183"
        );
        assert_eq!(url_host("http://[::1]:8090"), "::1");
    }

    /// Whatever the routing table answers has to be usable in a URL, which for
    /// IPv6 means brackets.
    #[test]
    fn an_ipv6_source_address_comes_back_bracketed() {
        let out = local_ip_for("http://[::1]:8090");
        assert!(
            out.starts_with('[') && out.ends_with(']'),
            "esperado bracketed, veio {out}"
        );
        assert!(
            format!("http://{out}:8090").parse::<reqwest::Url>().is_ok(),
            "o endereco tem que formar uma URL valida: {out}"
        );
    }
}

/// A busca de partes por conteúdo: um nó pede um **hash**, não um caminho.
///
/// É o que fecha o modelo torrent do lado da rede. Quem pede não sabe (nem
/// precisa saber) onde o outro lado guardou o arquivo, e o que chega ou tem o
/// hash pedido ou não entra no disco com nome de parte.
#[cfg(test)]
mod part_fetch_tests {
    use super::*;
    use crate::pom;

    /// Um nó de verdade servindo o roteador de transferências numa porta.
    async fn serving_node(shard_dir: &Path) -> (GatewayState, u16) {
        let state = crate::gateway::testkit::state_with("servidor", vec![]).await;
        state.auth.write().await.shard_dir = Some(shard_dir.to_string_lossy().into());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let porta = listener.local_addr().unwrap().port();
        let app = routes().with_state(state.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (state, porta)
    }

    /// **Uma parte não viaja sem o `.pom` dela.**
    ///
    /// Aqui a origem tem os bytes mas não tem o manifesto que os descreve. O
    /// pedido é recusado na hora, antes de mover coisa alguma: uma parte solta
    /// no destino é um arquivo que ninguém sabe de que modelo é nem que camadas
    /// traz, e a tela o mostraria como um modelo inteiro. Falhar dizendo o que
    /// falta é melhor que terminar bem e deixar lixo com cara de fatia.
    #[tokio::test]
    async fn a_part_without_its_manifest_is_refused() {
        let raiz =
            std::env::temp_dir().join(format!("pom-parte-{}", uuid::Uuid::new_v4().simple()));
        let disco_servidor = raiz.join("servidor");
        let disco_cliente = raiz.join("cliente");

        // Os bytes existem na origem, o manifesto não.
        let conteudo = crate::splitter::test_gguf_bytes(8);
        let (sha, _) = {
            let tmp = raiz.join("tmp.gguf");
            std::fs::create_dir_all(&raiz).unwrap();
            std::fs::write(&tmp, &conteudo).unwrap();
            pom::hash_file(&tmp).unwrap()
        };
        let pasta_servidor = disco_servidor.join("qwen3-8b-orfa");
        std::fs::create_dir_all(&pasta_servidor).unwrap();
        std::fs::write(pom::part_path(&pasta_servidor, &sha).unwrap(), &conteudo).unwrap();

        let (_servidor, porta) = serving_node(&disco_servidor).await;
        let cliente = crate::gateway::testkit::state_with(
            "cliente",
            vec![crate::gateway::testkit::peer("servidor", "servidor", porta)],
        )
        .await;
        cliente.auth.write().await.shard_dir = Some(disco_cliente.to_string_lossy().into());

        let resp = start_pull(
            ConnectionAuth,
            State(cliente.clone()),
            Json(PullRequest {
                sha256: Some(sha.clone()),
                model: Some("qwen3-8b".into()),
                node_id: "servidor".into(),
                remote_path: String::new(),
                name: String::new(),
            }),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "sem manifesto, o pull é recusado"
        );

        // E nada foi escrito: nem pasta de rodada inventada, nem quarentena.
        assert!(!disco_cliente.join("recebidos").exists());
        assert!(!disco_cliente.join("qwen3-8b").exists());

        std::fs::remove_dir_all(&raiz).ok();
    }

    /// Oferecer uma parte órfã também é recusado, e do lado de quem manda: a
    /// máquina que tem o arquivo sem o `.pom` é a que precisa consertar.
    #[tokio::test]
    async fn offering_an_orphan_part_is_refused_at_the_source() {
        let raiz = std::env::temp_dir().join(format!("pom-orf-{}", uuid::Uuid::new_v4().simple()));
        let pasta = raiz.join("qwen3-8b-orfa");
        std::fs::create_dir_all(&pasta).unwrap();

        let conteudo = crate::splitter::test_gguf_bytes(8);
        let tmp = raiz.join("tmp.gguf");
        std::fs::write(&tmp, &conteudo).unwrap();
        let (sha, _) = pom::hash_file(&tmp).unwrap();
        let caminho = pom::part_path(&pasta, &sha).unwrap();
        std::fs::rename(&tmp, &caminho).unwrap();

        let state = crate::gateway::testkit::state_with(
            "eu",
            vec![crate::gateway::testkit::peer("outro", "outro", 8080)],
        )
        .await;
        state.auth.write().await.shard_dir = Some(raiz.to_string_lossy().into());

        let resp = send_to_peer(
            ConnectionAuth,
            State(state),
            // O `main` passou a aceitar só o nome, resolvido contra o
            // catálogo desta máquina - caminho não vem mais do navegador.
            Json(SendRequest {
                to_node_id: "outro".into(),
                filename: caminho.file_name().unwrap().to_string_lossy().into_owned(),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        std::fs::remove_dir_all(&raiz).ok();
    }

    /// Pedir um hash que ninguém tem é recusado na hora, sem deixar arquivo.
    #[tokio::test]
    async fn a_part_nobody_has_fails_without_leaving_a_file() {
        let raiz =
            std::env::temp_dir().join(format!("pom-sem-parte-{}", uuid::Uuid::new_v4().simple()));
        let disco_servidor = raiz.join("servidor");
        let disco_cliente = raiz.join("cliente");
        std::fs::create_dir_all(&disco_servidor).unwrap();

        let (_servidor, porta) = serving_node(&disco_servidor).await;
        let cliente = crate::gateway::testkit::state_with(
            "cliente",
            vec![crate::gateway::testkit::peer("servidor", "servidor", porta)],
        )
        .await;
        cliente.auth.write().await.shard_dir = Some(disco_cliente.to_string_lossy().into());

        let ausente = "c".repeat(64);
        let resp = start_pull(
            ConnectionAuth,
            State(cliente.clone()),
            Json(PullRequest {
                sha256: Some(ausente.clone()),
                model: Some("qwen3-8b".into()),
                node_id: "servidor".into(),
                remote_path: String::new(),
                name: String::new(),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(!pom::part_path(&disco_cliente.join("recebidos"), &ausente)
            .unwrap()
            .exists());

        std::fs::remove_dir_all(&raiz).ok();
    }

    /// A regra vale nos dois sentidos: aceitar uma oferta de fatia sem `.pom`
    /// também é recusado, mesmo que o outro lado insista em mandá-la (um peer
    /// de versão anterior, por exemplo).
    #[tokio::test]
    async fn accepting_an_offer_of_a_part_without_a_manifest_is_refused() {
        let state = crate::gateway::testkit::state_with("eu", vec![]).await;
        let sha = "d".repeat(64);

        let offer = TransferOffer {
            id: "of-1".into(),
            from_node_id: "outro".into(),
            from_host: "outro".into(),
            from_gateway: "http://127.0.0.1:1".into(),
            remote_path: format!("/fatias/qwen/{sha}.gguf"),
            grant: Some("g".into()),
            name: format!("{sha}.gguf"),
            size_bytes: 4096,
            md5: String::new(),
            // O que falta.
            pom: None,
            start_layer: None,
            end_layer: None,
        };
        state.transfers.offers.write().await.push(offer);

        let resp = accept_offer(
            ConnectionAuth,
            State(state.clone()),
            AxumPath("of-1".to_string()),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // Um arquivo que **não** é parte continua passando: a exigência é sobre
        // fatia, não sobre tudo que chega.
        let inteiro = TransferOffer {
            id: "of-2".into(),
            from_node_id: "outro".into(),
            from_host: "outro".into(),
            from_gateway: "http://127.0.0.1:1".into(),
            remote_path: "/modelos/qwen3.5-9b.gguf".into(),
            grant: Some("g".into()),
            name: "qwen3.5-9b.gguf".into(),
            size_bytes: 4096,
            md5: String::new(),
            pom: None,
            start_layer: None,
            end_layer: None,
        };
        assert!(alvo_da_oferta(&state, &inteiro).await.is_ok());
    }

    /// Um `sha` da rede não pode virar caminho: `find_part` só aceita hash.
    #[test]
    fn a_sha_from_the_wire_cannot_escape_the_shard_dir() {
        let dir = std::env::temp_dir();
        assert!(find_part(&dir, "../../etc/passwd").is_none());
        assert!(find_part(&dir, "").is_none());
        assert!(find_part(&dir, &"z".repeat(64)).is_none());
    }
}

/// A fatia não viaja sozinha: o manifesto vai junto, e ela cai na pasta do
/// modelo — não solta na raiz, parecendo um modelo inteiro.
#[cfg(test)]
mod pom_travels_tests {
    use super::*;
    use crate::pom::{self, PomKind, PomManifest, PomModel, PomPart};

    /// Um nó com uma parte e o manifesto dela em disco, servindo o roteador.
    async fn no_com_parte(raiz: &Path) -> (GatewayState, u16, PomManifest, String) {
        let pasta = pom::scratch_dir(raiz);
        std::fs::create_dir_all(&pasta).unwrap();

        let bytes = crate::splitter::test_gguf_bytes(8);
        let tmp = pasta.join("tmp");
        std::fs::write(&tmp, &bytes).unwrap();
        let (sha, n) = pom::hash_file(&tmp).unwrap();
        std::fs::rename(&tmp, pom::part_path(&pasta, &sha).unwrap()).unwrap();

        let m = PomManifest::new(
            PomModel {
                name: "qwen3-8b".into(),
                label: "qwen3:8b".into(),
                total_layers: 8,
                quant: Some("Q4_K_M".into()),
            },
            PomKind::Layers,
            "origem",
            "a".repeat(64),
            vec![PomPart {
                sha256: sha.clone(),
                bytes: n,
                start_layer: 0,
                end_layer: 7,
                is_first: true,
                is_last: true,
                tp: None,
            }],
        );
        let pasta = pom::finalize_run(raiz, &pasta, &m).unwrap();
        let _ = &pasta;

        let state = crate::gateway::testkit::state_with("origem", vec![]).await;
        state.auth.write().await.shard_dir = Some(raiz.to_string_lossy().into());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let porta = listener.local_addr().unwrap().port();
        let app = routes().with_state(state.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (state, porta, m, sha)
    }

    /// Puxar por hash tem que trazer o `.pom` junto e gravar tudo na pasta do
    /// modelo. Era isto que faltava: a fatia chegava sozinha, na raiz da pasta
    /// de fatias, e o catálogo do destino a exibia como um modelo inteiro de
    /// nome ilegível.
    #[tokio::test]
    async fn pulling_a_part_brings_its_manifest_along() {
        let raiz = std::env::temp_dir().join(format!("pom-tr-{}", uuid::Uuid::new_v4().simple()));
        let disco_origem = raiz.join("origem");
        let disco_destino = raiz.join("destino");
        let (_origem, porta, manifesto, sha) = no_com_parte(&disco_origem).await;

        let cliente = crate::gateway::testkit::state_with(
            "destino",
            vec![crate::gateway::testkit::peer("origem", "origem", porta)],
        )
        .await;
        cliente.auth.write().await.shard_dir = Some(disco_destino.to_string_lossy().into());

        let resp = start_pull(
            ConnectionAuth,
            State(cliente.clone()),
            Json(PullRequest {
                sha256: Some(sha.clone()),
                model: Some("qwen3-8b".into()),
                node_id: "origem".into(),
                remote_path: String::new(),
                name: String::new(),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let pasta = pom::dir_for(&disco_destino, &manifesto);
        let destino = pom::part_path(&pasta, &sha).unwrap();
        for _ in 0..100 {
            if destino.is_file() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        assert!(destino.is_file(), "a parte cai na pasta do modelo");
        // E **nada** na raiz da pasta de fatias, que era onde ela ia parar.
        assert!(!disco_destino.join("recebidos").exists());
        assert!(!disco_destino.join(format!("{sha}.gguf")).exists());

        // O manifesto chegou junto, e é ele que dá nome à fatia no catálogo.
        let manifestos = pom::load_dir(&pasta);
        assert_eq!(manifestos.len(), 1, "o `.pom` veio com a parte");
        assert_eq!(manifestos[0].pom_id, manifesto.pom_id);

        let cat = crate::catalog::discover(&[], &disco_destino);
        assert_eq!(cat.models.len(), 1);
        assert!(
            cat.models[0].name.contains("qwen3:8b") && cat.models[0].name.contains("L0-7"),
            "a fatia se apresenta pelo modelo e pelas camadas: {}",
            cat.models[0].name
        );
        assert!(cat.models[0].is_shard(), "e como fatia, não modelo inteiro");

        std::fs::remove_dir_all(&raiz).ok();
    }

    /// Por que `recebidos/` ainda existe.
    ///
    /// Nem todo arquivo que chega é **parte** de um fatiamento: mandar um
    /// modelo inteiro de uma máquina para outra é um caminho legítimo da tela
    /// "Enviar um arquivo", e um GGUF inteiro não pertence a `.pom` nenhum. Ele
    /// não tem hash que o nomeie nem pasta de modelo onde morar, então cai em
    /// `recebidos/` - e o catálogo o mostra pelo nome dele, como modelo
    /// inteiro. O defeito antigo era a **fatia** cair aqui; a fatia agora vai
    /// para a pasta do modelo (ver `pulling_a_part_brings_its_manifest_along`).
    #[tokio::test]
    async fn a_whole_model_has_no_manifest_and_lands_in_recebidos() {
        let raiz = std::env::temp_dir().join(format!("pom-av-{}", uuid::Uuid::new_v4().simple()));
        let recebidos = raiz.join("recebidos");
        std::fs::create_dir_all(&recebidos).unwrap();
        // Um modelo inteiro que chegou por este caminho.
        std::fs::write(
            recebidos.join("qwen3.5-9b.gguf"),
            crate::splitter::test_gguf_bytes(8),
        )
        .unwrap();

        let cat = crate::catalog::discover(&[], &raiz);
        assert_eq!(cat.models.len(), 1);
        let m = &cat.models[0];
        assert_eq!(m.name, "qwen3.5-9b.gguf", "aparece pelo nome, não por hash");
        assert!(!m.is_shard(), "e como modelo inteiro, não como fatia");
        assert!(m.sha256.is_none());
        assert!(cat.shard_sets.is_empty(), "não forma conjunto de fatias");

        std::fs::remove_dir_all(&raiz).ok();
    }

    /// Quem oferece manda o manifesto na própria oferta, para o outro lado
    /// saber o que está aceitando antes de baixar nada.
    #[tokio::test]
    async fn an_offer_carries_the_manifest() {
        let raiz = std::env::temp_dir().join(format!("pom-of-{}", uuid::Uuid::new_v4().simple()));
        let disco = raiz.join("origem");
        let (state, _porta, manifesto, sha) = no_com_parte(&disco).await;

        let pasta = pom::dir_for(&disco, &manifesto);
        let caminho = pom::part_path(&pasta, &sha).unwrap();

        let achado = manifest_for(&state, &caminho)
            .await
            .expect("o manifesto da parte está ao lado dela");
        assert_eq!(achado.pom_id, manifesto.pom_id);

        // Um arquivo que não é parte de manifesto nenhum vai sozinho.
        let solto = raiz.join("solto.gguf");
        std::fs::write(&solto, crate::splitter::test_gguf_bytes(4)).unwrap();
        assert!(manifest_for(&state, &solto).await.is_none());

        std::fs::remove_dir_all(&raiz).ok();
    }

    #[tokio::test]
    async fn accepting_a_part_merges_with_the_local_manifest() {
        let raiz = std::env::temp_dir().join(format!(
            "pom-transfer-merge-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let source_sha256 = "a".repeat(64);
        let local_sha256 = "b".repeat(64);
        let remote_sha256 = "c".repeat(64);
        let model = PomModel {
            name: "original".into(),
            label: "original".into(),
            total_layers: 32,
            quant: None,
        };
        let local_part = PomPart {
            sha256: local_sha256.clone(),
            bytes: 5,
            start_layer: 0,
            end_layer: 14,
            is_first: true,
            is_last: false,
            tp: None,
        };
        let remote_part = PomPart {
            sha256: remote_sha256.clone(),
            bytes: 6,
            start_layer: 15,
            end_layer: 31,
            is_first: false,
            is_last: true,
            tp: None,
        };
        let local_manifest = PomManifest::new(
            model.clone(),
            PomKind::Layers,
            "local",
            source_sha256.clone(),
            vec![local_part],
        );
        let scratch = pom::scratch_dir(&raiz);
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::write(pom::part_path(&scratch, &local_sha256).unwrap(), b"local").unwrap();
        let model_dir = pom::finalize_run(&raiz, &scratch, &local_manifest).unwrap();

        let state = crate::gateway::testkit::state_with("destino", vec![]).await;
        state.auth.write().await.shard_dir = Some(raiz.to_string_lossy().into());
        let incoming = PomManifest::new(
            model,
            PomKind::Layers,
            "remote",
            source_sha256.clone(),
            vec![remote_part],
        );
        let offer = TransferOffer {
            id: "offer-merge".into(),
            from_node_id: "remoto".into(),
            from_host: "remoto".into(),
            from_gateway: "http://127.0.0.1:1".into(),
            remote_path: format!("/slices/{remote_sha256}.gguf"),
            grant: Some("grant".into()),
            name: format!("{remote_sha256}.gguf"),
            size_bytes: 6,
            md5: String::new(),
            pom: Some(incoming),
            start_layer: Some(15),
            end_layer: Some(31),
        };

        let alvo = alvo_da_oferta(&state, &offer).await.unwrap();
        let destino = match alvo {
            Alvo::Parte { model_dir, .. } => model_dir.unwrap(),
            Alvo::Avulso { .. } => panic!("a oferta com manifesto é uma parte"),
        };
        assert_eq!(destino, model_dir);
        let manifestos = pom::load_dir(&destino);
        assert_eq!(manifestos.len(), 1);
        assert_eq!(manifestos[0].pom_id, source_sha256);
        assert_eq!(manifestos[0].parts.len(), 2);
        assert!(manifestos[0].parts.iter().any(|p| p.sha256 == local_sha256));
        assert!(manifestos[0]
            .parts
            .iter()
            .any(|p| p.sha256 == remote_sha256));

        std::fs::remove_dir_all(&raiz).ok();
    }
}

/// **O veredito volta para quem ofereceu, e o envio sabe quando termina.**
///
/// Duas metades do mesmo silêncio relatado em campo. Quem mandava uma fatia
/// não recebia aviso nenhum: nem que o outro lado aceitou, nem que recusou, e
/// o job de envio ficava `running` para sempre porque nada o fechava quando o
/// corpo acabava de sair. Na prática o sino da máquina que enviou mostrava uma
/// transferência eterna a 0 B, e a recusa não produzia sinal algum.
#[cfg(test)]
mod verdict_tests {
    use super::*;
    use crate::pom::{self, PomKind, PomManifest, PomModel, PomPart};

    /// Um nó servindo o roteador de transferências numa porta efêmera.
    ///
    /// `gateway_port` é ajustado para a porta real: é ele que entra no
    /// `from_gateway` da oferta, e é por esse endereço que o veredito volta.
    /// Com o 8080 do testkit os dois lados discariam uma porta onde não há
    /// ninguém.
    async fn no_servindo(state: GatewayState, listener: tokio::net::TcpListener) -> GatewayState {
        let app = routes().with_state(state.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        state
    }

    /// Um nó com uma parte em disco, servindo o roteador de transferências.
    async fn no_com_parte(node_id: &str, raiz: &Path) -> (GatewayState, u16, PomManifest, String) {
        let pasta = pom::scratch_dir(raiz);
        std::fs::create_dir_all(&pasta).unwrap();

        let bytes = crate::splitter::test_gguf_bytes(8);
        let tmp = pasta.join("tmp");
        std::fs::write(&tmp, &bytes).unwrap();
        let (sha, n) = pom::hash_file(&tmp).unwrap();
        std::fs::rename(&tmp, pom::part_path(&pasta, &sha).unwrap()).unwrap();

        let m = PomManifest::new(
            PomModel {
                name: "qwen3-8b".into(),
                label: "qwen3:8b".into(),
                total_layers: 8,
                quant: Some("Q4_K_M".into()),
            },
            PomKind::Layers,
            node_id,
            "a".repeat(64),
            vec![PomPart {
                sha256: sha.clone(),
                bytes: n,
                start_layer: 0,
                end_layer: 7,
                is_first: true,
                is_last: true,
                tp: None,
            }],
        );
        pom::finalize_run(raiz, &pasta, &m).unwrap();

        let mut state = crate::gateway::testkit::state_with(node_id, vec![]).await;
        state.auth.write().await.shard_dir = Some(raiz.to_string_lossy().into());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let porta = listener.local_addr().unwrap().port();
        state.gateway_port = porta;
        let state = no_servindo(state, listener).await;
        (state, porta, m, sha)
    }

    /// O outro lado: um nó vazio que já conhece `peer`, servindo o roteador.
    async fn no_vazio(
        node_id: &str,
        raiz: &Path,
        peer: crate::network::PeerInfo,
    ) -> (GatewayState, u16) {
        let mut state = crate::gateway::testkit::state_with(node_id, vec![peer]).await;
        state.auth.write().await.shard_dir = Some(raiz.to_string_lossy().into());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let porta = listener.local_addr().unwrap().port();
        state.gateway_port = porta;
        (no_servindo(state, listener).await, porta)
    }

    /// Espera o job chegar a um estado terminal, ou desiste.
    async fn ate_terminar(state: &GatewayState, job_id: &str) -> TransferJob {
        for _ in 0..200 {
            if let Some(j) = state.transfers.jobs.read().await.get(job_id) {
                if is_finished(&j.state) {
                    return j.clone();
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("o job {job_id} nunca terminou");
    }

    /// Aceitar avisa quem ofereceu, e o envio fecha sozinho quando o último
    /// byte sai.
    #[tokio::test]
    async fn accepting_tells_the_sender_and_the_send_completes() {
        let _trava = crate::peer_client::POLITICA_GLOBAL.lock();
        crate::peer_client::set_tls_ativo(false);

        let raiz =
            std::env::temp_dir().join(format!("pom-vd-ok-{}", uuid::Uuid::new_v4().simple()));
        let disco_a = raiz.join("atena");
        let disco_b = raiz.join("ksia");
        let (atena, porta_a, _m, sha) = no_com_parte("atena", &disco_a).await;

        // O receptor precisa alcançar o remetente pelo roteador de verdade: é
        // por ele que o veredito volta.
        let (ksia, porta_b) = no_vazio(
            "ksia",
            &disco_b,
            crate::gateway::testkit::peer("atena", "atena", porta_a),
        )
        .await;
        // E o remetente precisa conhecer o destino, para nomeá-lo na tela.
        atena.network.peers.write().await.insert(
            "ksia".into(),
            crate::gateway::testkit::peer("ksia", "ksia", porta_b),
        );

        let resp = send_to_peer(
            ConnectionAuth,
            State(atena.clone()),
            Json(SendRequest {
                to_node_id: "ksia".into(),
                filename: format!("{sha}.gguf"),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK, "a oferta tem que sair");

        // O remetente guardou a oferta para saber nomear o veredito.
        assert_eq!(atena.transfers.offered.read().await.len(), 1);

        let oferta_id = {
            let offers = ksia.transfers.offers.read().await;
            assert_eq!(offers.len(), 1, "a oferta chegou no destino");
            offers[0].id.clone()
        };

        let resp = accept_offer(ConnectionAuth, State(ksia.clone()), AxumPath(oferta_id)).await;
        assert_eq!(resp.status(), StatusCode::OK);

        // O download termina, e o **envio** também: era este segundo fecho que
        // não existia, e por isso o sino de quem mandou nunca esvaziava.
        let recebido = {
            let id = ksia
                .transfers
                .jobs
                .read()
                .await
                .keys()
                .next()
                .cloned()
                .expect("o receptor registrou o job");
            ate_terminar(&ksia, &id).await
        };
        assert_eq!(recebido.state, TRANSFER_DONE);
        assert_eq!(recebido.direction, TRANSFER_RECEIVING);

        let enviado = ate_terminar(&atena, &recebido.id).await;
        assert_eq!(
            enviado.direction, TRANSFER_SENDING,
            "o mesmo id existe nos dois lados, com direções opostas"
        );
        assert_eq!(
            enviado.state, TRANSFER_DONE,
            "o envio ficava `running` para sempre: nada o fechava"
        );
        assert_eq!(enviado.peer_host, "ksia", "e a tela diz para quem foi");
        assert!(
            enviado.finished_at.is_some(),
            "sem a hora o sino não mostra o desfecho"
        );
        assert_eq!(
            enviado.bytes_done, enviado.bytes_total,
            "e o número final é o arquivo inteiro, não o último relato de 500 ms"
        );

        // Respondida, a oferta sai do registro do remetente.
        assert!(atena.transfers.offered.read().await.is_empty());

        // **E os dois lados sabem dizer o que era, sem falar em hash.** O nome
        // do arquivo é o sha256; quem diz de que modelo é e que camadas traz é
        // o `.pom`, e ele viaja com a oferta e é gravado antes do primeiro byte.
        for (quem, job) in [("recebido", &recebido), ("enviado", &enviado)] {
            assert_eq!(
                job.model_label.as_deref(),
                Some("qwen3:8b"),
                "{quem}: a tela precisa do modelo, não do hash"
            );
            assert_eq!(
                (job.start_layer, job.end_layer),
                (Some(0), Some(7)),
                "{quem}: e da faixa de camadas"
            );
            // O hash continua disponível, como identificação secundária: é ele
            // que nomeia o arquivo em disco.
            assert_eq!(job.name, format!("{sha}.gguf"), "{quem}");
        }

        std::fs::remove_dir_all(&raiz).ok();
    }

    /// Recusar também avisa - e era o caso que não produzia sinal nenhum.
    #[tokio::test]
    async fn rejecting_tells_the_sender_who_refused() {
        let _trava = crate::peer_client::POLITICA_GLOBAL.lock();
        crate::peer_client::set_tls_ativo(false);

        let raiz =
            std::env::temp_dir().join(format!("pom-vd-nao-{}", uuid::Uuid::new_v4().simple()));
        let disco_a = raiz.join("atena");
        let disco_b = raiz.join("ksia");
        let (atena, porta_a, _m, sha) = no_com_parte("atena", &disco_a).await;

        // O receptor precisa alcançar o remetente pelo roteador de verdade: é
        // por ele que o veredito volta.
        let (ksia, porta_b) = no_vazio(
            "ksia",
            &disco_b,
            crate::gateway::testkit::peer("atena", "atena", porta_a),
        )
        .await;
        atena.network.peers.write().await.insert(
            "ksia".into(),
            crate::gateway::testkit::peer("ksia", "ksia", porta_b),
        );

        let _ = send_to_peer(
            ConnectionAuth,
            State(atena.clone()),
            Json(SendRequest {
                to_node_id: "ksia".into(),
                filename: format!("{sha}.gguf"),
            }),
        )
        .await;

        let oferta_id = ksia.transfers.offers.read().await[0].id.clone();
        let resp = reject_offer(ConnectionAuth, State(ksia.clone()), AxumPath(oferta_id)).await;
        assert_eq!(resp.status(), StatusCode::OK);

        // O remetente fica sabendo. Antes disto a recusa era um silêncio: nem
        // job, nem linha de log, nem nada na tela de quem tinha oferecido.
        let mut recusa = None;
        for _ in 0..200 {
            let jobs = atena.transfers.jobs.read().await;
            if let Some(j) = jobs.values().next() {
                recusa = Some(j.clone());
                break;
            }
            drop(jobs);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let recusa = recusa.expect("a recusa tem que virar aviso no remetente");
        assert_eq!(recusa.direction, TRANSFER_SENDING);
        assert_eq!(
            recusa.state, TRANSFER_CANCELLED,
            "recusa é decisão de alguém, não falha"
        );
        assert!(
            recusa.detail.contains("ksia"),
            "a tela precisa dizer quem recusou: {}",
            recusa.detail
        );
        assert!(recusa.finished_at.is_some());
        // O aviso de recusa também fala em modelo, não em hash.
        assert_eq!(recusa.model_label.as_deref(), Some("qwen3:8b"));
        assert_eq!((recusa.start_layer, recusa.end_layer), (Some(0), Some(7)));

        std::fs::remove_dir_all(&raiz).ok();
    }

    /// Um veredito sobre uma concessão que esta máquina não ofereceu não
    /// descreve nada que ela possa mostrar.
    #[tokio::test]
    async fn an_answer_for_an_unknown_grant_is_refused() {
        let state = crate::gateway::testkit::state_with("atena", vec![]).await;

        let resp = receive_answer(
            ConnectionAuth,
            State(state.clone()),
            Json(AnswerRequest {
                grant: "nao-existe".into(),
                from_node_id: "ksia".into(),
                accepted: true,
                job_id: None,
                reason: None,
            }),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(state.transfers.jobs.read().await.is_empty());
    }

    /// E um veredito que não vem de quem recebeu a oferta é outra conversa.
    #[tokio::test]
    async fn an_answer_from_the_wrong_node_is_refused() {
        let state = crate::gateway::testkit::state_with("atena", vec![]).await;
        state.transfers.offered.write().await.insert(
            "g-1".into(),
            OfferedFile {
                to_node_id: "ksia".into(),
                to_host: "ksia".into(),
                name: "f.gguf".into(),
                path: "/m/f.gguf".into(),
                size_bytes: 10,
                fatia: None,
            },
        );

        let resp = receive_answer(
            ConnectionAuth,
            State(state.clone()),
            Json(AnswerRequest {
                grant: "g-1".into(),
                from_node_id: "intruso".into(),
                accepted: false,
                job_id: None,
                reason: None,
            }),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert!(state.transfers.jobs.read().await.is_empty());
        assert_eq!(
            state.transfers.offered.read().await.len(),
            1,
            "e a oferta continua de pé, esperando quem de direito"
        );
    }
}

#[cfg(test)]
mod offer_etag_tests {
    use super::*;
    use crate::gateway::testkit::{body_of, state_with};
    use axum::body::to_bytes;
    use axum::http::header::{ETAG, IF_NONE_MATCH};

    fn offer(id: &str) -> TransferOffer {
        TransferOffer {
            id: id.into(),
            from_node_id: "ksia-id".into(),
            from_host: "ksia".into(),
            from_gateway: "http://ksia:8080".into(),
            remote_path: "/models/f.gguf".into(),
            grant: Some("grant".into()),
            name: "f.gguf".into(),
            size_bytes: 10,
            md5: String::new(),
            pom: None,
            start_layer: Some(0),
            end_layer: Some(1),
        }
    }

    #[tokio::test]
    async fn unchanged_offers_return_304_and_a_new_offer_changes_the_etag() {
        let state = state_with("eu", vec![]).await;
        state.transfers.offers.write().await.push(offer("offer-1"));

        let first = list_offers(ConnectionAuth, HeaderMap::new(), State(state.clone())).await;
        assert_eq!(first.status(), StatusCode::OK);
        let etag = first.headers().get(ETAG).cloned().expect("ETag da oferta");
        assert!(body_of(first).await.get("disk_free_mb").is_none());

        let mut headers = HeaderMap::new();
        headers.insert(IF_NONE_MATCH, etag.clone());
        let second = list_offers(ConnectionAuth, headers, State(state.clone())).await;
        assert_eq!(second.status(), StatusCode::NOT_MODIFIED);
        assert!(to_bytes(second.into_body(), usize::MAX)
            .await
            .unwrap()
            .is_empty());

        state.transfers.offers.write().await.push(offer("offer-2"));
        let third = list_offers(ConnectionAuth, HeaderMap::new(), State(state)).await;
        assert_eq!(third.status(), StatusCode::OK);
        assert_ne!(third.headers().get(ETAG), Some(&etag));
    }
}
