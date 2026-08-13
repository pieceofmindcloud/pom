# Rota dinâmica e implantação incremental - Plano de Implementação

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Cada nó passa a anunciar vários endereços, e quem vai discar escolhe o melhor pela própria medição; e uma máquina pode ser somada ou tirada de um modelo já implantado, criando rotas paralelas.

**Architecture:** Duas fases independentes. A fase A troca o endereço único de `PeerInfo` por um conjunto de candidatos, mede cada um por HTTP e resolve no ponto de discagem em vez de no ponto de planejamento. A fase B abre dois endpoints incrementais sobre o `deploy.rs` existente, que tocam um nó só, e passa a permitir faixas sobrepostas no cluster mantendo a invariante "uma camada, um nó" dentro de cada plano.

**Tech Stack:** Rust (tokio, axum, serde, reqwest), React + TypeScript + Tailwind (`apps/frontend`), testes com `cargo test` e `vitest`.

**Spec:** `docs/superpowers/specs/2026-08-13-rota-dinamica-e-implantacao-incremental-design.md`

## Global Constraints

- **Nunca use travessão "—" em texto.** Use hífen "-". Vale para código, comentários, mensagens de commit e textos de interface.
- **Comentários em português** nos arquivos que já usam português; em inglês nos que já usam inglês. Não traduza o que já existe.
- **Teto de candidatos: 2 por família de endereço** (2 IPv4 + 2 IPv6 no máximo).
- **Margem de troca de rota: 25% E pelo menos 2 ms.** As duas condições, não uma.
- **Intervalo de remedição: 5 minutos.**
- **Sonda é `GET /health` por HTTP na porta de inferência.** Nunca ICMP.
- **`inference_url` continua sendo preenchido** em todo `PipelineStep`, mesmo quando há candidatos. Compatibilidade com nó rodando binário anterior.
- **Invariante inviolável:** dentro de um mesmo `PipelinePlan`, cada camada é servida por exatamente um nó. No cluster, N nós podem cobrir a mesma faixa.
- **Depois de editar `apps/frontend`, rode `npm run build` lá dentro.** A interface é lida do disco em tempo de execução; `cargo build` não a reconstrói.
- Todo campo novo em mensagem de rede leva `#[serde(default)]`, senão um nó velho deixa de ser parseável.

## Estrutura de arquivos

**Criar:**
- `crates/node/src/network/route.rs` - tabela de rotas, escolha e medição. Unidade nova e isolada: recebe candidatos e medições, devolve o endereço escolhido. É onde mora toda a lógica que erra calado.
- `crates/node/src/network/addrs.rs` - enumeração e filtragem dos endereços locais. Separado de `route.rs` porque é o único ponto com código por plataforma.

**Modificar:**
- `crates/node/src/network/network.rs` - `Announce` e `PeerInfo` ganham `addr_candidates`; o endereço observado passa a ser acrescentado.
- `crates/node/src/routing/pipeline.rs` - `PipelineStep` ganha `inference_candidates`.
- `crates/node/src/backend/gateway.rs` - `all_peers` publica os candidatos próprios.
- `crates/node/src/inference/prefill.rs` - resolve a rota no ponto de discagem, fixada por sessão.
- `crates/node/src/backend/webui.rs` - expõe a rota escolhida por peer e as arestas paralelas.
- `crates/node/src/backend/deploy.rs` - endpoints incrementais.
- `apps/frontend/src/lib/types.ts`, `src/routes/Dashboard.tsx`, `src/components/ModelNetworkCanvas.tsx`, `src/i18n/locales.ts`.

---

# Fase A: rota dinâmica

### Task A1: escolha da rota (função pura)

**Files:**
- Create: `crates/node/src/network/route.rs`
- Modify: `crates/node/src/network/mod.rs`

**Interfaces:**
- Produces: `RouteChoice { addr: String, rtt_ms: f64 }`, `Measurement { addr: String, rtt_ms: Option<f64> }`, `pub fn choose_route(medidas: &[Measurement], atual: Option<&str>) -> Option<RouteChoice>`

- [x] **Step 1: Write the failing test**

Crie `crates/node/src/network/route.rs` contendo apenas o bloco de testes abaixo (a implementação vem no passo 3):

```rust
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
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test -p node route::`
Expected: FAIL na compilação, `cannot find type Measurement in this scope`.

- [x] **Step 3: Write minimal implementation**

No topo de `crates/node/src/network/route.rs`, antes do `mod tests`:

```rust
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
```

Em `crates/node/src/network/mod.rs`, acrescente a linha:

```rust
pub mod route;
```

- [x] **Step 4: Run test to verify it passes**

Run: `cargo test -p node route::`
Expected: PASS, 7 testes.

- [x] **Step 5: Commit**

```bash
git add crates/node/src/network/route.rs crates/node/src/network/mod.rs
git commit -m "feat(rota): escolha de endereco com margem medida"
```

---

### Task A2: candidatos locais e filtragem

**Files:**
- Create: `crates/node/src/network/addrs.rs`
- Modify: `crates/node/src/network/mod.rs`

**Interfaces:**
- Consumes: nada.
- Produces: `pub struct LocalAddr { pub ip: String, pub deprecated: bool, pub temporary: bool }`, `pub fn filter_candidates(brutos: Vec<LocalAddr>, fixado: Option<&str>) -> Vec<String>`, `pub fn local_addrs() -> Vec<LocalAddr>`

- [x] **Step 1: Write the failing test**

Crie `crates/node/src/network/addrs.rs` com só o bloco de testes:

```rust
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
            a("192.168.1.1"), a("192.168.1.2"), a("192.168.1.3"),
            a("2804:d55::1"), a("2804:d55::2"), a("2804:d55::3"),
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
}
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test -p node addrs::`
Expected: FAIL, `cannot find type LocalAddr in this scope`.

- [x] **Step 3: Write minimal implementation**

No topo de `crates/node/src/network/addrs.rs`:

```rust
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
/// (Linux). Windows fica com a lista vazia por ora, e cai no endereco observado
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
```

Em `crates/node/src/network/mod.rs`, acrescente:

```rust
pub mod addrs;
```

- [x] **Step 4: Run test to verify it passes**

Run: `cargo test -p node addrs::`
Expected: PASS, 5 testes.

- [x] **Step 5: Commit**

```bash
git add crates/node/src/network/addrs.rs crates/node/src/network/mod.rs
git commit -m "feat(rota): enumeracao e filtragem dos enderecos locais"
```

---

### Task A3: `Announce` e `PeerInfo` carregam o conjunto

**Files:**
- Modify: `crates/node/src/network/network.rs`
- Modify: `crates/node/src/backend/gateway.rs:161-190`

**Interfaces:**
- Consumes: `addrs::filter_candidates`, `addrs::local_addrs`.
- Produces: `PeerInfo.addr_candidates: Vec<String>`, `PeerInfo::inference_candidates(&self) -> Vec<String>`, `Network::self_addr_candidates(&self) -> Vec<String>`

- [x] **Step 1: Write the failing test**

Acrescente ao `mod tests` de `crates/node/src/network/network.rs`:

```rust
/// O endereco de origem do pacote e acrescentado, nao substitui: e o unico
/// caminho comprovadamente funcional, mas nao e o unico que existe. Substituir
/// era o que fazia dois nos da mesma LAN, que se conheceram pela internet,
/// nunca descobrirem que tambem eram vizinhos.
#[test]
fn the_observed_address_joins_the_candidate_set() {
    let mut peer = fake_peer();
    peer.addr_candidates = vec!["192.168.1.9:42777".into()];
    peer.merge_observed("[2804:f30::1]:42777");
    assert_eq!(
        peer.addr_candidates,
        vec!["192.168.1.9:42777".to_string(), "[2804:f30::1]:42777".to_string()],
    );
}

#[test]
fn an_already_known_observed_address_is_not_duplicated() {
    let mut peer = fake_peer();
    peer.addr_candidates = vec!["192.168.1.9:42777".into()];
    peer.merge_observed("192.168.1.9:42777");
    assert_eq!(peer.addr_candidates.len(), 1);
}

/// Um no rodando binario anterior nao manda `addr_candidates`. O campo cai no
/// default e o `addr` unico dele vira o unico candidato: o cluster de versoes
/// misturadas continua funcionando.
#[test]
fn a_peer_without_candidates_falls_back_to_its_single_addr() {
    let mut peer = fake_peer();
    peer.addr = "192.168.1.9:42777".into();
    peer.addr_candidates = Vec::new();
    assert_eq!(peer.inference_candidates(), vec!["http://192.168.1.9:8181".to_string()]);
}
```

Use o helper `fake_peer()` já existente no módulo de testes; se ele não existir com esse nome, crie-o a partir do `PeerInfo` montado no teste `peer_list_chunks` (linhas ~890) e reaproveite.

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test -p node network::tests::the_observed`
Expected: FAIL, `no field addr_candidates on type PeerInfo`.

- [x] **Step 3: Write minimal implementation**

Em `PeerInfo` (após o campo `addr`):

```rust
    /// Todos os enderecos em que este peer pode ser encontrado.
    ///
    /// `addr` continua sendo o preferido historico e o unico que um no antigo
    /// entende. Este conjunto e o que permite a quem vai discar escolher, em
    /// vez de aceitar a escolha de quem planejou de outro lugar da rede.
    #[serde(default)]
    pub addr_candidates: Vec<String>,
```

No `impl PeerInfo`:

```rust
    /// Acrescenta o endereco de onde o pacote realmente veio.
    ///
    /// Nao substitui: um caminho que funcionou nao invalida os outros, e e este
    /// o unico candidato que enxerga NAT.
    pub fn merge_observed(&mut self, observado: &str) {
        if !self.addr_candidates.iter().any(|c| c == observado) {
            self.addr_candidates.push(observado.to_string());
        }
    }

    /// URLs do servidor de inferencia, uma por candidato.
    ///
    /// Cai no `addr` unico quando o peer nao anunciou candidatos, que e o caso
    /// de um no rodando binario anterior.
    pub fn inference_candidates(&self) -> Vec<String> {
        let Some(porta) = self.inference_port else {
            return Vec::new();
        };
        let base: Vec<&String> = if self.addr_candidates.is_empty() {
            vec![&self.addr]
        } else {
            self.addr_candidates.iter().collect()
        };
        base.iter()
            .map(|a| format!("http://{}:{}", ip_host(a), porta))
            .collect()
    }
```

Em `P2PMessage::Announce`, após `addr: String`:

```rust
        /// Todos os enderecos deste no. `serde(default)` mantem um no antigo
        /// parseavel: sem candidatos, o `addr` unico continua valendo.
        #[serde(default)]
        addr_candidates: Vec<String>,
```

Em `Network`, o método que monta o conjunto próprio:

```rust
    /// Enderecos que este no anuncia de si.
    pub fn self_addr_candidates(&self) -> Vec<String> {
        let porta = self.bind_addr.port();
        crate::network::addrs::filter_candidates(
            crate::network::addrs::local_addrs(),
            self.advertised_host().as_deref(),
        )
        .into_iter()
        .map(|ip| {
            if ip.contains(':') && !ip.starts_with('[') {
                format!("[{ip}]:{porta}")
            } else {
                format!("{ip}:{porta}")
            }
        })
        .collect()
    }
```

Preencha `addr_candidates: self.self_addr_candidates()` em `self_announce`, e em `handle_message` (braço `Announce`), depois de montar o `peer`, chame:

```rust
                let mut peer = peer;
                peer.merge_observed(&peer_addr.clone());
```

Em `gateway.rs`, no `PeerInfo` da entrada própria, acrescente
`addr_candidates: self.network.self_addr_candidates(),`.

Compile e corrija os demais literais de `PeerInfo` que o compilador apontar
(testes e `webui.rs`) acrescentando `addr_candidates: Vec::new(),`.

- [x] **Step 4: Run test to verify it passes**

Run: `cargo test -p node`
Expected: PASS, todos os testes anteriores mais os 3 novos.

- [x] **Step 5: Commit**

```bash
git add crates/node/src/network/network.rs crates/node/src/backend/gateway.rs
git commit -m "feat(rota): Announce e PeerInfo carregam conjunto de enderecos"
```

---

### Task A4: medição periódica e tabela de rotas

**Files:**
- Modify: `crates/node/src/network/route.rs`
- Modify: `crates/node/src/network/network.rs` (laço de gossip)

**Interfaces:**
- Consumes: `choose_route`, `Measurement`, `PeerInfo::inference_candidates`.
- Produces: `pub struct RouteTable`, `RouteTable::new()`, `RouteTable::resolve(&self, node_id: &str) -> Option<String>`, `RouteTable::measure_peer(&self, http: &reqwest::Client, node_id: &str, candidatos: &[String])`, `RouteTable::snapshot(&self) -> Vec<(String, RouteChoice)>`, `RouteTable::mark_failure(&self, node_id: &str, addr: &str)`

- [x] **Step 1: Write the failing test**

Acrescente ao `mod tests` de `route.rs`:

```rust
    use std::time::Duration;

    /// Dois servidores locais com atrasos diferentes: o mais rapido vence, e a
    /// tabela passa a resolver aquele node_id para ele.
    #[tokio::test]
    async fn measuring_two_candidates_picks_the_faster_one() {
        let rapido = crate::network::route::tests_support::health_server(0).await;
        let lento = crate::network::route::tests_support::health_server(60).await;
        let tabela = RouteTable::new();
        let http = reqwest::Client::new();
        tabela
            .measure_peer(&http, "n1", &[lento.clone(), rapido.clone()])
            .await;
        assert_eq!(tabela.resolve("n1"), Some(rapido));
    }

    /// Uma falha marcada tira o endereco de uso imediatamente, sem esperar o
    /// ciclo de 5 minutos: a proxima requisicao ja vai por outro caminho.
    #[tokio::test]
    async fn a_failure_drops_the_route_right_away() {
        let vivo = crate::network::route::tests_support::health_server(0).await;
        let tabela = RouteTable::new();
        let http = reqwest::Client::new();
        tabela.measure_peer(&http, "n1", &[vivo.clone()]).await;
        assert_eq!(tabela.resolve("n1"), Some(vivo.clone()));
        tabela.mark_failure("n1", &vivo);
        assert_eq!(tabela.resolve("n1"), None);
    }

    /// Um peer nunca medido nao resolve para nada - o chamador cai no
    /// `inference_url` do plano, que e o comportamento de hoje.
    #[test]
    fn an_unmeasured_peer_resolves_to_nothing() {
        assert_eq!(RouteTable::new().resolve("desconhecido"), None);
    }
```

E o apoio de teste, no mesmo arquivo:

```rust
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
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test -p node route::`
Expected: FAIL, `cannot find type RouteTable in this scope`.

- [x] **Step 3: Write minimal implementation**

Em `route.rs`, antes do `mod tests`:

```rust
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
            .map(|t| t.iter().map(|(k, v)| (k.clone(), v.escolha.clone())).collect())
            .unwrap_or_default()
    }

    /// Tira uma rota de uso na hora, sem esperar o ciclo.
    ///
    /// Chamado quando uma requisicao real falhou naquele endereco: esperar 5
    /// minutos significaria falhar tudo que chegasse nesse meio tempo.
    pub fn mark_failure(&self, node_id: &str, addr: &str) {
        if let Ok(mut t) = self.por_no.write() {
            if t.get(node_id).map(|e| e.escolha.addr == addr).unwrap_or(false) {
                t.remove(node_id);
            }
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
    pub async fn measure_peer(
        &self,
        http: &reqwest::Client,
        node_id: &str,
        candidatos: &[String],
    ) {
        let mut medidas = Vec::with_capacity(candidatos.len());
        for base in candidatos {
            let inicio = Instant::now();
            let resposta = tokio::time::timeout(
                TIMEOUT_SONDA,
                http.get(format!("{base}/health")).send(),
            )
            .await;
            let rtt = match resposta {
                Ok(Ok(r)) if r.status().is_success() => {
                    Some(inicio.elapsed().as_secs_f64() * 1000.0)
                }
                _ => None,
            };
            medidas.push(Measurement { addr: base.clone(), rtt_ms: rtt });
        }
        let atual = self.resolve(node_id);
        if let Some(escolha) = choose_route(&medidas, atual.as_deref()) {
            if let Ok(mut t) = self.por_no.write() {
                t.insert(node_id.to_string(), Entrada { escolha, medido_em: Instant::now() });
            }
        } else if let Ok(mut t) = self.por_no.write() {
            t.remove(node_id);
        }
    }
}
```

Em `Network`, acrescente o campo `pub routes: Arc<crate::network::route::RouteTable>`, inicializado em `Network::new` com `Arc::new(RouteTable::new())`.

A medição vai numa task **própria**, nunca dentro do laço de gossip. Com 9 peers
e até 4 candidatos cada, um timeout de 3 s por sonda bloquearia o tique de
gossip por mais de um minuto - e é esse laço que anuncia o nó e expira peers
mortos. O cluster se desmontaria justamente quando um peer ficasse lento, que é o
oposto do que esta mudança existe para fazer. Em `Network::start`:

```rust
        // Medicao de rotas, em task propria: uma sonda lenta nao pode segurar o
        // anuncio nem a limpeza de peers mortos.
        let net_routes = self.clone();
        tokio::spawn(async move {
            let http = reqwest::Client::new();
            let mut ticker = interval(Duration::from_secs(30));
            loop {
                ticker.tick().await;
                if net_routes.is_detached() {
                    continue;
                }
                for peer in net_routes.alive_peers().await {
                    if !net_routes.routes.precisa_medir(&peer.node_id) {
                        continue;
                    }
                    let candidatos = peer.inference_candidates();
                    if candidatos.is_empty() {
                        continue;
                    }
                    net_routes
                        .routes
                        .measure_peer(&http, &peer.node_id, &candidatos)
                        .await;
                }
            }
        });
```

O tique de 30 s é só a checagem; quem decide se mede de verdade é
`precisa_medir`, com o intervalo de 5 minutos da spec. No regime normal quase
todo tique não mede ninguém.

- [x] **Step 4: Run test to verify it passes**

Run: `cargo test -p node route::`
Expected: PASS, 10 testes.

- [x] **Step 5: Commit**

```bash
git add crates/node/src/network/route.rs crates/node/src/network/network.rs
git commit -m "feat(rota): medicao periodica por /health e tabela de rotas"
```

---

### Task A5: quem disca resolve, fixado por sessão

**Files:**
- Modify: `crates/node/src/routing/pipeline.rs:73-86`
- Modify: `crates/node/src/inference/prefill.rs:531-575`

**Interfaces:**
- Consumes: `RouteTable::resolve`, `PeerInfo::inference_candidates`.
- Produces: `PipelineStep.inference_candidates: Vec<String>`, `pub fn resolve_step_url(step: &PipelineStep, tabela: &RouteTable, fixadas: &mut HashMap<String, String>) -> String`

- [x] **Step 1: Write the failing test**

Em `pipeline.rs`, no `mod tests`:

```rust
    fn step_com(candidatos: Vec<&str>, url: &str) -> PipelineStep {
        PipelineStep {
            node_id: "n1".into(),
            inference_url: url.into(),
            inference_candidates: candidatos.into_iter().map(String::from).collect(),
            start_layer: 0,
            end_layer: 9,
            inference_frame_capabilities: Vec::new(),
        }
    }

    /// Um step sem candidatos e um no rodando binario anterior. Cair no
    /// `inference_url` e o que impede um cluster de versoes misturadas de
    /// partir ao meio.
    #[test]
    fn a_step_without_candidates_falls_back_to_the_resolved_url() {
        let tabela = crate::network::route::RouteTable::new();
        let mut fixadas = std::collections::HashMap::new();
        let step = step_com(vec![], "http://192.168.1.9:8181");
        assert_eq!(
            resolve_step_url(&step, &tabela, &mut fixadas),
            "http://192.168.1.9:8181",
        );
    }

    /// A escolha da sessao nao muda no meio: o cache KV do outro lado e por
    /// sessao, e trocar de endereco na metade e pior que a rota lenta.
    #[test]
    fn the_route_is_pinned_for_the_whole_session() {
        let tabela = crate::network::route::RouteTable::new();
        let mut fixadas = std::collections::HashMap::new();
        fixadas.insert("n1".to_string(), "http://escolhido:8181".to_string());
        let step = step_com(vec!["http://outro:8181"], "http://legado:8181");
        assert_eq!(
            resolve_step_url(&step, &tabela, &mut fixadas),
            "http://escolhido:8181",
        );
    }

    /// Sem medicao para aquele no, o primeiro candidato serve - e melhor que o
    /// `inference_url`, que foi resolvido por outra maquina.
    #[test]
    fn without_a_measurement_the_first_candidate_is_used() {
        let tabela = crate::network::route::RouteTable::new();
        let mut fixadas = std::collections::HashMap::new();
        let step = step_com(vec!["http://a:8181", "http://b:8181"], "http://legado:8181");
        assert_eq!(resolve_step_url(&step, &tabela, &mut fixadas), "http://a:8181");
    }
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test -p node pipeline::tests::a_step_without`
Expected: FAIL, `no field inference_candidates on type PipelineStep`.

- [x] **Step 3: Write minimal implementation**

Em `PipelineStep`, depois de `inference_url`:

```rust
    /// Todos os enderecos por onde este hop pode ser alcancado.
    ///
    /// `inference_url` continua preenchido e e o que um no antigo le. Quem
    /// disca prefere esta lista, porque a escolha tem que ser de quem mediu: o
    /// plano pode ter sido montado por uma maquina noutra rede.
    #[serde(default)]
    pub inference_candidates: Vec<String>,
```

Em `plan_pipeline`, ao montar cada step, acrescente
`inference_candidates: chosen.inference_candidates(),`.

Ainda em `pipeline.rs`:

```rust
/// Endereco por onde falar com este hop, decidido por quem vai discar.
///
/// `fixadas` e a memoria da sessao: uma vez escolhido, o endereco daquele no
/// nao muda ate a sessao acabar.
pub fn resolve_step_url(
    step: &PipelineStep,
    tabela: &crate::network::route::RouteTable,
    fixadas: &mut std::collections::HashMap<String, String>,
) -> String {
    if let Some(ja) = fixadas.get(&step.node_id) {
        return ja.clone();
    }
    let escolhido = tabela
        .resolve(&step.node_id)
        .filter(|url| step.inference_candidates.iter().any(|c| c == url))
        .or_else(|| step.inference_candidates.first().cloned())
        .unwrap_or_else(|| step.inference_url.clone());
    fixadas.insert(step.node_id.clone(), escolhido.clone());
    escolhido
}
```

Em `prefill.rs`, em `run_ring` e em `run`, substitua os usos de
`head.inference_url` por `resolve_step_url(head, tabela, &mut fixadas)`, com
`let mut fixadas = std::collections::HashMap::new();` antes do laço e a tabela
recebida por parâmetro a partir do `GatewayState`. Na falha de um salto,
chame `tabela.mark_failure(&head.node_id, &url_usada)` antes de propagar o erro.

- [x] **Step 4: Run test to verify it passes**

Run: `cargo test -p node`
Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add crates/node/src/routing/pipeline.rs crates/node/src/inference/prefill.rs
git commit -m "feat(rota): quem disca resolve o endereco, fixado por sessao"
```

---

### Task A6: rota na interface

**Files:**
- Modify: `crates/node/src/backend/webui.rs:387-400,618-660`
- Modify: `apps/frontend/src/lib/types.ts:163-183`
- Modify: `apps/frontend/src/routes/Dashboard.tsx`
- Modify: `apps/frontend/src/i18n/locales.ts`

**Interfaces:**
- Consumes: `RouteTable::snapshot`.
- Produces: campo `route: { addr: string, rtt_ms: number } | null` em `PeerView`; chaves `dashboard.route` e `dashboard.routeUnmeasured`.

- [x] **Step 1: Write the failing test**

Em `webui.rs`, no `mod tests`:

```rust
    /// A rota escolhida tem que chegar a interface: sem ela, "por onde este
    /// salto vai" so se responde com shell na maquina.
    #[test]
    fn the_cluster_view_carries_the_chosen_route() {
        let tabela = crate::network::route::RouteTable::new();
        let vista = route_view_for("n1", &tabela);
        assert!(vista.is_none(), "peer nao medido nao inventa rota");
    }
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test -p node webui::tests::the_cluster_view`
Expected: FAIL, `cannot find function route_view_for`.

- [x] **Step 3: Write minimal implementation**

Em `webui.rs`:

```rust
#[derive(Serialize)]
pub struct RouteView {
    pub addr: String,
    pub rtt_ms: f64,
}

/// Rota escolhida para este peer, ou `None` quando ainda nao foi medida.
///
/// `None` e informacao: significa "ainda nao sei", e a tela diz isso em vez de
/// mostrar um endereco que ninguem confirmou.
pub fn route_view_for(
    node_id: &str,
    tabela: &crate::network::route::RouteTable,
) -> Option<RouteView> {
    tabela
        .snapshot()
        .into_iter()
        .find(|(id, _)| id == node_id)
        .map(|(_, escolha)| RouteView { addr: escolha.addr, rtt_ms: escolha.rtt_ms })
}
```

Inclua `"route": route_view_for(&peer.node_id, &state.network.routes)` em cada
peer do JSON de `/api/ui/cluster`.

Em `types.ts`, dentro de `PeerView`:

```ts
  /** Endereço escolhido para falar com este peer, e a latência medida. */
  route: { addr: string; rtt_ms: number } | null;
```

Em `Dashboard.tsx`, logo depois do `KeyValue` de `dashboard.build`:

```tsx
            <KeyValue label={t("dashboard.route")}>
              {peer.route ? (
                <span className="font-mono text-xs">
                  {peer.route.addr} · {Math.round(peer.route.rtt_ms)} ms
                </span>
              ) : (
                <span className="text-muted">{t("dashboard.routeUnmeasured")}</span>
              )}
            </KeyValue>
```

Em `locales.ts`, junto de `"dashboard.build"`:

```ts
    "dashboard.route": () => "Route",
    "dashboard.routeUnmeasured": () => "not measured yet",
```

e no bloco português:

```ts
    "dashboard.route": () => "Rota",
    "dashboard.routeUnmeasured": () => "ainda não medida",
```

- [x] **Step 4: Run test to verify it passes**

Run: `cargo test -p node && cd apps/frontend && npm run build`
Expected: PASS e build da interface sem erro de tipo.

- [x] **Step 5: Commit**

```bash
git add crates/node/src/backend/webui.rs apps/frontend/src
git commit -m "feat(rota): rota escolhida e latencia no card de cada no"
```

---

# Fase B: implantação incremental

### Task B1: adicionar um nó, permitindo faixa sobreposta

**Files:**
- Modify: `crates/node/src/backend/deploy.rs:65-69`

**Interfaces:**
- Consumes: `SelectedSlice`, `RuntimeParams`.
- Produces: rota `POST /api/ui/deploy/node`, `pub struct AddNodeRequest { pub group: String, pub target_node_id: String, pub start_layer: u32, pub end_layer: u32, pub params: RuntimeParams }`, `pub fn overlapping_layers(existentes: &[(u32, u32)], nova: (u32, u32)) -> Vec<u32>`

- [x] **Step 1: Write the failing test**

Em `deploy.rs`, no `mod tests`:

```rust
    /// Faixa sobreposta no cluster deixa de ser erro: e assim que se soma uma
    /// rota paralela. A invariante "uma camada, um no" continua valendo, mas
    /// dentro de um plano, e quem a garante e `plan_pipeline`, que varre camada
    /// a camada escolhendo um peer por posicao.
    #[test]
    fn overlapping_ranges_are_reported_but_not_refused() {
        let sobrepostas = overlapping_layers(&[(0, 19), (20, 39)], (0, 19));
        assert_eq!(sobrepostas, (0..=19).collect::<Vec<u32>>());
    }

    #[test]
    fn a_range_that_fills_a_gap_overlaps_nothing() {
        assert!(overlapping_layers(&[(0, 19)], (20, 39)).is_empty());
    }

    #[test]
    fn a_partial_overlap_reports_only_the_shared_layers() {
        assert_eq!(overlapping_layers(&[(0, 19)], (15, 25)), vec![15, 16, 17, 18, 19]);
    }
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test -p node deploy::tests::overlapping`
Expected: FAIL, `cannot find function overlapping_layers`.

- [x] **Step 3: Write minimal implementation**

Em `deploy.rs`:

```rust
/// Camadas que a faixa nova compartilha com as que ja estao no ar.
///
/// Nao e recusa: e o que a tela mostra ao operador para ele saber que esta
/// criando uma rota paralela em vez de preencher um buraco.
pub fn overlapping_layers(existentes: &[(u32, u32)], nova: (u32, u32)) -> Vec<u32> {
    (nova.0..=nova.1)
        .filter(|c| existentes.iter().any(|(i, f)| c >= i && c <= f))
        .collect()
}

#[derive(Debug, Deserialize)]
pub struct AddNodeRequest {
    pub group: String,
    pub target_node_id: String,
    pub start_layer: u32,
    pub end_layer: u32,
    #[serde(default)]
    pub params: RuntimeParams,
}
```

Acrescente a rota em `routes()`:

```rust
        .route("/api/ui/deploy/node", post(add_node_endpoint))
```

O handler aplica **somente** no nó alvo, reusando o caminho de `apply` que o
`deploy_endpoint` já usa por máquina, e devolve
`{"overlapping_layers": [...], "applied": true}`. Não chame o `launch` que para
a inferência das outras máquinas: derrubaria quem já está servindo, que é
exatamente o que este caminho existe para evitar.

- [x] **Step 4: Run test to verify it passes**

Run: `cargo test -p node deploy::`
Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add crates/node/src/backend/deploy.rs
git commit -m "feat(implantacao): adicionar um no sem tocar nos demais"
```

---

### Task B2: remover um nó, distinguindo réplica de buraco

**Files:**
- Modify: `crates/node/src/backend/deploy.rs`

**Interfaces:**
- Consumes: `overlapping_layers`.
- Produces: rota `POST /api/ui/deploy/node/remove`, `pub fn uncovered_after_removal(restantes: &[(u32, u32)], removida: (u32, u32)) -> Vec<u32>`

- [x] **Step 1: Write the failing test**

```rust
    /// Tirar uma replica nao descobre camada nenhuma: a rede so perde uma rota
    /// paralela. Avisar de buraco aqui seria mentira.
    #[test]
    fn removing_a_replica_uncovers_nothing() {
        assert!(uncovered_after_removal(&[(0, 19), (20, 39)], (0, 19)).is_empty());
    }

    /// Tirar a ultima daquela faixa descobre as camadas dela, e o pipeline para
    /// de planejar ate alguem assumir.
    #[test]
    fn removing_the_last_holder_uncovers_its_layers() {
        assert_eq!(
            uncovered_after_removal(&[(20, 39)], (0, 19)),
            (0..=19).collect::<Vec<u32>>(),
        );
    }

    #[test]
    fn a_partially_replicated_range_uncovers_only_the_rest() {
        assert_eq!(uncovered_after_removal(&[(0, 9), (20, 39)], (0, 19)), (10..=19).collect::<Vec<u32>>());
    }
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test -p node deploy::tests::removing_a_replica`
Expected: FAIL, `cannot find function uncovered_after_removal`.

- [x] **Step 3: Write minimal implementation**

```rust
/// Camadas que ficam sem ninguem depois de tirar esta maquina.
///
/// Vazio significa que outra maquina cobre a mesma faixa - a rede perde uma
/// rota paralela e segue servindo, e o aviso ao operador e ameno. Nao vazio
/// significa buraco, e ai a confirmacao e explicita.
pub fn uncovered_after_removal(restantes: &[(u32, u32)], removida: (u32, u32)) -> Vec<u32> {
    (removida.0..=removida.1)
        .filter(|c| !restantes.iter().any(|(i, f)| c >= i && c <= f))
        .collect()
}
```

Acrescente a rota em `routes()`:

```rust
        .route("/api/ui/deploy/node/remove", post(remove_node_endpoint))
```

O handler encaminha ao nó alvo o mesmo caminho que `ReleaseSlice` já usa (quem
libera a memória é o dono dela) e devolve `{"uncovered_layers": [...]}`.

- [x] **Step 4: Run test to verify it passes**

Run: `cargo test -p node deploy::`
Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add crates/node/src/backend/deploy.rs
git commit -m "feat(implantacao): remover no distinguindo replica de buraco"
```

---

### Task B3: botões Adicionar e Remover

**Files:**
- Create: `apps/frontend/src/components/AddNodeDialog.tsx`
- Modify: `apps/frontend/src/routes/Dashboard.tsx`
- Modify: `apps/frontend/src/lib/api.ts`
- Modify: `apps/frontend/src/i18n/locales.ts`

**Interfaces:**
- Consumes: `POST /api/ui/deploy/node`, `POST /api/ui/deploy/node/remove`.
- Produces: `addNode(req)`, `removeNode(nodeId, group)` em `api.ts`.

- [x] **Step 1: Write the failing test**

Crie `apps/frontend/src/components/AddNodeDialog.test.tsx`:

```tsx
import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { AddNodeDialog } from "./AddNodeDialog";

describe("AddNodeDialog", () => {
  it("avisa que a faixa escolhida cria uma rota paralela", () => {
    render(
      <AddNodeDialog
        open
        peers={[{ node_id: "n2", short_id: "n2", hostname: "salem" }]}
        slices={[{ start_layer: 0, end_layer: 19, label: "0-19" }]}
        existingRanges={[[0, 19]]}
        onClose={() => {}}
      />,
    );
    expect(screen.getByText(/rota paralela/i)).toBeInTheDocument();
  });
});
```

- [x] **Step 2: Run test to verify it fails**

Run: `cd apps/frontend && npx vitest run src/components/AddNodeDialog.test.tsx`
Expected: FAIL, módulo `./AddNodeDialog` não existe.

- [x] **Step 3: Write minimal implementation**

`AddNodeDialog.tsx` monta três campos (nó, fatia, botão Implantar) e, quando a
faixa escolhida se sobrepõe a `existingRanges`, mostra o aviso
`t("deploy.parallelRoute")` - texto afirmativo, porque sobreposição aqui é o
objetivo, não erro. Reuse `Confirm.tsx` para o diálogo e `Button` de `ui.tsx`.

No card de `Dashboard.tsx`, ao lado de `ReleaseSlice`, acrescente o botão
Remover, que abre `Confirm` com a lista de camadas que ficarão descobertas
(vinda da resposta de `/api/ui/deploy/node/remove` em modo prévia) ou com o
aviso ameno quando a lista vier vazia.

Chaves novas em `locales.ts` (inglês e português):

```ts
    "deploy.addNode": () => "Add",
    "deploy.parallelRoute": () => "This range is already served. The machine becomes a parallel route.",
    "deploy.removeNode": () => "Remove",
    "deploy.removeLeavesGap": ({ layers }) => "Layers " + String(layers) + " are left uncovered and the pipeline stops planning until someone takes them.",
    "deploy.removeReplica": () => "Another machine serves this range. The network loses one parallel route and keeps serving.",
```

```ts
    "deploy.addNode": () => "Adicionar",
    "deploy.parallelRoute": () => "Esta faixa já é servida. A máquina entra como rota paralela.",
    "deploy.removeNode": () => "Remover",
    "deploy.removeLeavesGap": ({ layers }) => "As camadas " + String(layers) + " ficam descobertas e o pipeline para de planejar até alguém assumir.",
    "deploy.removeReplica": () => "Outra máquina serve esta faixa. A rede perde uma rota paralela e continua servindo.",
```

- [x] **Step 4: Run test to verify it passes**

Run: `cd apps/frontend && npx vitest run && npm run build`
Expected: PASS e build sem erro.

- [x] **Step 5: Commit**

```bash
git add apps/frontend/src
git commit -m "feat(implantacao): botoes de adicionar e remover no painel"
```

---

### Task B4: três estados na linha do canvas

**Files:**
- Modify: `apps/frontend/src/components/ModelNetworkCanvas.tsx:145-158`
- Modify: `apps/frontend/src/i18n/locales.ts`

**Interfaces:**
- Consumes: `routing_edges` (rota principal) e `candidate_edges` (paralelas), já existentes em `ClusterView`.

- [x] **Step 1: Write the failing test**

Crie `apps/frontend/src/components/ModelNetworkCanvas.test.tsx`:

```tsx
import { describe, expect, it } from "vitest";
import { edgeStyle } from "./ModelNetworkCanvas";

describe("edgeStyle", () => {
  it("pinta a rota principal de verde, sólida e com seta", () => {
    const s = edgeStyle("principal");
    expect(s.className).toContain("stroke-aqua");
    expect(s.strokeDasharray).toBeUndefined();
    expect(s.markerEnd).toBeTruthy();
  });

  it("pinta rota paralela de azul, sólida e sem seta", () => {
    const s = edgeStyle("paralela");
    expect(s.className).toContain("stroke-iris");
    expect(s.strokeDasharray).toBeUndefined();
    expect(s.markerEnd).toBeUndefined();
  });

  it("pinta rota inativa de cinza tracejado", () => {
    const s = edgeStyle("inativa");
    expect(s.className).toContain("stroke-faint");
    expect(s.strokeDasharray).toBe("7 7");
  });
});
```

- [x] **Step 2: Run test to verify it fails**

Run: `cd apps/frontend && npx vitest run src/components/ModelNetworkCanvas.test.tsx`
Expected: FAIL, `edgeStyle` não é exportado.

- [x] **Step 3: Write minimal implementation**

Em `ModelNetworkCanvas.tsx`:

```tsx
export type EdgeState = "principal" | "paralela" | "inativa";

/** Três estados, porque com réplicas "escolhida ou não" deixou de bastar:
 *  uma aresta pode não estar em uso e ainda assim ser um caminho pronto. */
export function edgeStyle(state: EdgeState) {
  if (state === "principal") {
    return { className: "fill-none stroke-aqua", strokeWidth: "3", strokeDasharray: undefined, markerEnd: "url(#route-arrow)" };
  }
  if (state === "paralela") {
    return { className: "fill-none stroke-iris", strokeWidth: "2", strokeDasharray: undefined, markerEnd: undefined };
  }
  return { className: "fill-none stroke-faint/70", strokeWidth: "2", strokeDasharray: "7 7", markerEnd: undefined };
}
```

Use `edgeStyle` no `<path>` do `edges.map`, decidindo o estado por: presente em
`routing_edges` → `"principal"`; presente em `candidate_edges` → `"paralela"`;
caso contrário `"inativa"`. Troque a seta do `marker` para `fill-aqua` e ajuste
a legenda para três itens, com as chaves `canvas.legendPrimary`,
`canvas.legendParallel` e `canvas.legendInactive`.

- [x] **Step 4: Run test to verify it passes**

Run: `cd apps/frontend && npx vitest run && npm run build`
Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add apps/frontend/src
git commit -m "feat(painel): rota principal, paralelas e inativas na malha"
```

---

## Verificação final

- [x] `cargo build --release` termina sem erro.
- [x] `cargo test --release` passa; o total sobe dos 243 atuais para ~268.
- [x] `cd apps/frontend && npm run build` termina sem erro de tipo.
- [ ] Num cluster de dois nós em redes diferentes, `/api/ui/cluster` mostra
      `route` preenchida para cada peer, e uma requisição completa nas **duas**
      ordens de pipeline - o caso que falhava em 2026-08-13.
