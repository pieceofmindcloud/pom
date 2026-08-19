//! Escolha do endereco por onde falar com cada peer.
//!
//! Um no tem mais de um endereco valido, e qual deles serve depende de quem
//! pergunta. Este modulo guarda a medicao de cada candidato e decide qual usar.
//! A decisao mora aqui, sozinha e pura, porque errar nela nao da erro: o no
//! sobe, aparece no cluster, e a requisicao morre calada no primeiro salto.

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
const TIMEOUT_SONDA: Duration = Duration::from_secs(3);

struct Entrada {
    escolha: RouteChoice,
    medido_em: Instant,
}

/// Por onde este no fala com cada peer.
///
/// Vive so na memoria deste processo e nao e publicada no gossip: publicar
/// tabela de medicao transformaria enderecamento em estado distribuido, e o
/// `PeerList` ja esta no limite da MTU.
#[derive(Default)]
pub struct RouteTable {
    por_no: RwLock<HashMap<String, Entrada>>,
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
    /// A sonda e `GET /health` por HTTP na porta de inferencia, nao ICMP: mede
    /// o caminho que a inferencia usa de verdade, com firewall e proxy no meio.
    /// ICMP liberado onde o TCP nao esta mediria um caminho que ninguem percorre.
    /// O cliente vem do cache por peer porque a sonda fala com o mesmo servidor
    /// que a inferência vai falar: se o cluster está em TLS, medir em texto
    /// claro mediria um caminho que a inferência não percorre - e, no modo
    /// estrito, um caminho que o outro lado recusa.
    pub async fn measure_peer(
        &self,
        clientes: &crate::peer_client::PeerClients,
        node_id: &str,
        candidatos: &[String],
    ) {
        let Ok(http) = clientes.for_peer(node_id) else {
            return;
        };
        let mut medidas = Vec::with_capacity(candidatos.len());
        for base in candidatos {
            let inicio = Instant::now();
            let resposta =
                tokio::time::timeout(TIMEOUT_SONDA, http.get(format!("{base}/health")).send())
                    .await;
            let rtt = match resposta {
                Ok(Ok(r)) if r.status().is_success() => {
                    Some(inicio.elapsed().as_secs_f64() * 1000.0)
                }
                _ => None,
            };
            medidas.push(Measurement {
                addr: base.clone(),
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
