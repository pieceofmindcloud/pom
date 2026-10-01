# TODO - defeitos anotados nos AGENTS.md

Lista de trabalho consolidada em 2026-09-07 a partir da auditoria das anotações
dos `AGENTS.md`, na revisão `985cfb51183c4fa1b509524e54d39aeda78b1cc8`.
A evidência completa de cada item está no relatório da auditoria que acompanha
esta tarefa. A lista atual mantém a numeração da auditoria: 17 defeitos ainda
existentes e 1 item não verificável nesta máquina. Itens acrescentados depois
da auditoria seguem a numeração a partir do 27.

## Regra de manutenção

Ao corrigir um item numerado atual, remova a anotação do `AGENTS.md`
correspondente e o item daqui no mesmo commit. Um defeito corrigido que
continua anotado é pior que um defeito não anotado. As notas em
`## Histórico preservado`, `## Recusados` e `## Dívida de documentação` são
contexto e não devem voltar a ser propostas como defeitos novos.

## Travessia de NAT (IPv4 sem endereço público)

Entregue em 2026-09-22, em seis partes. O diagnóstico e o desenho final estão
em `docs/subsystems/p2p-network.md`, seção "NAT traversal"; esta lista fica
como registro do que foi feito e não tem item aberto.

- [x] **N1 - Swarm capaz de travessia.** QUIC no `SwarmBuilder` e nos listen
  addresses; `DEFAULT_RELAY_NODES` esvaziado, porque bootstrapper do IPFS não
  concede reserva de Circuit Relay v2; `reserve_circuit` pede reserva a
  qualquer par que anuncie `relay::HOP_PROTOCOL_NAME`; `NatStatus::Public`
  chama `Swarm::add_external_address`.
- [x] **N2 - Publicar e discar multiaddresses reais.** `Announce`/`PeerInfo`
  levam `libp2p_addrs`, alimentado por `publish_swarm_addrs`;
  `libp2p_dialable_addrs` devolve todos; `tunnel_peer_multiaddrs` aceita
  multiaddress completo e `host:port` legado.
- [x] **N3 - Plano de controle pelo swarm.** `TunnelRequest.target` escolhe
  entre inferência e gateway; `gateway_path_is_allowed` é a lista fechada
  conferida no receptor, porque o que sai do túnel chega ao gateway por
  `127.0.0.1`. A sonda `/handshake` da admissão e as catracas de política caem
  no túnel depois de todos os endereços diretos falharem.
- [x] **N4 - Preferência aprendida pelo túnel.** `PeerClients::prefer_tunnel`
  liga com uma entrega do túnel depois de a escada direta falhar inteira e
  desliga no primeiro sucesso direto; ligada, o frame pula QUIC e HTTP na
  primeira tentativa.
- [x] **N5 - pkarr publica o endereço confirmado.** `public_libp2p_addr`
  vence o multiaddress derivado no arranque. Circuito fica fora: o registro é
  republicado a cada 30 min e a reserva não dura previsivelmente tanto.
- [x] **N0 - Observabilidade.** `TraversalStatus` em `/api/ui/cluster` e no
  painel "Travessia de NAT" da tela de Rede.

## Alta

- [ ] **12 - Qualquer papel pode desimplantar a pipeline**
  **Fonte:** `crates/node/src/backend/deploy.rs:6714-6716`.
  **Prática:** `undeploy_authority` retorna sucesso para qualquer papel
  autenticado, permitindo derrubar todas as fatias da implantação; a rota
  continua protegida por `ConnectionAuth`.
  **Afetados:** disponibilidade e controle operacional em clusters com papéis
  separados.
  **Esforço:** baixo para o gate, médio para revisar autorização, testes e UI.
  **Decisão do operador:** decidir se o desimplante deve continuar aberto a
  qualquer papel autenticado; não corrigir sem essa decisão.

- [ ] **13 - Rollback não é repassado ao TP**
  **Fonte:** `crates/node/src/inference/infer_server.rs:3092-3108,3951-3984`;
  fan-out em `crates/node/src/backend/gateway.rs:2733-2797`.
  **Prática:** `/v1/session/rollback` trata o rank local, mas não replica o
  descarte do sufixo rejeitado aos workers; com especulação TP ligada, os KV
  podem ficar divergentes.
  **Afetados:** implantações tensor-parallel que ativem speculative decoding.
  **Esforço:** médio, incluindo headers, fan-out do rank 0 e testes com dois
  ranks.

- [ ] **20 - Testes UDP transmitem na LAN real**
  **Fonte:** `crates/node/src/network/network.rs:1075-1082,1368-1393`.
  **Prática:** `broadcast_local` envia para `255.255.255.255:42777`, e testes
  que anunciam um nó não destacado podem alterar um cluster vivo na mesma LAN.
  **Afetados:** desenvolvedores e CI que executam a suíte junto de um cluster.
  **Esforço:** baixo/médio, com sockets isolados, porta injetável e testkit sem
  broadcast global.

## Média

- [ ] **5 - Planner open-core não está ligado ao node**
  **Fonte:** `crates/routing-base/src/lib.rs:12-62`;
  `crates/node/src/routing/pipeline.rs:262-283,379-410`.
  **Prática:** `routing-base::plan` é completo e testado, mas o node usa outro
  planner guloso com pontuação de carga; os dois podem evoluir sem teste de
  equivalência.
  **Afetados:** consumidores do provider aberto e mantenedores dos dois
  planners.
  **Esforço:** médio para equivalência; alto para trocar o caminho de produção
  com compatibilidade e telemetria.

- [ ] **10 - Build depende de extração completa do fork**
  **Fonte:** `crates/llama-engine/build/mtp_arch.rs:25-47,104-114,178-186`.
  **Prática:** o build falha quando o fork não existe ou a extração de
  arquiteturas MTP é vazia, ambígua ou fica abaixo do piso de segurança.
  **Afetados:** onboarding, CI e releases sem o checkout correto.
  **Esforço:** médio para tornar a preparação reproduzível; alto para eliminar
  a dependência local.

- [ ] **15 - Limite de restart não persiste entre processos**
  **Fonte:** `crates/node/src/inference/runtime.rs:20-25,1116-1120,1207-1227`.
  **Prática:** `MAX_RESTARTS=10` vive no supervisor do processo; se OOM ou outra
  falha mata o processo inteiro, o contador desaparece.
  **Afetados:** operadores de máquinas no limite de memória e ambientes com
  política automática de restart.
  **Esforço:** médio, persistindo falha sem recriar o laço infinito que a
  anotação `Pending`/`NeverLoaded` já evita.

- [ ] **16 - Modos de falha do relay não têm E2E**
  **Fonte:** `crates/node/src/inference/direct.rs`;
  `crates/node/src/inference/AGENTS.md:1011-1013`.
  **Prática:** hop intermediário morto, cauda morta, resposta direta perdida e
  timeout têm código e testes unitários, mas não um cenário de quatro peers
  que reproduza a topologia completa.
  **Afetados:** confiabilidade do pipeline distribuído.
  **Esforço:** alto, com harness multi-peer determinístico, falhas injetáveis e
  isolamento da rede real.

- [ ] **22 - Wizard de primeiro acesso ainda é protótipo**
  **Fonte:** `apps/frontend/src/components/setup-wizard/WizardScreens.tsx:24-45`;
  `apps/frontend/src/routes/setupWizardMocks.ts:11-80`.
  **Prática:** `/setup` navega por hardware, nós, modelos e redes simulados;
  não configura o nó, cria permissões, descobre peers nem baixa modelos.
  **Afetados:** usuários que esperam um fluxo real de primeira configuração.
  **Esforço:** alto, envolvendo backend, permissões, descoberta, downloads e
  estados de erro.

- [ ] **24 - Contrato ainda usa fim inclusivo**
  **Fonte:** `crates/routing-contract/src/lib.rs:16-24,65-73,81-91,131-167`.
  **Prática:** o wire v1 valida e percorre `end_layer` inclusivo; a versão futura
  com fim exclusivo só aparece como intenção, sem conversão ou implementação.
  **Afetados:** evolução do protocolo e interoperabilidade futura.
  **Esforço:** médio quando necessário, incluindo versão, validação,
  serialização e testes de compatibilidade.

- [ ] **26 - Build manual pode perder o commit do binário**
  **Fonte:** `scripts/build-docker.sh`; `Dockerfile` (runtime `ARG POM_BUILD_COMMIT`).
  **Prática:** um `docker compose build` manual sem `POM_BUILD_COMMIT` pode
  produzir o carimbo `desconhecido`.
  **Afetados:** builds manuais e pipelines que não exportam a variável;
  diagnóstico do binário fica degradado.
  **Esforço:** baixo, com validação no Dockerfile ou exigência explícita da
  variável, preservando builds locais sem Docker.

## Baixa

- [ ] **6 - Grafo do workspace diverge da documentação**
  **Fonte:** `Cargo.toml` do workspace; `crates/node/Cargo.toml:49`;
  `crates/AGENTS.md`.
  **Prática:** `node` depende de `routing-contract`, `llama-engine` e `brand`,
  não de `config`, `types` ou `routing-base`; planos de extração e análise de
  recompilação baseados no grafo antigo ficam errados.
  **Afetados:** mantenedores e futuros consumidores da arquitetura open-core.
  **Esforço:** baixo para corrigir documentação; médio/alto para concluir a
  extração real.

- [ ] **14 - Pool de contextos não está integrado**
  **Fonte:** `crates/llama-engine/src/llm.rs:3200-3214`.
  **Prática:** `LlmEngine::pool` existe e tem testes de orçamento, mas o
  servidor atual carrega um contexto por `load` e não usa a API.
  **Afetados:** quem espera decodes concorrentes no mesmo modelo.
  **Esforço:** médio/alto, com política de slots, memória, isolamento e
  telemetria.

- [ ] **18 - Diagnóstico MoE é ignorado por padrão**
  **Fonte:** `crates/llama-engine/src/llm.rs:958-960`.
  **Prática:** o teste só executa quando `POM_MOE_ROUTE_DIAGNOSTICS=1`; uma
  regressão pode passar despercebida na suíte normal.
  **Afetados:** diagnóstico e manutenção do caminho MoE.
  **Esforço:** baixo/médio, separando a asserção determinística da parte de
  logging e documentando o custo.

- [ ] **23 - Fixtures parciais podem quebrar testes**
  **Fonte:** `apps/frontend/src/test/fixtures.ts:1-8`;
  `apps/frontend/src/components/PeerDetailCard.tsx:339,686-695`.
  **Prática:** uma leitura usa `n?.status`, mas outra pressupõe o objeto
  completo; uma fixture parcial pode passar num componente e quebrar o
  seguinte. A fixture oficial já fornece a forma integral.
  **Afetados:** manutenção de novos testes e diagnósticos falsos durante o
  desenvolvimento.
  **Esforço:** baixo, com construtores tipados obrigatórios ou validação de
  fixture.

- [ ] **25 - Crate público de tipos ainda é placeholder**
  **Fonte:** `crates/types/src/lib.rs:1-12`.
  **Prática:** `types` só expõe `WorkspaceMarker`; `PeerInfo` e `NodeConfig`
  continuam dentro de `node`.
  **Afetados:** fronteira open-core e consumidores que esperam tipos públicos.
  **Esforço:** médio, extraindo tipos sem criar ciclos e atualizando
  consumidores.

## Não verificável nesta máquina

- [ ] **17 - Prova diferencial CUDA de MoE sem ambiente de execução**
  **Fonte:** `crates/node/src/inference/AGENTS.md:359-375`;
  `crates/llama-engine/src/llm.rs:6268-6273`.
  **Prática:** a prova exige um GGUF real, `--features cuda`, uma GPU CUDA e o
  executor externo; esses recursos não estavam disponíveis na auditoria.
  **Afetados:** validação numérica do caminho MoE CUDA.
  **Esforço:** alto, em uma máquina com GPU compatível e o GGUF real.

## Histórico preservado

O TODO anterior veio do levantamento de 2026-08-15, teve última limpeza em
2026-08-17 e registrava 29 itens triados pelo operador, dos quais 23 haviam
saído, 4 estavam adiados e 2 recusados. Essa contagem é histórica e não é
substituída pela classificação da auditoria de 2026-09-07.

O item antigo de bitswap fica registrado como resolvido: o arquivo e o
behaviour saíram do swarm, e a transferência por conteúdo passou a usar o
`.pom` e `GET /api/transfer/*?sha=` sobre HTTP Range. Na auditoria nova, ele é
o item 21, anotação obsoleta.

As notas antigas sobre alternância do endereço observado e sobre o timeout de
75 s do `connect` também ficam preservadas como histórico fora da auditoria
nova. A revisão atual já descreve a preferência pelo endereço local e o
cliente de peer limita a abertura TCP a 2 s; elas não são contadas como itens
atuais desta lista.

## Dívida de documentação em aberto

- O grafo do workspace diverge da arquitetura declarada. A pendência está
  consolidada no item 6; a decisão maior sobre a extração dos crates continua
  registrada aqui para não se perder.
- O contrato usa `end_layer` inclusivo. A evolução futura para fim exclusivo
  está consolidada no item 24 e exige nova versão do wire.

## Itens da triagem anterior incorporados

Os adiados `routing-base::plan`, `undeploy_authority` e `crates/types` são agora
os itens 5, 12 e 25, respectivamente, com a evidência atualizada. Nenhum
desses contextos foi descartado.
