# Rede de três máquinas: o botão que não habilitava, o gossipsub que não falava, e o túnel que ninguém tinha provado

Data: 2026-08-16. Branch `fm/rdma-rede-3-maquinas`, sobre `f8eca5f`.

Ambiente: o cluster **de verdade** do captain, no ar durante todo o trabalho -
MacBook (LAN), `ksia` (LAN, VM Proxmox) e `salem-gpu`/`salem-cpu` (mesma máquina
física do outro lado da internet, duas identidades de nó). Doze nós no total,
contando as identidades extras (`ksia-2..5`, `salem-gpu2`, `salem-cpu2`).

Nada foi alterado nas máquinas do captain. Toda medição foi feita com **nós
sonda próprios** (`probe-fm`, `probe-fm-2`), subidos com `RDMA_P2P_HOME` no
diretório temporário desta sessão, sem modelo e sem tocar nas fatias do
cluster - o que permitiu comparar o binário `f8eca5f` e o binário corrigido no
mesmo cluster, com a mesma identidade de nó, no mesmo minuto.

---

## Defeito 1 - o botão de implantar não habilita e a tela não diz por quê

### Reprodução, na máquina do captain

Na tela do nó dele (`http://localhost:8080/implantar`, binário e interface
`f8eca5f`), com o conjunto `lmstudio-community-Qwen3.6-35B-A3B-GGUF`:

1. ligar "Merge neighboring shards on the same machine";
2. dar a mesma máquina (`salem-cpu`) para L0-7 e para L16-23.

Resultado medido no DOM: `Deploy this way` com `disabled: true`, e **nenhum
texto de explicação em toda a `<main>`** - os únicos textos que casam com
"cannot / blocked / não / vizinhas" são as dicas de outros controles.

O mesmo estado, no nó com a correção (`http://localhost:8099/implantar`):

```
This selection cannot be deployed
as fatias de ksia-2 não são vizinhas (L0-7 e L16-23) - só dá para juntar fatias adjacentes
[ao lado do botão] see the reason above
```

### Causa raiz - são dois defeitos, um em cada lado

**(a) O nó devolve `steps` fora da ordem de camadas.** `deploy::plan` agrupa as
faixas escolhidas **por máquina de destino** e só então emite os passos. Uma
máquina com duas faixas emite a segunda logo depois da primeira, na frente de
tudo que está no meio. Medido contra o cluster real, com a seleção de cinco
faixas e `ksia` recebendo L0-7 e L16-23:

```
$ curl -s -H "Authorization: Bearer $KEY" -d @sel2.json localhost:8080/api/ui/deploy/plan
blocked: ksia ficou com duas faixas (L0-7 e L16-23) - junte as fatias vizinhas...
STEPS (ordem devolvida):
  1  0 - 7  -> ksia
  2 16 - 23 -> ksia        <-- fora de ordem
  3  8 - 15 -> salem-cpu2
  4 24 - 31 -> Jairs-MacBook-Pro.local
  5 32 - 39 -> ksia-3
```

Há um segundo caso, pior, porque atinge o plano automático: `target_node_id`
vazio agrupa por igualdade de string como qualquer outro, então **toda faixa sem
máquina virava "a mesma máquina"**. Com duas faixas sem destino a ordem
quebrava, e a recusa saía nomeando uma máquina sem nome (`" ficou com duas
faixas (L8-15 e L24-31)"`, com o espaço no lugar do nome).

**(b) A tela emudece exatamente por causa disso.** Em
`apps/frontend/src/routes/DeployModel.tsx`:

```ts
const rows = (selection ?? rowsOf(p)).slice().sort(byLayer);   // ordenado por camada
const validated = !!p && sameRows(rows, rowsOf(p));            // comparado com o plano CRU
const canDeploy = validated && !p?.blocked && rows.length > 0;
```

`sameRows` compara posição a posição. As linhas da tela estão ordenadas por
camada; `rowsOf(p)` sai na ordem do plano. Divergiram, `validated` = falso - e
os quatro painéis de explicação (recusa, cobertura incompleta, total deduzido,
avisos de memória) **também renderizavam sob `validated`**. Ou seja: no único
estado em que o operador precisa do motivo, a tela some com ele e deixa só o
botão cinza.

Note que `validated` fica falso mesmo **sem o operador editar nada**: com
`selection === null`, `rows = rowsOf(p)` **ordenado** e a comparação é contra a
lista crua. Um plano automático fora de ordem - que é o que acontece quando o
planejador fica sem máquinas e sobra mais de uma faixa sem destino - cala a tela
sozinho, e o motivo suprimido é justamente `"são N fatias para M máquinas - um
nó serve uma faixa por vez"`.

### Correção

Lado do nó (`crates/node/src/backend/deploy.rs`):

- `steps` é ordenado por `(start_layer, end_layer)` uma vez no fim, e `position`
  é numerado **depois** disso. Não é cosmético: `position` diz "lugar no
  pipeline", a ordem do pipeline é a ordem das camadas, e `run` lê a ordem de
  lançamento direto dessa lista (`LaunchTarget` com `peer_index: None` deduz o
  índice da posição no vetor).
- faixa com `target_node_id` vazio nunca agrupa com outra - cada uma é seu
  próprio passo, com seu próprio `problem` ("a fatia L{}-{} está sem máquina").

Lado da tela (`DeployModel.tsx`):

- `sameRows` compara os dois lados **ordenados por camada**;
- os painéis de explicação passaram a renderizar sob `answered` (o nó respondeu
  sobre *estas* linhas: mesmo grupo, sem `isPlaceholderData`, sem edição a
  caminho), nunca sob `validated`;
- `whyNot` põe a razão em uma linha ao lado do botão, cobrindo também os dois
  estados que não têm painel nenhum: nada selecionado e resposta a caminho.

### Testes de regressão

Rust (`deploy.rs`), os três falhavam antes:

- `steps_stay_in_layer_order_when_a_machine_takes_two_ranges`
- `ranges_left_without_a_machine_are_not_one_machine`
- `two_hand_picked_ranges_without_a_machine_keep_their_own_rows`

Frontend (`DeployModel.test.tsx`), os três falhavam antes:

- explica a recusa mesmo quando o nó devolve os passos fora da ordem de camadas
- libera o botão quando o plano descreve as mesmas linhas em outra ordem
- nomeia cada faixa sem máquina em vez de calar

Verificado que os três frontend falham sem a correção (`git checkout` do
`DeployModel.tsx`, `npm test`: 3 failed) e passam com ela.

### O que isso significa para o captain

Com a correção, a implantação das três máquinas volta a ser possível **e** o
caminho fica visível: o plano automático deste cluster já fecha 0-39 sozinho
(cinco faixas em cinco nós, `blocked: None`, `complete: true`); quando a escolha
dele não fecha, a tela agora diz qual é a regra violada e em qual máquina.

---

## Defeito 2 - o gossipsub parou de carregar anúncio (e não era payload repetido)

### Reprodução

Nó sonda com o binário `f8eca5f`, entrando no cluster real:

```
2026-08-16T13:16:25 WARN Not publishing a message that has already been published message_id=31355230
2026-08-16T13:16:30 WARN Not publishing a message that has already been published message_id=31355230
… a cada 5 s, sempre o mesmo id
```

59 avisos em 296 s de janela - ou seja, **todo publish depois do primeiro** foi
recusado, e a espera pelo TTL de 60 s do cache nunca ajudou.

### Causa raiz

Não é o payload. O `Announce` carrega `timestamp: Utc::now()`
(`Network::self_announce`), então dois anúncios consecutivos **são** diferentes
em bytes. O que é constante é o **id**.

`Discovery::new` monta o gossipsub com `MessageAuthenticity::Anonymous` e não
define `message_id_fn`. O padrão do libp2p é `source + sequence_number`
(`libp2p-gossipsub-0.49.4/src/config.rs:518`) e as duas são `None` sob
autenticação anônima, então o id é a constante `"15R0"` - `31355230` em hexa,
exatamente o id do log do captain.

O estrago é dos dois lados do mesmo cache (`duplicate_cache`, 60 s):

- **publicar**: `Behaviour::publish` recusa com `PublishError::Duplicate`;
- **receber**: `handle_received_message` descarta como duplicata - e como o id
  não depende do conteúdo, o anúncio de *qualquer* peer é "duplicata" do de
  qualquer outro.

E como todo nó do cluster reinsere a mesma constante o tempo todo, a entrada
nunca expira de fato.

### Impacto real, medido

Nó sonda, cluster real, malha formada (5 peers grafados, tópico assinado),
`RUST_LOG=libp2p_gossipsub=debug`:

| binário | janela | mensagens aceitas | descartadas como duplicata | publishes recusados |
|---|---|---|---|---|
| `f8eca5f` | 377 s | **0** | 0 | 75 |
| corrigido | 2223 s | **444** | 39 | **0** |

444 mensagens em 2223 s = 444 tiques de 5 s: exatamente um anúncio por tique do
outro nó corrigido. As 39 descartadas mostram que a deduplicação continua
fazendo o trabalho de verdade dela (a mesma mensagem chegando por dois caminhos
da malha).

Zero mensagens em 377 s com a malha formada é a prova de que o tráfego de
anúncio do gossipsub **estava completamente parado no cluster inteiro**. O
cluster só funcionava porque o caminho UDP é independente.

### Correção

`discovery::announce_message_id`: id = hash do tópico + payload. É configuração
**local** (`message_id_fn` não viaja no protocolo), então um nó atualizado
interopera com um que ainda roda o padrão - o que permitiu justamente medir o
antes e o depois no mesmo cluster.

Testes: `the_libp2p_default_id_collapses_every_announce_into_one` (reproduz o id
`31355230` no lugar exato), `two_different_announces_get_two_different_ids`,
`the_same_announce_keeps_one_id`.

---

## Defeito 3 - verificar o que o `f8eca5f` declarou não verificado

### 3.1 Endereço anunciado

**Precedência conferida ao vivo, sem reiniciar nada**, lendo o que as *outras*
máquinas anunciam do nó sonda (`PeerRequest` por UDP contra o MacBook do captain
e contra a ksia):

| passo | o que a tela do nó diz | o que MacBook e ksia veem |
|---|---|---|
| sem nada fixado | `[2804:d55:…:99c3:77db:53b:cc08]:42799` (observado) | o mesmo, nas duas |
| fixado pela tela de Rede | `[2804:d55:…:1c20:e551:6a95:9a0d]:42799` | o mesmo em ~10 s, nas duas |
| campo limpo | volta ao observado | volta ao observado, nas duas |

Precedência #3 (interfaces locais) aparece em `addr_candidates`
(`192.168.100.240:42799`, `100.122.132.97:42799`, …), que é onde ela vive.

**Um defeito real encontrado aqui.** A primeira resposta de `identify` que o nó
recebeu veio de um peer alcançado pelo **tailnet**, e o nó adotou como endereço
*público*:

```
INFO 📡 Endereço público observado por um peer: 100.122.132.97:42799
```

`100.64.0.0/10` é a faixa RFC 6598 (CGNAT) - a que a Tailscale usa -, e
`Ipv4Addr::is_private` responde só RFC 1918, então `addrs::is_private_or_local`
a deixava passar. Um endereço de tailnet prova que o peer está no mesmo tailnet,
do mesmo jeito que `192.168.` prova que ele está na mesma LAN: quem está fora
não disca nenhum dos dois. Corrigido junto com as outras faixas que a checagem
tratava como endereços soltos em vez de faixas (`127/8` inteiro, `169.254/16`,
`0.0.0.0/8`, multicast, broadcast, `fe80::/10` inteiro, `::`). Depois da
correção, no mesmo cluster, só endereços IPv6 globais aparecem como observados.

Um segundo achado, anotado e **não** corrigido porque é decisão de produto:
`set_observed_addr` guarda o que o **último** `identify` disse, sem critério de
desempate. Num Mac com vários IPv6 de privacidade, peers diferentes observam
endereços diferentes e o anunciado alterna - visto na própria tela, mostrando
`…fc93:da35:2de:2926` e `…99c3:77db:53b:cc08` em leituras seguidas.
`filter_candidates` prefere o estável ao temporário na enumeração local; o
caminho do observado não tem essa preferência. Quem precisa de estabilidade fixa
o endereço, e isso funciona.

Terceiro achado, corrigido porque é barato e o datagrama tem orçamento de MTU: o
endereço fixado saía **duplicado** em `addr_candidates`, porque a deduplicação
comparava a forma colchetada (`effective_host`) com a forma crua da enumeração.

### 3.2 Fallback por túnel

**Ele dispara e funciona contra a `salem-gpu`, pela internet.** Duas sondas
`#[ignore]`, dirigidas por `RDMA_P2P_TUNNEL_PROBE`, ficam no repositório como
procedimento reproduzível:

`infer_tunnel::tests::tunnel_reaches_a_live_peer` - protocolo cru:

```
ConnectionEstablished peer=12D3KooWRuq6…(salem-gpu) established_in: 68.358ms
túnel respondeu: status=400 body="quadro inválido: quadro curto demais: 0 bytes"
```

O 400 é gerado **no servidor de inferência da salem** (não é o 502 sintético de
`serve_loopback`), então o quadro atravessou a internet, entrou no túnel, foi
repassado ao `127.0.0.1:8181` de lá e a resposta voltou.

Repetido contra a `salem-cpu` (mesma máquina física, mesmo IPv6, porta
diferente): `established_in: 73.673ms`, mesma resposta. **As duas identidades
não se confundem** - cada `PeerId` chega ao servidor de inferência da sua
própria porta.

`relay::tests::the_tunnel_carries_a_real_frame_to_a_live_peer` - a troca de
transporte inteira, pelo código de produção, com o HTTP direto apontado para
`192.0.2.1` (TEST-NET-1, RFC 5737, nunca roteável):

```
primeira perna (HTTP direto): [Timeout] em 75 s
o peer recusou pelo túnel (o que já prova o caminho): 415 Expected request with `Content-Type: application/json`
```

**Ele NÃO dispara na rede saudável.** Inferência distribuída de verdade pelas
três máquinas, com o nó sonda corrigido servindo de gateway, e zero ocorrência
de `tentando via túnel libp2p` / `túnel libp2p também falhou` no log.

Duas ressalvas honestas:

1. As máquinas se alcançam por **IPv6 global**. O que foi exercitado é "pela
   internet", não "atrás de um NAT que bloqueia" - no IPv6 deste cluster não há
   NAT para atravessar. Quebrar o HTTP direto contra a salem exigiria mexer nela
   ou na tabela de rotas do Mac do captain, e o brief proíbe a primeira; por
   isso a falha foi induzida com um endereço-buraco-negro, que é o mesmo modo de
   falha (`Timeout`) pelo mesmo gatilho.
2. **O desvio custa caro para começar**: 75 s medidos no macOS, porque
   `relay::send_frame` fixa 600 s por requisição e sobrepõe o prazo do cliente,
   deixando a conta com o `connect` do sistema. O túnel em si é rápido. Isso é
   decisão de produto sobre o prazo do HTTP direto, então ficou anotado e não
   mudado.

### 3.3 Não-regressão

Com o binário corrigido servindo de gateway, pelos dois pares que o captain tem
no ar:

| par | `source` | tokens | tok/s |
|---|---|---|---|
| Ornith 0-19 (salem-gpu) → 20-39 (ksia) | `pipeline:02eb8c40→5b035803` | 46 | **19,6** |
| qwen 0-15 (MacBook) → 16-31 (salem-cpu) | `pipeline:7d00220e→ff952586` | 44 | 6,6 |

O par de controle do captain marcava 17,9 tok/s; 19,6 tok/s é a mesma faixa. As
duas cadeias fecham com dois hops, que é a prova de execução conjunta.

---

## Defeito 4 (achado durante o trabalho) - id curto na rede mata o `receive_loop`

Não estava no brief: apareceu porque **derrubou o cluster do captain durante
esta sessão**, e a causa está no log do nó dele (`/tmp/rdma-mac.log`):

```
thread 'tokio-rt-worker' panicked at crates/node/src/network/network.rs:1024:33:
byte index 12 is out of bounds of `eu`
2026-08-16T14:05:18 INFO 🧹 Removed 10 timed-out peers
```

`&node_id[..12]` no tratamento de `PeerList` fatia por índice de byte um id que
veio **da rede**. Um id de dois caracteres entra em pânico, a task do
`receive_loop` morre, e o nó **para de receber UDP para sempre**. O que sobra é
o pior estado possível de diagnosticar: HTTP responde, o gossip continua
anunciando, a TUI mostra o nó pronto com o modelo carregado, e a única pista é
o socket com a fila de recepção cheia (medido: `Recv-Q` 786.835 bytes no
MacBook, 212.752 na ksia, congelados no teto).

De onde veio o id `"eu"`: é uma fixture de teste. `cargo test -p node` roda
testes que chamam `Network::announce` num nó **não** destacado, e
`broadcast_local` manda para `255.255.255.255:42777` **qualquer que seja a porta
do nó** - fato que o próprio `a_detached_node_neither_announces_nor_looks_for_bootstrap`
documenta. Rodar a suíte numa máquina da mesma LAN de um cluster vivo, portanto,
dispara o pânico em **todo nó na porta 42777 daquela LAN**.

A correlação bate exatamente: ficaram mudos o nó do MacBook (42777) e o nó
principal da ksia (42777) - os dois únicos que recebem broadcast da LAN. Os
`ksia-2..5` (portas 42778-42781) e os dois nós da salem (remotos) continuaram
saudáveis, com `/peers` e UDP respondendo normalmente.

**Corrigido**: `network::short()` corta com `len().min(12)` respeitando fronteira
de caractere, e é usado nos quatro pontos que fatiavam o id. Teste:
`a_short_node_id_from_the_wire_never_panics`.

**Não corrigido** (ver Pendências): a task do `receive_loop` não tem isolamento
de pânico nenhum, então o *próximo* pânico numa mensagem qualquer repete o mesmo
desfecho silencioso; e a suíte de testes continua podendo transmitir na LAN de
verdade.

## Estado dos testes

- `cargo test -p node`: **436 passed, 0 failed, 5 ignored** (as duas sondas
  novas entram nos ignorados, junto com as três que já existiam).
- `npm test` em `apps/frontend`: **88 passed, 20 files**.
- `cargo clippy -p node --all-targets`: 29 avisos, contra 30 no `f8eca5f` - as
  mudanças não acrescentam nenhum.

## Pendências

0. **AÇÃO NECESSÁRIA AGORA: o nó do MacBook e o nó principal da ksia estão com
   o `receive_loop` morto** pelo pânico acima e precisam ser **reiniciados** -
   só isso recupera a recepção UDP, e sem eles o cluster está sem pipeline
   (`/peers` de cada um mostra só ele mesmo). Não reiniciei porque são processos
   do captain, iniciados nos terminais dele:
   - MacBook (PID 89046): `cd /Users/jairpereira/Sites/rdma-p2p && ./target/release/rdma-p2p --refresh 3600 --advertise-addr '[2804:d55:5579:1000:1c20:e551:6a95:9a0d]:42777'`
     - a fatia volta sozinha: `rdma-p2p.json.last-slice.json` está em `serving`.
   - ksia (PID 2857383): **não há `last-slice.json`** em
     `/home/jairpereira/workspace/rdma-p2p/`, então a fatia Ornith 20-39 dele
     **não** volta sozinha e o par precisa ser reimplantado pela tela.
   Os outros nós (salem-gpu, salem-cpu, ksia-2..5) estão intactos.
1. **A suíte de testes transmite na LAN de verdade.** `broadcast_local` vai para
   `255.255.255.255:42777` qualquer que seja a porta, e há testes que chamam
   `announce` num nó não destacado. Rodar `cargo test` ao lado de um cluster
   vivo injeta mensagens de teste nele. A correção do pânico tira o efeito
   catastrófico, não a transmissão. Merece um guarda no harness (nó de teste
   sempre destacado, ou endereço de broadcast injetável).
2. **O `receive_loop` não tem isolamento de pânico.** Um pânico em
   `handle_message` mata a recepção UDP do processo inteiro, em silêncio, e nada
   na TUI, no `/health` ou no `/api/ui/node` denuncia. Um `catch_unwind` por
   mensagem, ou uma task supervisora que ressuscita o laço, transformaria um
   defeito silencioso e permanente num erro no log.
3. **`Pipeline plan failed: Gap at layer 16 for model 'qwen'` a cada 5 s** no log
   do MacBook: consequência da perda dos peers (com o peer da faixa 16-31 fora
   da tabela, o planejador não fecha). Anotado por completude; deve sumir com o
   reinício do item 0. Não investigado.
4. **O endereço observado não tem critério de desempate** (3.1). Precisa de
   decisão: preferir o estável ao temporário como a enumeração local faz, ou
   exigir concordância de mais de um peer antes de trocar. Anotado em
   `crates/node/src/network/AGENTS.md` e em `docs/subsystems/rede-p2p.md`.
5. **Os 75 s até o desvio pelo túnel** (3.2). `relay::send_frame` fixa 600 s por
   requisição; um hop atrás de NAT paga o timeout de `connect` do SO antes de o
   túnel ser tentado. Decisão de produto sobre esse prazo.
6. **Um NAT que realmente bloqueia** nunca foi exercitado, porque o cluster se
   alcança por IPv6 global. Só é possível com uma máquina que não tenha IPv6
   roteável.
7. **`ksia-2` a `ksia-5` não anunciam `libp2p_peer_id` nem
   `transport/libp2p-tunnel-v1`** (medido no `PeerRequest`): são exatamente os
   nós que nunca poderiam usar o túnel. Não investigado - não estava no escopo,
   mas é o tipo de coisa que só aparece quando o túnel é preciso.
8. **A mensagem de recusa do nó chega em pt-BR numa interface em inglês** e a
   tela mostra "The node returned details in an unsupported language" ao lado do
   texto original. Visto em toda recusa do plano; é cosmético e não foi mexido.

## Como reproduzir as medições

```sh
# Nó sonda, sem tocar no cluster do captain
mkdir -p /tmp/probe/models && ln -s <worktree>/apps /tmp/probe/apps
# rdma-p2p.json com gateway_port 8099, p2p_port 42799, bootstrap 127.0.0.1:42777
RDMA_P2P_HOME=/tmp/probe RUST_LOG=info,libp2p_gossipsub=debug \
  ./target/release/rdma-p2p --refresh 3600 > /tmp/probe/log 2>&1 &

grep -c "Not publishing" /tmp/probe/log              # publishes recusados
grep -c "Put message in duplicate_cache" /tmp/probe/log   # anúncios aceitos
grep -c "Adding peer to the mesh" /tmp/probe/log     # a malha existe mesmo

# Túnel contra um peer vivo (o multiaddr sai do libp2p_peer_id anunciado
# e do endereço do peer com a porta libp2p = porta UDP + 10000)
RDMA_P2P_TUNNEL_PROBE=/ip6/<ip>/tcp/<porta+10000>/p2p/<peer-id> \
  cargo test -p node --bin rdma-p2p tunnel_reaches_a_live_peer -- --ignored --nocapture
RDMA_P2P_TUNNEL_PROBE=… \
  cargo test -p node --bin rdma-p2p the_tunnel_carries_a_real_frame -- --ignored --nocapture
```
