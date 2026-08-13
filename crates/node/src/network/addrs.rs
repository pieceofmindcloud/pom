//! Enderecos em que esta maquina pode ser encontrada.
//!
//! Separado de `route.rs` porque e o unico ponto com codigo por plataforma. A
//! filtragem, que e a parte que erra calado, fica fora do `cfg` e tem teste
//! que roda em qualquer sistema.

/// Teto por familia. Ver o teste `at_most_two_candidates_per_family_survive`.
const MAX_POR_FAMILIA: usize = 2;

#[derive(Debug, Clone, PartialEq)]
pub struct LocalAddr {
    pub ip: String,
    /// Endereco que o sistema marcou como em fim de vida. O macOS mantem estes
    /// por um tempo depois de rotacionar o IPv6 de privacidade.
    pub deprecated: bool,
    /// IPv6 de privacidade, que roda sozinho. Serve, mas perde para o `secured`.
    pub temporary: bool,
}

fn e_ipv6(ip: &str) -> bool {
    ip.contains(':')
}

fn descartavel(ip: &str) -> bool {
    ip == "127.0.0.1"
        || ip == "::1"
        || ip.starts_with("fe80:")
        || ip.starts_with("169.254.")
        || ip.starts_with("0.0.0.0")
}

/// Aplica as regras da spec: descarta o que nao serve, prefere o estavel, e
/// limita quantos candidatos cada familia contribui.
pub fn filter_candidates(brutos: Vec<LocalAddr>, fixado: Option<&str>) -> Vec<String> {
    let mut uteis: Vec<LocalAddr> = brutos
        .into_iter()
        .filter(|a| !a.deprecated && !descartavel(&a.ip))
        .collect();
    // Estavel antes de temporario, e ordem lexicografica para desempatar: o
    // conjunto anunciado nao pode mudar so porque a enumeracao veio noutra ordem.
    uteis.sort_by(|a, b| a.temporary.cmp(&b.temporary).then_with(|| a.ip.cmp(&b.ip)));

    let mut saida: Vec<String> = Vec::new();
    if let Some(f) = fixado {
        // Nao conta para o teto: e intencao explicita do operador.
        saida.push(f.to_string());
    }
    let (mut v4, mut v6) = (0usize, 0usize);
    for a in uteis {
        if saida.iter().any(|s| *s == a.ip) {
            continue;
        }
        let conta = if e_ipv6(&a.ip) { &mut v6 } else { &mut v4 };
        if *conta >= MAX_POR_FAMILIA {
            continue;
        }
        *conta += 1;
        saida.push(a.ip);
    }
    saida
}

/// Enderecos das interfaces desta maquina.
///
/// Sem dependencia nova: le a saida de `ifconfig` (macOS) ou `ip -o addr`
/// (Linux). Windows fica com a lista vazia por ora e cai no endereco observado
/// pelo peer - que e exatamente o comportamento de hoje, nao uma regressao.
pub fn local_addrs() -> Vec<LocalAddr> {
    #[cfg(target_os = "macos")]
    {
        parse_ifconfig(&run("ifconfig", &[]))
    }
    #[cfg(target_os = "linux")]
    {
        parse_ip_addr(&run("ip", &["-o", "addr"]))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Vec::new()
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn run(programa: &str, args: &[&str]) -> String {
    std::process::Command::new(programa)
        .args(args)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default()
}

#[cfg(target_os = "macos")]
fn parse_ifconfig(saida: &str) -> Vec<LocalAddr> {
    let mut out = Vec::new();
    for linha in saida.lines() {
        let l = linha.trim();
        let campos: Vec<&str> = l.split_whitespace().collect();
        if campos.len() < 2 || (campos[0] != "inet" && campos[0] != "inet6") {
            continue;
        }
        // "inet6 fe80::1%en0" - o escopo nao faz parte do endereco.
        let ip = campos[1].split('%').next().unwrap_or(campos[1]).to_string();
        out.push(LocalAddr {
            ip,
            deprecated: l.contains("deprecated"),
            temporary: l.contains("temporary"),
        });
    }
    out
}

#[cfg(target_os = "linux")]
fn parse_ip_addr(saida: &str) -> Vec<LocalAddr> {
    let mut out = Vec::new();
    for linha in saida.lines() {
        let campos: Vec<&str> = linha.split_whitespace().collect();
        let Some(pos) = campos.iter().position(|c| *c == "inet" || *c == "inet6") else {
            continue;
        };
        let Some(cidr) = campos.get(pos + 1) else { continue };
        let ip = cidr.split('/').next().unwrap_or(cidr).to_string();
        out.push(LocalAddr {
            ip,
            deprecated: linha.contains("deprecated"),
            temporary: linha.contains("temporary"),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(ip: &str) -> LocalAddr {
        LocalAddr { ip: ip.into(), deprecated: false, temporary: false }
    }

    #[test]
    fn deprecated_and_link_local_and_loopback_are_dropped() {
        let brutos = vec![
            a("192.168.100.240"),
            LocalAddr { ip: "2804:d55::9a0d".into(), deprecated: true, temporary: true },
            a("fe80::1"),
            a("127.0.0.1"),
            a("::1"),
        ];
        assert_eq!(filter_candidates(brutos, None), vec!["192.168.100.240"]);
    }

    /// O macOS cria varios IPv6 sozinho - cinco nesta maquina em 2026-08-13.
    /// PeerList cresce ~530 bytes por peer e ja e fatiado por MTU, entao o teto
    /// nao e estetica: sem ele a descoberta vira fragmentacao.
    #[test]
    fn at_most_two_candidates_per_family_survive() {
        let brutos = vec![
            a("192.168.1.1"),
            a("192.168.1.2"),
            a("192.168.1.3"),
            a("2804:d55::1"),
            a("2804:d55::2"),
            a("2804:d55::3"),
        ];
        let saida = filter_candidates(brutos, None);
        assert_eq!(saida.iter().filter(|s| s.contains(':')).count(), 2);
        assert_eq!(saida.iter().filter(|s| !s.contains(':')).count(), 2);
    }

    /// O endereco fixado pelo operador e intencao explicita: vem primeiro e
    /// nao conta para o teto, senao a filtragem poderia descarta-lo.
    #[test]
    fn a_pinned_address_comes_first_and_is_never_dropped() {
        let brutos = vec![a("192.168.1.1"), a("192.168.1.2"), a("192.168.1.3")];
        let saida = filter_candidates(brutos, Some("2804:d55::99"));
        assert_eq!(saida[0], "2804:d55::99");
        assert!(saida.contains(&"192.168.1.1".to_string()));
    }

    #[test]
    fn a_secured_address_wins_over_a_temporary_one() {
        let brutos = vec![
            LocalAddr { ip: "2804:d55::temp1".into(), deprecated: false, temporary: true },
            LocalAddr { ip: "2804:d55::sec".into(), deprecated: false, temporary: false },
            LocalAddr { ip: "2804:d55::temp2".into(), deprecated: false, temporary: true },
        ];
        assert_eq!(filter_candidates(brutos, None)[0], "2804:d55::sec");
    }

    #[test]
    fn duplicates_are_collapsed() {
        let brutos = vec![a("192.168.1.1"), a("192.168.1.1")];
        assert_eq!(filter_candidates(brutos, None).len(), 1);
    }

    /// A maquina de desenvolvimento tem enderecos de verdade, e a filtragem
    /// nunca pode devolver algo indiscavel a partir deles.
    #[test]
    fn the_real_machine_yields_only_usable_addresses() {
        let achados = filter_candidates(local_addrs(), None);
        // Sem esta linha o teste passaria por vacuidade numa maquina onde a
        // enumeracao nao funcionasse, que e justamente o que ele deveria pegar.
        assert!(!achados.is_empty(), "a enumeracao nao achou endereco nenhum");
        assert!(achados.len() <= 4, "teto de 2 por familia: {achados:?}");
        for ip in achados {
            assert!(!descartavel(&ip), "{ip} nao deveria ter passado");
        }
    }
}
