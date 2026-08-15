# TODO - defeitos anotados nos AGENTS.md

Levantamento de 2026-08-15, extraído das seções `## Defeitos anotados` de todos
os `AGENTS.md` do repositório.

Cada item traz o arquivo onde o defeito está anotado, a evidência no código e o
critério de pronto. Itens que aparecem anotados em mais de um `AGENTS.md` foram
unificados, com todas as fontes listadas.

**Regra de manutenção:** ao corrigir um item, remova a anotação do
`AGENTS.md` correspondente no mesmo commit e marque a caixa aqui. Um defeito
corrigido que continua anotado é pior que um defeito não anotado.

## Triagem do captain - 2026-08-15

29 itens triados. Cada item abaixo carrega o veredito na própria linha.

| Veredito | Itens | O que significa |
|---|---|---|
| **Corrigir** | 23 | Entram em trabalho agora. |
| **Depois** | 4 | Reconhecidos, fora da rodada atual. |
| **Não fazer** | 2 | Decisão tomada; não voltar a propor. |

Os quatro adiados: fechar o `undeploy` para o principal, ligar ou remover o
`routing-base`, preencher ou remover `crates/types`, e resolver o `bitswap`
órfão. Os três últimos dependem das decisões de arquitetura modular e de
open-core, ainda em aberto.

Os dois recusados são o par do `docker-compose`, e a razão importa mais que o
veredito: **não é defeito**. São dois ambientes de gente diferente -
`gen-node-configs.py` gera a configuração da máquina do captain, e o
`docker-compose.yml` serve a máquina do salem. Os dois esquemas de nome não
precisam se encontrar. A anotação em `scripts/AGENTS.md` está errada e deve ser
substituída por essa explicação.

---

## Correção (o comportamento está errado)

- [x] **`self_reply_urls` não filtra o loopback e perde a resposta**
  `crates/node/src/inference/direct.rs:171-184` · anotado em `crates/node/src/inference/AGENTS.md`
  O doc promete que o loopback não entra, mas o código só descarta vazio, `0.0.0.0` e `[::]`.
  Quando `gateway::self_dial_host` cai no fallback `127.0.0.1`, a lista fica com
  `http://127.0.0.1:<gateway_port>`, `DirectReply::new` a considera não-vazia e liga o modo
  direto - a cauda em outra máquina entrega ao loopback *dela* e a resposta se perde,
  com prazo de 600 s. O filtro prometido é justamente o que faria a lista ficar vazia e o
  plano cair no anel, que funciona.
  **Pronto quando:** o filtro descarta loopback IPv4 e IPv6, existe teste de que uma lista
  só-loopback resulta em plano em anel, e o caso é exercitado sem esperar os 600 s.

- [ ] **Anúncio do gossipsub diverge do `self_announce`**
  `crates/node/src/main.rs:776-825` · anotado em `crates/node/AGENTS.md` e `crates/node/src/network/AGENTS.md`
  A task de publish anuncia só `frame/v1-f16` e `frame/v2-zstd-f16` (faltam `relay/v1-chain`
  e `speculate/v1-lookup`), com `role` e `hostname` vazios, e **só publica quando existe
  assignment**. Um peer que chegue só por gossipsub perde capacidades, papel e nome, e um nó
  sem modelo fica invisível pela internet - o caminho UDP anuncia sempre. O comentário de
  `self_announce` fala em sincronizar "os outros dois builders" e não conta com este terceiro.
  **Pronto quando:** existe um único construtor do anúncio usado pelos três caminhos, e um
  teste prova que gossipsub e UDP publicam o mesmo conjunto, inclusive sem assignment.

- [ ] **Chave pkarr resolvida contra o cwd, não contra o project root**
  `crates/node/src/main.rs:462` (`Path::new(".rdma-p2p.key")`) · anotado em `crates/node/AGENTS.md`
  Diferente de `identity::key_path()`, que resolve contra `project_root()`. Rodar o binário de
  outro diretório gera ou ignora outra identidade pkarr, contra a regra documentada do project root.
  **Pronto quando:** o caminho passa por `identity::key_path()` e um teste cobre a execução a
  partir de um cwd diferente.

- [ ] **`--gateway` deprecated ainda muda comportamento**
  `crates/node/src/main.rs:564-568` · anotado em `crates/node/AGENTS.md`
  Sem a flag, `ollama_port` não é anunciado, então peers não conseguem rotear para o Ollama
  local deste nó mesmo com ele servindo a API.
  **Pronto quando:** o anúncio do `ollama_port` não depende mais de uma flag marcada como
  deprecated, ou a flag deixa de ser deprecated e a documentação diz o que ela faz.

- [ ] **Desistência silenciosa quando a porta de inferência fica presa**
  `crates/node/src/inference/runtime.rs` · anotado em `crates/node/src/inference/AGENTS.md`
  `free_port` mata o ocupante antes de subir, mas quando o ocupante não é identificável
  (`lsof`/`netstat` falham) ou a porta segue presa, o supervisor esgota `MAX_RESTARTS` em
  silêncio e a TUI continua mostrando "Mem loaded: inference_server" como se estivesse tudo bem.
  O peer só quebra na inferência, com `encerrou com erro: Address already in use` no log.
  **Pronto quando:** o estado visível ao operador reflete a falha (mesmo padrão do ciclo de
  vida da fatia introduzido em `73426e7`) em vez de continuar anunciando saúde.

- [ ] **Dois estados vazios se sobrepõem na aba Detalhes**
  `apps/frontend/src/routes/Dashboard.tsx:222` e `:272` · anotado em `apps/frontend/src/routes/AGENTS.md`
  Com `c.peers.length === 0` na visão estendida, `dashboard.noMachines` rende dentro da área de
  rolagem e o overlay `dashboard.emptyNetwork` rende por cima - a mesma condição desenha duas
  mensagens empilhadas. Preexistente ao `5423852`, que mudou a condição da primeira sem
  resolver a duplicação.
  **Pronto quando:** uma única mensagem aparece na rede vazia, com teste que falha se as duas
  voltarem a coexistir.

- [ ] **`AddNodeDialog` mantém seleções velhas entre aberturas**
  `apps/frontend/src/components/AddNodeDialog.tsx` e `apps/frontend/src/routes/Dashboard.tsx`
  anotado em `apps/frontend/src/components/AGENTS.md` e `apps/frontend/src/routes/AGENTS.md`
  `nodeId` e `sliceIndex` são semeados uma vez (`useState(peers[0]?.node_id ?? "")`) e a dialog
  fica sempre montada, então reabrir depois de a lista de peers mudar pode submeter um
  `node_id` que nem está no dropdown.
  **Pronto quando:** as escolhas são re-semeadas a cada abertura, com teste de reabertura após
  um peer sair.

- [ ] **`ModelNetworkCanvas` entrega faixas inválidas ao `PeerDetailDialog`**
  `apps/frontend/src/components/ModelNetworkCanvas.tsx` · anotado em `apps/frontend/src/components/AGENTS.md`
  `otherRanges` vem de `pipelinePeers(cluster)`, que cai para *todos* os peers quando nenhum
  casa com o modelo ativo; um peer sem fatia então contribui seu `[0, 0]` padrão às faixas que
  `RemoveNode` compara, e o aviso "estas camadas ficam descobertas" pode estar errado nesse
  estado degenerado. Só o caminho de duplo clique do canvas é afetado - a aba Painel monta
  `otherRanges` só a partir do grupo da pipeline.
  **Pronto quando:** o caminho do canvas usa a mesma origem de faixas da aba Painel, com teste
  do estado degenerado.

## Segurança e validação de entrada

- [ ] **O handshake lê `pubkey` e nunca o verifica**
  `crates/node/src/backend/session.rs` (`handle_handshake`, `HandshakeRequest.pubkey`)
  anotado em `crates/node/src/backend/AGENTS.md`
  A aprovação depende só de `connection_key` e `blocked_nodes`; o campo é aceito e ignorado,
  então não oferece autenticação nenhuma.
  **Pronto quando:** ou o campo passa a ser verificado de fato, ou sai do protocolo e do doc -
  um campo de autenticação que não autentica é pior que a ausência dele.

- [ ] **`RoutingRequest::validate` não valida o que importa**
  `crates/routing-contract` · anotado em `crates/routing-contract/AGENTS.md` e `crates/routing-base/AGENTS.md`
  `validate` confere só a versão da API, então um snapshot malformado (peer com
  `end_layer < start_layer`, ou faixa além de `total_layers`) chega ao provider.
  `RoutingPlan::validate` pega os mesmos problemas na *saída*, mas só depois de o provider já
  ter confiado na entrada. `ModelCapability.total_layers` e `PeerCapacity` não têm validação própria.
  **Pronto quando:** a validação de entrada cobre ordenação das faixas e limite de
  `total_layers`, com testes dos dois casos malformados.

- [ ] `DEPOIS` **`undeploy_authority` permite qualquer papel derrubar a pipeline inteira**
  `crates/node/src/backend/deploy.rs:1794` · anotado em `crates/node/src/backend/AGENTS.md`
  Retorna `Ok(())` para todo papel. Está aberto de propósito e o teste
  `any_node_may_undeploy_for_now` (`deploy.rs:2242`) documenta a porta aberta, então fechá-la é
  uma edição consciente: trocar o corpo da função pela checagem Principal-only que o doc
  comment já descreve, e atualizar o teste. A rota ainda exige a connection key
  (`ConnectionAuth`), então é aberta ao operador, não anônima.
  **Decisão do captain antes de mexer:** fechar agora para Principal-only, ou manter aberto.

## Configuração e build (silenciosamente sem efeito)

- [ ] **`target_os = "linux1"` - o linker mold nunca é aplicado**
  `.cargo/config.toml:27` · anotado em `scripts/AGENTS.md`
  `target_os` nunca será `"linux1"`, então `-fuse-ld=mold` não vale em lugar nenhum.
  **Pronto quando:** o cfg vira `target_os = "linux"` e o mold entra de fato, ou a seção sai.

- [ ] **Comentário e valor de `incremental` se contradizem**
  `.cargo/config.toml:25` · anotado em `scripts/AGENTS.md`
  O comentário diz que `incremental` fica desligado porque impede inline, mas o valor real é
  `incremental = true`. Um dos dois está errado.
  **Decisão do captain:** manter `incremental = true`. É o comentário que está errado.
  **Pronto quando:** o comentário descreve a escolha real e a razão dela.

- [x] `NÃO FAZER` **`docker-compose.yml` monta fixtures que o gerador nunca escreve**
  `docker-compose.yml` e `scripts/gen-node-configs.py` · anotado em `scripts/AGENTS.md`
  O compose monta `./tests/fixtures/node-gpu`, `node-gpu-2`, `node-cpu-1`, `node-cpu-2`, mas o
  gerador escreve em `tests/fixtures/node-<N>` (node-1..node-5) - os dois esquemas de nome
  nunca se encontram, e `tests/` nem existe hoje. O compose sobe com diretórios vazios e nós em
  modo setup, sem as portas que a config gerada definiria.
  **Pronto quando:** um único esquema de nomes, e subir o compose a partir do repositório limpo
  produz um cluster funcional.

- [x] `NÃO FAZER` **`.dockerignore` ignora `rdma-p2p.json` sem caminho versionado alternativo**
  `.dockerignore` · anotado em `scripts/AGENTS.md`
  O compose só funciona com os fixtures do item acima, e os `node-<N>/` são gerados, não
  versionados. Depende do item anterior.

## Código morto e andaimes

- [ ] `DEPOIS` **`routing-base::plan` não é chamado em lugar nenhum**
  anotado em `crates/AGENTS.md` e `crates/routing-base/AGENTS.md`
  O planejador do `node` (`crates/node/src/routing/pipeline.rs::plan_pipeline`) monta um
  `RoutingRequest` a partir dos tipos do contrato para validar, mas faz seu próprio
  planejamento guloso com pontuação de carga. `routing-base` é código morto, vivo apenas pelos
  próprios testes de unidade, e os dois planejadores podem divergir sem nenhum teste comparando.
  A intenção declarada de open-core (um provider contra o qual o plugin fechado implementaria)
  não está ligada a nenhum ponto de chamada.
  **Depende de:** a decisão de open-core aberta com o captain (licença e fronteira de entrega do Pro).

- [ ] `DEPOIS` **`crates/types` é um marcador vazio**
  anotado em `crates/types/AGENTS.md`
  O único "tipo" é `WorkspaceMarker`. Tipos de domínio de verdade que o `node` ainda detém
  (`PeerInfo`, `NodeConfig`) não foram movidos.
  **Depende de:** a mesma decisão de arquitetura modular.

- [ ] **`crates/config` é andaime vazio**
  anotado em `crates/config/AGENTS.md`
  Declarado no workspace, morto até a extração de `crates/node/src/utils/config.rs`. Quem mexe
  em config hoje edita o módulo do node, não este crate.
  **Depende de:** a mesma decisão de arquitetura modular.

- [ ] `DEPOIS` **`bitswap` é protocolo órfão**
  `crates/node/src/network/discovery.rs::handle_event` · anotado em `crates/node/src/network/AGENTS.md`
  `bitswap_behaviour` está no swarm e os eventos só viram `debug!` - nenhuma requisição é
  atendida, nenhum bloco é servido. A transferência real vai por HTTP Range
  (`backend/transfer.rs`). O protocolo existe sem lado servidor.
  **Pronto quando:** ou o lado servidor existe, ou o behaviour sai do swarm.

- [ ] **`/admin/transfer-offers` é um stub que sempre devolve vazio**
  `crates/node/src/backend/admin.rs:27-28` · anotado em `crates/node/src/backend/AGENTS.md`
  Retorna `{ "offers": [] }` fixo. As ofertas de verdade estão em `GET /api/transfer/offers`
  (`transfer.rs::list_offers`), então quem consumir a rota `/admin` nunca vê uma oferta.
  **Decisão do captain:** implementar a rota corretamente, não remover.
  **Pronto quando:** a rota devolve as ofertas reais, com teste.

- [ ] **`ModelNetworkDemo.tsx` é código morto**
  anotado em `apps/frontend/src/components/AGENTS.md`
  Nada na aplicação o importa; é uma simulação com valores fixos, resto da construção do
  `ModelNetworkCanvas`. Mantém a própria cópia da lógica de layout e medidores, que vai
  divergir do canvas real.

- [ ] **`transferStatus.ts` exporta `TransferState` sem uso**
  anotado em `apps/frontend/src/lib/AGENTS.md`
  Nada fora do módulo usa a união; os predicados tipam direto em `TransferJob["state"]`.

## Robustez

- [ ] **`networkLayout.ts` escreve em `localStorage` sem proteção**
  `writePositions` e `clearPositions` · anotado em `apps/frontend/src/lib/AGENTS.md`
  Sem `try/catch`, diferente de todos os outros helpers de armazenamento da pasta - exceções de
  cota ou segurança estourariam de dentro do handler de arrastar do canvas.

- [ ] **`set_log` nunca é chamado**
  `crates/node/src/inference/runtime.rs:704` · anotado em `crates/node/src/inference/AGENTS.md`
  A função existe para mandar os logs do servidor embutido para `infer_server_<porta>.log`, mas
  nenhum caminho a invoca, então `log_path` fica sempre `None` e todo log vai para o stdout. A
  TUI limpa a tela a cada `--refresh`, então o único jeito de ver é redirecionar o stdout do binário.
  **Decisão do captain:** passar a escrever o arquivo de log, não remover a função.
  **Pronto quando:** `set_log` é chamado no arranque do servidor embutido e o arquivo aparece.

## Internacionalização (strings pt-BR fixas fora do i18n)

Todos estes contam como um trabalho só, e conversam com a investigação de i18n já em curso.
Com o locale EN ativo, estas partes continuam em português.

- [ ] **`ModelNetworkCanvas.tsx`** - botões ("Diminuir zoom", "Ajustar à rede", "Reorganizar nós
  automaticamente", "Fit") e estados vazios ("Nenhum modelo implantado", "Nenhuma capacidade de
  inferência anunciada") são literais, enquanto a legenda já usa `t("canvas.*")` - a legenda
  troca de idioma e o resto não. Anotado em `apps/frontend/src/components/AGENTS.md`.
- [ ] **`deleteModel.ts` e `toast.tsx`** - confirmações ("Apagar ... do disco desta máquina?",
  "Atenção:", "Apagar mesmo assim?") e `aria-label="Fechar aviso"` fixos. Anotado em
  `apps/frontend/src/lib/AGENTS.md`.
- [ ] **`Shards.tsx`, `WholeModels.tsx`, `Transfers.tsx`** - "Excluir", "apagado", "Não deu para
  apagar", "Cancelar", "Transferência cancelada", "enviando para", "recebendo de". Anotado em
  `apps/frontend/src/routes/AGENTS.md`.
- [ ] **`Chat.tsx`** - o `ExecutionTelemetry` traz a tabela inteira de telemetria e a nota de
  rodapé fixas em português. Anotado em `apps/frontend/src/routes/AGENTS.md`.

## Cosmético

- [ ] **`LeaveNode` reusa o rótulo do gatilho no botão de confirmação**
  anotado em `apps/frontend/src/components/AGENTS.md`
  O botão do card e a confirmação da janela leem os dois "Sair da rede" - a janela mostra dois
  botões de mesmo nome, e o teste clica em `buttons[buttons.length - 1]`, contando com o portal
  pôr o botão da janela por último no DOM. O usuário pode confundir gatilho e confirmação.

## Dívida de documentação (sem tarefa de código)

- O `README.md` da raiz já foi reconciliado com o código em 2026-08-15. Ele não é fonte de
  verdade para arquitetura: valem o `AGENTS.md` da raiz e `docs/subsystems/*`.
  Anotado em `docs/AGENTS.md`.
- O grafo do workspace diverge da arquitetura declarada - a documentação descreve o `node`
  dependendo de `config` e `types`, e o código não. Não é defeito de comportamento; se resolve
  junto com a extração dos crates. Anotado em `crates/AGENTS.md`.
- O contrato diz que `end_layer` é inclusivo; uma versão futura de fio com "fim exclusivo" está
  sinalizada em comentário mas não implementada. Anotado em `crates/routing-contract/AGENTS.md`.
- O doc do módulo `webui.rs` afirma que os assets da UI entram compilados via `include_str!`
  (`crates/node/src/backend/webui.rs:1-9`) - não existe `include_str!` nem `include_bytes!` no
  backend. O bundle é lido do disco por `webapp.rs` em `config::ui_dist_dir()` (padrão
  `<project_root>/apps/frontend/dist`). O doc está defasado e engana quem procurar de onde a UI
  é servida. Anotado em `crates/node/src/backend/AGENTS.md`.
