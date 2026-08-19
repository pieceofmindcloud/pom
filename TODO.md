# TODO - defeitos anotados nos AGENTS.md

Levantamento de 2026-08-15, extraído das seções `## Defeitos anotados` de todos
os `AGENTS.md` do repositório. Última limpeza: 2026-08-17.

Cada item traz o arquivo onde o defeito está anotado, a evidência no código e o
critério de pronto.

**Regra de manutenção:** ao corrigir um item, remova a anotação do
`AGENTS.md` correspondente **e** o item daqui, no mesmo commit. Um defeito
corrigido que continua anotado é pior que um defeito não anotado.

## Estado

Dos 29 itens triados pelo captain em 2026-08-15, **23 foram corrigidos e saíram
desta lista**. Ficam os 4 adiados e os 2 recusados (registrados abaixo só para
não voltarem a ser propostos).

## Adiados - reconhecidos, fora da rodada atual

- [ ] **`undeploy_authority` permite qualquer papel derrubar a pipeline inteira**
  `crates/node/src/backend/deploy.rs:1794` · anotado em `crates/node/src/backend/AGENTS.md`
  Retorna `Ok(())` para todo papel. Está aberto de propósito e o teste
  `any_node_may_undeploy_for_now` (`deploy.rs:2242`) documenta a porta aberta, então fechá-la é
  uma edição consciente: trocar o corpo da função pela checagem Principal-only que o doc
  comment já descreve, e atualizar o teste. A rota ainda exige a connection key
  (`ConnectionAuth`), então é aberta ao operador, não anônima.
  **Decisão do captain antes de mexer:** fechar agora para Principal-only, ou manter aberto.

- [ ] **`routing-base::plan` não é chamado em lugar nenhum**
  anotado em `crates/AGENTS.md`, `crates/routing-base/AGENTS.md` e `docs/subsystems/build-execucao.md`
  O planejador do `node` (`crates/node/src/routing/pipeline.rs::plan_pipeline`) monta um
  `RoutingRequest` a partir dos tipos do contrato para validar, mas faz seu próprio
  planejamento guloso com pontuação de carga. `routing-base` é código morto, vivo apenas pelos
  próprios testes de unidade, e os dois planejadores podem divergir sem nenhum teste comparando.
  **Depende de:** a decisão de open-core aberta com o captain (licença e fronteira de entrega do Pro).

- [ ] **`crates/types` é um marcador vazio**
  anotado em `crates/types/AGENTS.md`
  O único "tipo" é `WorkspaceMarker`. Tipos de domínio de verdade que o `node` ainda detém
  (`PeerInfo`, `NodeConfig`) não foram movidos.
  **Depende de:** a mesma decisão de arquitetura modular.

- [x] **`bitswap` é protocolo órfão** — resolvido saindo do swarm.
  O behaviour e o arquivo `network/bitswap.rs` foram removidos. O papel que ele
  ia cumprir (endereçamento por conteúdo) é do `.pom` + `GET /api/transfer/*?sha=`,
  sobre o mesmo HTTP Range que já carregava as fatias.

## Recusados - decisão tomada, não voltar a propor

- **O par do `docker-compose`** (`docker-compose.yml` montando fixtures que
  `scripts/gen-node-configs.py` não escreve, e o `.dockerignore` cortando
  `pom.json`). **Não é defeito**: são dois ambientes de gente diferente -
  o gerador serve a máquina do captain, o compose serve a do salem - e os dois
  esquemas de nome não precisam se encontrar. Registrado em `scripts/AGENTS.md`.

## Dívida de documentação em aberto

- O grafo do workspace diverge da arquitetura declarada - a documentação descreve o `node`
  dependendo de `config` e `types`, e o código não. Não é defeito de comportamento; se resolve
  junto com a extração dos crates. Anotado em `crates/AGENTS.md`.
- O contrato diz que `end_layer` é inclusivo; uma versão futura de fio com "fim exclusivo" está
  sinalizada em comentário mas não implementada. Anotado em `crates/routing-contract/AGENTS.md`.

## Defeitos anotados depois desta triagem

Estes não fizeram parte da rodada de 2026-08-15 e continuam anotados em
`crates/node/src/network/AGENTS.md`:

- [ ] **O `receive_loop` não tem isolamento de pânico** - um pânico em
  `handle_message` mata a task e o nó para de receber UDP para sempre, enquanto
  HTTP, gossip e TUI seguem saudáveis. Aconteceu de verdade em 2026-08-16.
- [ ] **A suíte de testes transmite na LAN de verdade** - `broadcast_local` manda
  para `255.255.255.255:42777` qualquer que seja a porta do nó. Rodar
  `cargo test` ao lado de um cluster vivo injeta mensagens de teste nele.
- [ ] **O endereço observado troca a cada `identify`** e não há preferência entre
  eles; num Mac com vários IPv6 de privacidade o `Announce` alterna. Quem precisa
  de endereço estável fixa em `--advertise-addr`.
- [ ] **Um hop inalcançável custa o timeout de `connect` do SO antes do túnel** -
  75 s medidos no macOS. É decisão de produto sobre o prazo do HTTP direto.
