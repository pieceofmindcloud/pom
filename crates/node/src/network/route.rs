//! Escolha do endereco por onde falar com cada peer.
//!
//! Um no tem mais de um endereco valido, e qual deles serve depende de quem
//! pergunta. Este modulo guarda a medicao de cada candidato e decide qual usar.
//! A decisao mora aqui, sozinha e pura, porque errar nela nao da erro: o no
//! sobe, aparece no cluster, e a requisicao morre calada no primeiro salto.

use serde::{Deserialize, Serialize};

/// Quanto o candidato novo precisa ser melhor para valer a troca.
///
/// As duas condicoes valem juntas. A relativa sozinha trocaria de rota entre
/// 0,8 e 1,2 ms; a absoluta sozinha ignoraria uma piora proporcionalmente
/// enorme num enlace ja lento. Os 25% saem da medicao de 2026-08-13 entre
/// MacBook e ksia (9,6 contra 7,2 ms de p50), que precisa NAO trocar.
const MARGEM_RELATIVA: f64 = 0.25;
const MARGEM_ABSOLUTA_MS: f64 = 2.0;

#[derive(Debug, Clone, PartialEq)]
pub struct Measurement {
    pub addr: String,
    /// `None` e "nao respondeu", que e diferente de "respondeu devagar".
    pub rtt_ms: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RouteChoice {
    pub addr: String,
    pub rtt_ms: f64,
}

/// Escolhe por onde falar, dado o que foi medido e por onde se fala hoje.
///
/// Devolve `None` quando nenhum candidato respondeu - o chamador trata isso
/// como peer inalcancavel, nao como "use o primeiro da lista".
pub fn choose_route(medidas: &[Measurement], atual: Option<&str>) -> Option<RouteChoice> {
    let mut vivos: Vec<(&str, f64)> = medidas
        .iter()
        .filter_map(|m| m.rtt_ms.map(|rtt| (m.addr.as_str(), rtt)))
        .collect();
    if vivos.is_empty() {
        return None;
    }
    // Ordem estavel antes de comparar: sem isto, dois candidatos empatados
    // saem em ordem de iteracao e dois nos podem escolher caminhos diferentes.
    vivos.sort_by(|a, b| {
        a.1.partial_cmp(&b.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(b.0))
    });
    let (melhor_addr, melhor_rtt) = vivos[0];

    // A rota atual so e mantida se ainda responde.
    if let Some(atual_addr) = atual {
        if let Some(&(_, atual_rtt)) = vivos.iter().find(|(a, _)| *a == atual_addr) {
            let ganho_relativo = (atual_rtt - melhor_rtt) / atual_rtt;
            let ganho_absoluto = atual_rtt - melhor_rtt;
            let vale_trocar =
                ganho_relativo >= MARGEM_RELATIVA && ganho_absoluto >= MARGEM_ABSOLUTA_MS;
            if !vale_trocar {
                return Some(RouteChoice {
                    addr: atual_addr.to_string(),
                    rtt_ms: atual_rtt,
                });
            }
        }
    }
    Some(RouteChoice {
        addr: melhor_addr.to_string(),
        rtt_ms: melhor_rtt,
    })
}

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

/// De quanto em quanto tempo os candidatos sao remedidos.
pub const INTERVALO_MEDICAO: Duration = Duration::from_secs(300);
/// Uma sonda que passa disso conta como falha.
#[cfg(not(test))]
const TIMEOUT_SONDA: Duration = Duration::from_secs(3);

#[cfg(test)]
const TIMEOUT_SONDA: Duration = Duration::from_millis(50);

struct Entrada {
    escolha: RouteChoice,
    medido_em: Instant,
}

/// Falha de transporte recente. A entrada permanece até uma nova sonda bem
/// sucedida ou até expirar, para que uma sessão seguinte não volte a pagar o
/// timeout do mesmo endereço.
const FAILURE_COOLDOWN: Duration = Duration::from_secs(300);

/// Por onde este no fala com cada peer.
///
/// Vive so na memoria deste processo e nao e publicada no gossip: publicar
/// tabela de medicao transformaria enderecamento em estado distribuido, e o
/// `PeerList` ja esta no limite da MTU.
#[derive(Default)]
pub struct RouteTable {
    por_no: RwLock<HashMap<String, Entrada>>,
    /// Memória de falha por `(node_id, URL)`.
    ///
    /// A chave é a URL inteira, então a porta de inferência e a porta do
    /// gateway do mesmo host convivem sem se confundir - e é de propósito que
    /// as duas moram aqui. O plano de controle (o Painel lendo
    /// `/api/ui/node`) sofre do mesmo defeito que o caminho de inferência
    /// sofria: sem memória, um endereço morto é rediscado em toda leitura.
    /// Uma segunda tabela seria um mecanismo paralelo com a mesma decisão
    /// para tomar em dois lugares.
    falhas: RwLock<HashMap<(String, String), Instant>>,
}

impl RouteTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Endereco escolhido para este peer, se ja medido e ainda valido.
    pub fn resolve(&self, node_id: &str) -> Option<String> {
        self.por_no
            .read()
            .ok()?
            .get(node_id)
            .filter(|entry| entry.medido_em.elapsed() < INTERVALO_MEDICAO)
            .map(|e| e.escolha.addr.clone())
    }

    /// Rota e latencia de cada peer, para a interface mostrar.
    pub fn snapshot(&self) -> Vec<(String, RouteChoice)> {
        self.por_no
            .read()
            .map(|t| {
                t.iter()
                    .map(|(k, v)| (k.clone(), v.escolha.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Tira uma rota de uso na hora, sem esperar o ciclo.
    ///
    /// Chamado quando uma requisicao real falhou naquele endereco: esperar 5
    /// minutos significaria falhar tudo que chegasse nesse meio tempo.
    pub fn mark_failure(&self, node_id: &str, addr: &str) {
        if let Ok(mut t) = self.por_no.write() {
            if t.get(node_id)
                .map(|e| e.escolha.addr == addr)
                .unwrap_or(false)
            {
                t.remove(node_id);
            }
        }
        if let Ok(mut falhas) = self.falhas.write() {
            falhas.insert((node_id.to_string(), addr.to_string()), Instant::now());
        }
    }

    /// Informa se uma requisição real já falhou recentemente neste endereço.
    pub fn is_failed(&self, node_id: &str, addr: &str) -> bool {
        let Ok(mut falhas) = self.falhas.write() else {
            return false;
        };
        let chave = (node_id.to_string(), addr.to_string());
        let Some(falhou_em) = falhas.get(&chave).copied() else {
            return false;
        };
        if falhou_em.elapsed() >= FAILURE_COOLDOWN {
            falhas.remove(&chave);
            false
        } else {
            true
        }
    }

    /// Descarta a escolha quando o peer anuncia outra topologia de endereços.
    ///
    /// Uma rota medida para um endereço antigo não pode sobreviver a uma
    /// atualização de `addr_candidates`: durante a janela de 5 minutos da
    /// medição ela faria o pipeline continuar discando um IP de container que
    /// já deixou de ser anunciado.
    ///
    /// **A memória de falha não é apagada aqui.** Ela é chaveada pelo
    /// endereço, então um endereço novo simplesmente não está no mapa, e um
    /// endereço que voltou a responder é limpo por `mark_success` ou pela
    /// sonda. Apagar por `node_id` era pior que inútil: um IPv6 temporário
    /// rodando muda `addr_candidates`, e a rotação - que é exatamente o
    /// evento que mata o endereço antigo - era também o evento que apagava a
    /// prova de que ele morreu. O cluster voltava a discar o cadáver.
    pub fn invalidate(&self, node_id: &str) {
        if let Ok(mut t) = self.por_no.write() {
            t.remove(node_id);
        }
    }

    /// Esquece tudo que se sabe deste peer. Só para quando o peer some.
    pub fn forget(&self, node_id: &str) {
        self.invalidate(node_id);
        if let Ok(mut falhas) = self.falhas.write() {
            falhas.retain(|(id, _), _| id != node_id);
        }
    }

    /// Uma requisição real que respondeu apaga a memória de falha do endereço.
    ///
    /// É o contrário de `mark_failure`, e o que permite um endereço voltar ao
    /// uso antes do fim do `FAILURE_COOLDOWN` quando ele volta a atender.
    pub fn mark_success(&self, node_id: &str, addr: &str) {
        if let Ok(mut falhas) = self.falhas.write() {
            falhas.remove(&(node_id.to_string(), addr.to_string()));
        }
    }

    /// Candidatos que ainda não foram desacreditados, na ordem recebida.
    ///
    /// Devolver vazio é informação, não erro: significa "todo endereço que
    /// este nó conhece deste peer falhou recentemente". O chamador não deve
    /// insistir em nenhum deles - deve pedir o endereço atual do par (ver
    /// `Network::request_peer_addresses`) e dizer na tela que está
    /// reencontrando o nó.
    pub fn usable(&self, node_id: &str, candidatos: &[String]) -> Vec<String> {
        candidatos
            .iter()
            .filter(|url| !self.is_failed(node_id, url))
            .cloned()
            .collect()
    }

    /// Se a rota para este endpoint exato falhou recentemente.
    ///
    /// Uma falha HTTP na porta do gateway ou da inferência não prova que o
    /// endpoint P2P/UDP do mesmo host está morto. Comparar somente o host fazia
    /// o gossip e a redescoberta apagar endereços de transporte que ainda
    /// serviam o prefill.
    pub fn endpoint_is_discredited(&self, node_id: &str, addr: &str) -> bool {
        let Some(alvo) = host_and_port_of(addr) else {
            return false;
        };
        let Ok(falhas) = self.falhas.read() else {
            return false;
        };
        falhas.iter().any(|((id, url), falhou_em)| {
            id == node_id
                && host_and_port_of(url).is_some_and(|endpoint| endpoint == alvo)
                && falhou_em.elapsed() < FAILURE_COOLDOWN
        })
    }

    /// Grava uma escolha direto, sem sondar. Só para teste.
    #[cfg(test)]
    pub fn remember_for_test(&self, node_id: &str, addr: &str, rtt_ms: f64) {
        if let Ok(mut t) = self.por_no.write() {
            t.insert(
                node_id.to_string(),
                Entrada {
                    escolha: RouteChoice {
                        addr: addr.to_string(),
                        rtt_ms,
                    },
                    medido_em: Instant::now(),
                },
            );
        }
        self.mark_success(node_id, addr);
    }

    #[cfg(test)]
    pub fn remember_for_test_with_age(
        &self,
        node_id: &str,
        addr: &str,
        rtt_ms: f64,
        age: Duration,
    ) {
        if let Ok(mut table) = self.por_no.write() {
            table.insert(
                node_id.to_string(),
                Entrada {
                    escolha: RouteChoice {
                        addr: addr.to_string(),
                        rtt_ms,
                    },
                    medido_em: Instant::now() - age,
                },
            );
        }
        self.mark_success(node_id, addr);
    }

    /// Se ja passou tempo suficiente desde a ultima medicao deste peer.
    pub fn precisa_medir(&self, node_id: &str) -> bool {
        self.por_no
            .read()
            .map(|t| match t.get(node_id) {
                Some(e) => e.medido_em.elapsed() >= INTERVALO_MEDICAO,
                None => true,
            })
            .unwrap_or(true)
    }

    /// Mede todos os candidatos e grava a escolha.
    ///
    /// A sonda e `GET /health` por HTTP(S), nao ICMP: mede exatamente a porta
    /// de inferência que o pipeline vai usar, com firewall e proxy no meio.
    /// Sondar o gateway seria insuficiente: ele pode responder enquanto a
    /// porta de inferência engole SYN.
    ///
    /// O cliente da sonda aceita o certificado autoassinado do nó. Isso só vale
    /// para medir latência: os clientes que transportam inferência continuam
    /// usando pinning por `node_id`.
    pub async fn measure_peer(
        &self,
        clientes: &crate::peer_client::PeerClients,
        node_id: &str,
        candidatos: &[String],
    ) {
        let Ok(http) = clientes.probe() else {
            return;
        };
        let mut medidas = Vec::with_capacity(candidatos.len());
        for rota in candidatos {
            let inicio = Instant::now();
            let resposta =
                tokio::time::timeout(TIMEOUT_SONDA, http.get(format!("{rota}/health")).send())
                    .await;
            let rtt = match resposta {
                Ok(Ok(r)) if r.status().is_success() => {
                    self.mark_success(node_id, rota);
                    Some(inicio.elapsed().as_secs_f64() * 1000.0)
                }
                // Uma sonda lenta **não** desacredita o endereço: o timeout
                // dela é 3 s, muito mais apertado que o de uma requisição
                // real, e `resolve_step_url` deixa de ter rota quando todos os
                // candidatos estão desacreditados. Só falha de requisição de
                // verdade escreve na memória - foi assim que 84e7141 a
                // desenhou, e é assim que ela fica.
                _ => None,
            };
            medidas.push(Measurement {
                addr: rota.clone(),
                rtt_ms: rtt,
            });
        }
        let atual = self.resolve(node_id);
        if let Some(escolha) = choose_route(&medidas, atual.as_deref()) {
            if let Ok(mut t) = self.por_no.write() {
                t.insert(
                    node_id.to_string(),
                    Entrada {
                        escolha,
                        medido_em: Instant::now(),
                    },
                );
            }
        } else if let Ok(mut t) = self.por_no.write() {
            t.remove(node_id);
        }
    }

    /// Remede a rota atualmente escolhida sem reordenar os candidatos.
    ///
    /// A medição de endereçamento é deliberadamente mais lenta que a medição
    /// de RTT usada pelo painel. Reusar `RouteChoice.rtt_ms` faria o RTT
    /// continuar congelado até a próxima troca de rota; sondar a URL escolhida
    /// mantém as duas cadências independentes.
    pub async fn measure_current_peer(
        &self,
        clientes: &crate::peer_client::PeerClients,
        node_id: &str,
    ) -> Option<f64> {
        let rota = self.resolve(node_id)?;
        let http = clientes.probe().ok()?;
        let inicio = Instant::now();
        let resposta =
            tokio::time::timeout(TIMEOUT_SONDA, http.get(format!("{rota}/health")).send()).await;
        match resposta {
            Ok(Ok(response)) if response.status().is_success() => {
                self.mark_success(node_id, &rota);
                Some(inicio.elapsed().as_secs_f64() * 1000.0)
            }
            _ => None,
        }
    }
}

fn host_and_port_of(addr: &str) -> Option<(&str, u16)> {
    let sem_esquema = addr.split_once("://").map_or(addr, |(_, resto)| resto);
    let autoridade = sem_esquema.split('/').next().unwrap_or(sem_esquema);
    let (host, port) = if autoridade.starts_with('[') {
        let fim_host = autoridade.find(']')?;
        let host = &autoridade[1..fim_host];
        let port = autoridade.get(fim_host + 1..)?.strip_prefix(':')?;
        (host, port)
    } else {
        autoridade.rsplit_once(':')?
    };
    Some((host, port.parse().ok()?))
}

/// Uma medição de latência de ida e volta partindo deste nó para um destino.
/// O sentido inverso é outra medição, mantida pelo processo remoto.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RttMeasurement {
    pub target_node_id: String,
    pub rtt_ms: f64,
    /// `libp2p` é o transporte normal. `direct-health` identifica o fallback
    /// usado quando o peer ainda não oferece o túnel novo.
    pub transport: String,
}

#[derive(Debug, Clone)]
pub struct RttSnapshot {
    pub measurement: RttMeasurement,
    pub age: Duration,
}

struct RttEntry {
    measurement: RttMeasurement,
    measured_at: Instant,
}

/// RTTs direcionais medidos por este processo.
///
/// Não é uma tabela de menor caminho. Cada entrada é uma medição direta
/// `origem -> destino`; nunca se infere uma aresta pela soma de outras.
#[derive(Default)]
pub struct RttTable {
    por_destino: RwLock<HashMap<String, RttEntry>>,
}

impl RttTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> Vec<RttMeasurement> {
        self.snapshot_with_age()
            .into_iter()
            .map(|entry| entry.measurement)
            .collect()
    }

    pub fn snapshot_with_age(&self) -> Vec<RttSnapshot> {
        let mut snapshots = self
            .por_destino
            .read()
            .map(|table| {
                table
                    .values()
                    .map(|entry| RttSnapshot {
                        measurement: entry.measurement.clone(),
                        age: entry.measured_at.elapsed(),
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        snapshots.sort_by(|a, b| {
            a.measurement
                .target_node_id
                .cmp(&b.measurement.target_node_id)
        });
        snapshots
    }

    pub fn fresh_snapshot(&self) -> Vec<RttMeasurement> {
        self.snapshot_with_age()
            .into_iter()
            .filter(|entry| entry.age < RTT_INTERVALO_MEDICAO)
            .map(|entry| entry.measurement)
            .collect()
    }

    pub fn precisa_medir(&self, node_id: &str) -> bool {
        self.por_destino
            .read()
            .map(|t| match t.get(node_id) {
                Some(entry) => entry.measured_at.elapsed() >= RTT_INTERVALO_MEDICAO,
                None => true,
            })
            .unwrap_or(true)
    }

    pub fn remember(&self, node_id: &str, rtt_ms: f64, transport: &str) {
        if !rtt_ms.is_finite() || rtt_ms < 0.0 {
            return;
        }
        if let Ok(mut t) = self.por_destino.write() {
            t.insert(
                node_id.to_string(),
                RttEntry {
                    measurement: RttMeasurement {
                        target_node_id: node_id.to_string(),
                        rtt_ms,
                        transport: transport.to_string(),
                    },
                    measured_at: Instant::now(),
                },
            );
        }
    }

    pub fn mark_failure(&self, node_id: &str) {
        if let Ok(mut t) = self.por_destino.write() {
            t.remove(node_id);
        }
    }

    #[cfg(test)]
    pub fn remember_for_test(&self, node_id: &str, rtt_ms: f64, transport: &str) {
        self.remember(node_id, rtt_ms, transport);
    }

    #[cfg(test)]
    pub fn remember_for_test_with_age(
        &self,
        node_id: &str,
        rtt_ms: f64,
        transport: &str,
        age: Duration,
    ) {
        if let Ok(mut table) = self.por_destino.write() {
            table.insert(
                node_id.to_string(),
                RttEntry {
                    measurement: RttMeasurement {
                        target_node_id: node_id.to_string(),
                        rtt_ms,
                        transport: transport.to_string(),
                    },
                    measured_at: Instant::now() - age,
                },
            );
        }
    }
}

/// A medição direcional é renovada junto com a rota de endereçamento.
pub const RTT_INTERVALO_MEDICAO: Duration = Duration::from_secs(120);

/// Prefere o túnel quando ele respondeu, mas permite que um peer que anunciou
/// a capability e esteja momentaneamente sem o swarm continue mensurável pelo
/// `/health` direto. Antes, esse fallback só existia para peers que nunca
/// anunciavam túnel, então a entrada de RTT era apagada a cada renovação.
pub(crate) fn escolher_rtt(
    tunnel_rtt_ms: Option<f64>,
    direct_health_rtt_ms: Option<f64>,
) -> Option<(f64, &'static str)> {
    tunnel_rtt_ms
        .filter(|rtt| rtt.is_finite() && *rtt >= 0.0)
        .map(|rtt| (rtt, "libp2p"))
        .or_else(|| {
            direct_health_rtt_ms
                .filter(|rtt| rtt.is_finite() && *rtt >= 0.0)
                .map(|rtt| (rtt, "direct-health"))
        })
}

#[cfg(test)]
pub mod tests_support {
    use axum::{routing::get, Router};
    use std::time::Duration;

    /// Sobe um `/health` local que responde depois de `atraso_ms`, e devolve a
    /// URL base. E o mesmo endpoint que a sonda usa em producao.
    pub async fn health_server(atraso_ms: u64) -> String {
        let app = Router::new().route(
            "/health",
            get(move || async move {
                if atraso_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(atraso_ms)).await;
                }
                "ok"
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(addr: &str, rtt: f64) -> Measurement {
        Measurement {
            addr: addr.into(),
            rtt_ms: Some(rtt),
        }
    }

    /// Os numeros sao os medidos em 2026-08-13 entre o MacBook e a ksia:
    /// IPv4 da LAN 9,6 ms de p50 contra 7,2 ms do IPv6 global, empate dentro
    /// do ruido do Wi-Fi. Sem margem, esse par trocaria de rota a cada ciclo.
    #[test]
    fn a_tie_within_the_margin_keeps_the_current_route() {
        let medidas = vec![m("192.168.100.138:8092", 9.6), m("[2804:d55::1]:8092", 7.2)];
        let escolha = choose_route(&medidas, Some("192.168.100.138:8092")).unwrap();
        assert_eq!(
            escolha.addr, "192.168.100.138:8092",
            "7,2 vs 9,6 nao passa dos 25%"
        );
    }

    #[test]
    fn a_clear_win_switches_the_route() {
        let medidas = vec![
            m("192.168.100.138:8092", 40.0),
            m("[2804:d55::1]:8092", 7.0),
        ];
        let escolha = choose_route(&medidas, Some("192.168.100.138:8092")).unwrap();
        assert_eq!(escolha.addr, "[2804:d55::1]:8092");
    }

    /// Diferenca percentual grande mas absoluta minuscula: 0,8 contra 1,2 ms
    /// passa dos 25% e nao chega aos 2 ms. Trocar aqui e ruido.
    #[test]
    fn a_percentage_win_under_two_milliseconds_does_not_switch() {
        let medidas = vec![m("a:1", 1.2), m("b:1", 0.8)];
        let escolha = choose_route(&medidas, Some("a:1")).unwrap();
        assert_eq!(escolha.addr, "a:1");
    }

    #[test]
    fn without_a_current_route_the_fastest_wins() {
        let medidas = vec![m("a:1", 9.6), m("b:1", 7.2)];
        assert_eq!(choose_route(&medidas, None).unwrap().addr, "b:1");
    }

    /// Um candidato que nao respondeu nao e escolhivel, mesmo sendo o atual.
    #[test]
    fn an_unreachable_current_route_is_replaced_at_once() {
        let medidas = vec![
            Measurement {
                addr: "a:1".into(),
                rtt_ms: None,
            },
            m("b:1", 50.0),
        ];
        assert_eq!(choose_route(&medidas, Some("a:1")).unwrap().addr, "b:1");
    }

    #[test]
    fn no_reachable_candidate_yields_none() {
        let medidas = vec![Measurement {
            addr: "a:1".into(),
            rtt_ms: None,
        }];
        assert!(choose_route(&medidas, Some("a:1")).is_none());
    }

    #[test]
    fn rtt_table_keeps_directional_measurements_and_transport() {
        let table = RttTable::new();
        table.remember_for_test("peer-b", 12.5, "libp2p");
        table.remember_for_test("peer-c", 30.0, "direct-health");

        assert_eq!(
            table.snapshot(),
            vec![
                RttMeasurement {
                    target_node_id: "peer-b".into(),
                    rtt_ms: 12.5,
                    transport: "libp2p".into(),
                },
                RttMeasurement {
                    target_node_id: "peer-c".into(),
                    rtt_ms: 30.0,
                    transport: "direct-health".into(),
                },
            ]
        );
        assert!(table.precisa_medir("peer-d"));
    }

    #[test]
    fn direct_health_is_a_fallback_when_the_tunnel_measurement_is_unavailable() {
        assert_eq!(escolher_rtt(None, Some(3.5)), Some((3.5, "direct-health")));
        assert_eq!(escolher_rtt(Some(1.5), Some(3.5)), Some((1.5, "libp2p")));
    }

    #[test]
    fn a_stale_rtt_is_visible_but_not_fresh_for_selection() {
        let table = RttTable::new();
        table.remember_for_test("fresh", 12.5, "libp2p");
        table.remember_for_test_with_age(
            "stale",
            1.0,
            "direct-health",
            RTT_INTERVALO_MEDICAO + Duration::from_secs(1),
        );

        let with_age = table.snapshot_with_age();
        assert_eq!(with_age.len(), 2);
        assert!(with_age.iter().any(|entry| {
            entry.measurement.target_node_id == "stale" && entry.age >= RTT_INTERVALO_MEDICAO
        }));
        assert_eq!(
            table
                .fresh_snapshot()
                .into_iter()
                .map(|measurement| measurement.target_node_id)
                .collect::<Vec<_>>(),
            vec!["fresh"]
        );
    }

    /// Empate exato entre dois candidatos sem rota atual: vence a ordem
    /// lexicografica, para dois nos chegarem a mesma conclusao em vez de
    /// divergirem por ordem de HashMap.
    #[test]
    fn an_exact_tie_is_broken_deterministically() {
        let medidas = vec![m("b:1", 5.0), m("a:1", 5.0)];
        assert_eq!(choose_route(&medidas, None).unwrap().addr, "a:1");
    }

    /// Dois servidores locais com atrasos diferentes: o mais rapido vence, e a
    /// tabela passa a resolver aquele node_id para ele.
    #[tokio::test]
    async fn measuring_two_candidates_picks_the_faster_one() {
        let rapido = tests_support::health_server(0).await;
        let lento = tests_support::health_server(80).await;
        let tabela = RouteTable::new();
        let http = crate::peer_client::PeerClients::new(Duration::from_secs(10));
        tabela
            .measure_peer(&http, "n1", &[lento.clone(), rapido.clone()])
            .await;
        assert_eq!(tabela.resolve("n1"), Some(rapido));
    }

    /// Uma falha marcada tira o endereco de uso imediatamente, sem esperar o
    /// ciclo de 5 minutos: a proxima requisicao ja vai por outro caminho.
    #[tokio::test]
    async fn a_failure_drops_the_route_right_away() {
        let vivo = tests_support::health_server(0).await;
        let tabela = RouteTable::new();
        let http = crate::peer_client::PeerClients::new(Duration::from_secs(10));
        tabela.measure_peer(&http, "n1", &[vivo.clone()]).await;
        assert_eq!(tabela.resolve("n1"), Some(vivo.clone()));
        tabela.mark_failure("n1", &vivo);
        assert_eq!(tabela.resolve("n1"), None);
    }

    #[test]
    fn an_address_update_invalidates_the_stale_route() {
        let tabela = RouteTable::new();
        tabela.remember_for_test("n1", "https://172.17.0.1:8092", 1.0);

        tabela.invalidate("n1");

        assert_eq!(tabela.resolve("n1"), None);
    }

    #[test]
    fn a_stale_route_is_not_used_for_inference_address_selection() {
        let tabela = RouteTable::new();
        tabela.remember_for_test_with_age(
            "n1",
            "https://172.17.0.1:8092",
            1.0,
            INTERVALO_MEDICAO + Duration::from_secs(1),
        );

        assert_eq!(tabela.resolve("n1"), None);
    }

    /// Candidato morto nao e escolhido so por estar na lista.
    #[tokio::test]
    async fn a_dead_candidate_is_never_chosen() {
        let vivo = tests_support::health_server(0).await;
        let tabela = RouteTable::new();
        let http = crate::peer_client::PeerClients::new(Duration::from_secs(10));
        // Porta 1 fechada: a sonda falha rapido em vez de esperar o timeout.
        tabela
            .measure_peer(
                &http,
                "n1",
                &["http://127.0.0.1:1".to_string(), vivo.clone()],
            )
            .await;
        assert_eq!(tabela.resolve("n1"), Some(vivo));
    }

    /// A rota com esquema e o endereço cru com a mesma porta comparam iguais,
    /// mas serviços diferentes no mesmo host continuam independentes.
    #[test]
    fn endpoint_includes_its_port_with_or_without_a_url_scheme() {
        assert_eq!(
            host_and_port_of("http://[2804:d55::9]:8080"),
            Some(("2804:d55::9", 8080))
        );
        assert_eq!(
            host_and_port_of("[2804:d55::9]:42777"),
            Some(("2804:d55::9", 42777))
        );
    }

    /// Uma falha no gateway não apaga o endereço P2P usado pelo túnel ou
    /// prefill, mesmo quando ambos resolvem para o mesmo host.
    #[test]
    fn a_dead_gateway_does_not_discredit_the_peer_transport_port() {
        let tabela = RouteTable::new();
        tabela.mark_failure("n1", "http://[2804:d55::9]:8080");
        assert!(!tabela.endpoint_is_discredited("n1", "[2804:d55::9]:42777"));
        assert!(tabela.endpoint_is_discredited("n1", "http://[2804:d55::9]:8080"));
        assert!(!tabela.endpoint_is_discredited("n1", "[2804:d55::a]:42777"));
        assert!(!tabela.endpoint_is_discredited("n2", "[2804:d55::9]:42777"));
    }

    /// `usable` devolve vazio quando tudo falhou, e isso é informação: o
    /// chamador tem de pedir o endereço atual em vez de escolher um cadáver.
    #[test]
    fn usable_is_empty_when_every_candidate_is_discredited() {
        let tabela = RouteTable::new();
        let candidatos = vec![
            "http://10.0.0.5:8080".to_string(),
            "http://10.0.0.6:8080".to_string(),
        ];
        assert_eq!(tabela.usable("n1", &candidatos), candidatos);
        tabela.mark_failure("n1", &candidatos[0]);
        assert_eq!(
            tabela.usable("n1", &candidatos),
            vec![candidatos[1].clone()]
        );
        tabela.mark_failure("n1", &candidatos[1]);
        assert!(tabela.usable("n1", &candidatos).is_empty());
        tabela.mark_success("n1", &candidatos[0]);
        assert_eq!(
            tabela.usable("n1", &candidatos),
            vec![candidatos[0].clone()]
        );
    }

    #[tokio::test]
    async fn a_route_probe_must_test_the_inference_endpoint() {
        let blackhole = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let blackhole_addr = blackhole.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((stream, _)) = blackhole.accept().await else {
                return;
            };
            let _stream = stream;
            std::future::pending::<()>().await;
        });

        let inference = format!("http://{blackhole_addr}");
        let healthy_inference = tests_support::health_server(0).await;
        let tabela = RouteTable::new();
        tabela
            .measure_peer(
                &crate::peer_client::PeerClients::new(Duration::from_secs(10)),
                "n1",
                &[inference, healthy_inference.clone()],
            )
            .await;

        assert_eq!(
            tabela.resolve("n1"),
            Some(healthy_inference),
            "a sonda precisa medir a porta de inferência, não um gateway diferente"
        );
    }

    #[tokio::test]
    async fn current_route_rtt_is_measured_without_replacing_the_route() {
        let healthy = tests_support::health_server(0).await;
        let tabela = RouteTable::new();
        let http = crate::peer_client::PeerClients::new(Duration::from_secs(10));
        tabela
            .measure_peer(&http, "n1", std::slice::from_ref(&healthy))
            .await;
        let before = tabela.resolve("n1");

        let rtt = tabela.measure_current_peer(&http, "n1").await;

        assert!(rtt.is_some());
        assert_eq!(tabela.resolve("n1"), before);
    }

    /// Um peer nunca medido nao resolve para nada - o chamador cai no
    /// `inference_url` do plano, que e o comportamento de hoje.
    #[test]
    fn an_unmeasured_peer_resolves_to_nothing() {
        assert_eq!(RouteTable::new().resolve("desconhecido"), None);
    }

    /// Recem-medido nao remede: e o que mantem a sonda barata no regime normal.
    #[tokio::test]
    async fn a_freshly_measured_peer_is_not_measured_again() {
        let vivo = tests_support::health_server(0).await;
        let tabela = RouteTable::new();
        assert!(tabela.precisa_medir("n1"), "nunca medido precisa medir");
        tabela
            .measure_peer(
                &crate::peer_client::PeerClients::new(Duration::from_secs(10)),
                "n1",
                &[vivo],
            )
            .await;
        assert!(!tabela.precisa_medir("n1"));
    }
}
