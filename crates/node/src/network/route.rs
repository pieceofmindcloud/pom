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
                return Some(RouteChoice { addr: atual_addr.to_string(), rtt_ms: atual_rtt });
            }
        }
    }
    Some(RouteChoice { addr: melhor_addr.to_string(), rtt_ms: melhor_rtt })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(addr: &str, rtt: f64) -> Measurement {
        Measurement { addr: addr.into(), rtt_ms: Some(rtt) }
    }

    /// Os numeros sao os medidos em 2026-08-13 entre o MacBook e a ksia:
    /// IPv4 da LAN 9,6 ms de p50 contra 7,2 ms do IPv6 global, empate dentro
    /// do ruido do Wi-Fi. Sem margem, esse par trocaria de rota a cada ciclo.
    #[test]
    fn a_tie_within_the_margin_keeps_the_current_route() {
        let medidas = vec![m("192.168.100.138:8092", 9.6), m("[2804:d55::1]:8092", 7.2)];
        let escolha = choose_route(&medidas, Some("192.168.100.138:8092")).unwrap();
        assert_eq!(escolha.addr, "192.168.100.138:8092", "7,2 vs 9,6 nao passa dos 25%");
    }

    #[test]
    fn a_clear_win_switches_the_route() {
        let medidas = vec![m("192.168.100.138:8092", 40.0), m("[2804:d55::1]:8092", 7.0)];
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
            Measurement { addr: "a:1".into(), rtt_ms: None },
            m("b:1", 50.0),
        ];
        assert_eq!(choose_route(&medidas, Some("a:1")).unwrap().addr, "b:1");
    }

    #[test]
    fn no_reachable_candidate_yields_none() {
        let medidas = vec![Measurement { addr: "a:1".into(), rtt_ms: None }];
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
}
