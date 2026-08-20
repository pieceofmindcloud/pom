# Subsistema: implantação na rede

O subsistema põe em serviço um conjunto de fatias que já existe, distribuindo-as
pelas máquinas do cluster. Vive em `crates/node/src/backend/deploy.rs`
(planejamento e sequenciamento) e usa `crates/node/src/backend/webui.rs`
(aplicação da fatia num nó, `apply`/`launch`/`release`), `crates/node/src/models/`
(catálogo, `plan_by_size`/`extract_shard`, `splitter::merge`) e
`crates/node/src/backend/transfer.rs` (movimentação dos bytes). Toda a lógica é
"a escolha do operador é parte do pedido": o nó sugere, o operador decide, e o
deploy re-varre o cluster em vez de confiar na prévia.

## 1. Visão geral: o plano, a fusão e a ordem

Duas rotas fazem o trabalho grosso: `POST /api/ui/deploy/plan` (leitura, barata,
chamada a cada edição na tela) e `POST /api/ui/deploy` (executa; re-varre o
cluster). O pedido carrega `group` (o modelo original, pela chave técnica
`ShardSet.group` derivada do `pom_id` do `.pom` - ou o **nome** do modelo no catálogo, com
`kind: "whole"`), `kind`
(`"shards"`, o padrão, `"whole"` - ver a seção 1.1 - ou
`"tensor_parallel"` - ver a seção 1.2), `params` (o
`RuntimeParams` do pipeline inteiro), `selection` (a escolha explícita do
operador - `None` = o nó decide) e `merge` (junção das fatias adjacentes de uma
máquina). O `selection` viaja na requisição porque o
`deploy` re-surveys: a escolha é validada contra a varredura nova e recusada se
não continuar valendo - a cópia da tela pode estar um minuto velha e uma máquina
que saiu do cluster não pode receber uma fatia.

O pedido também aceita `deployment_name` (nome operacional separado de `group`),
`strategy: "simple" | "complete"` e `preferred_node_ids`. Nome vazio usa o nome
do modelo e nomes maiores que 128 caracteres são recusados. `group` continua
sendo a identidade técnica usada pelo catálogo, gossip e roteamento. A estratégia
simples usa o menor conjunto suficiente de peers; a completa prefere os cortes
existentes mais finos e distribui a carga entre mais máquinas. Nenhuma estratégia
cria limites novos de GGUF: para isso continua existindo a tela de Fatiar.

Três regras saem da arquitetura, não do gosto:

- **Ordem do pipeline = ordem das camadas.** Quem fica com a camada 0 recebe o
  prompt inteiro de cada requisição. Escolher a ordem é o mesmo ato que escolher
  quem fica com qual faixa - não há segundo botão.
- **Um nó serve uma faixa distinta**, exceto com `merge` ligado: faixas adjacentes
  viram uma unidade fundida (`splitter::merge`, Rust), a máquina junta as fatias
  num arquivo só e serve a união. O passo fundido carrega `parts`, um por fatia.
  A mesma faixa exata em nós diferentes é uma réplica do mesmo estágio e forma
  uma rota paralela válida.
- **Sobreposição parcial é recusa, buraco é aviso.** Duas faixas com cortes
  diferentes sobre a mesma camada tornariam a resposta ambígua; cópias exatas
  são ignoradas na cobertura lógica e ficam disponíveis para o roteador. Um
  buraco é meia-implantação legítima que só impede o pipeline de planejar.

## 1.1 Modelo inteiro: o caso degenerado, não um segundo caminho

`kind: "whole"` (`DeployKind`, `deploy.rs`) implanta **um GGUF não fatiado em
uma ou mais máquinas**. Cópias da faixa completa são réplicas do mesmo estágio
e rotas paralelas válidas para balanceamento por RTT/carga. Não existe segundo fluxo: `survey` lê o modelo do `models` do
`/api/ui/models` de cada peer (em vez do `shard_sets`) e o converte, em
`parse_whole`, num `HeldShard` de faixa `0..total_layers-1`. Daí para frente é
tudo o mesmo código - `candidates`, `model_size`, a aritmética de cobertura, o
guarda de disco/memória, a fase de cópia e `launch_targets`. A cobertura fecha
por construção, "um nó serve uma faixa" é trivialmente satisfeita, e cada alvo sai
como `LaunchTarget` comum com `skip_split: true`, o que faz o `apply` no destino
se reconhecer como `single_peer` (sem extração de estado oculto). Quando o
operador seleciona mais de um destino, cada cópia vira um `Step` independente;
uma repetição literal da mesma faixa no mesmo destino é ignorada como ação
idempotente.

O que muda de fato:

- **De onde o inventário é lido.** `parse_whole` recusa uma fatia
  (`shard_range` preenchido) e recusa um modelo sem contagem de camadas no
  cabeçalho GGUF - `total_layers` é o número com que todo peer é configurado, e
  `NodeConfig::validate` recusaria o zero no destino, depois de `launch` já ter
  parado a inferência de lá.
- **O substantivo das recusas.** `DeployKind::unit` troca "a fatia L0-31" por
  "o modelo <nome>": quem escolheu um modelo inteiro não vê fatia nenhuma na
  tela.
- **A identidade do modelo.** `group` é o `name` do catálogo, e ele viaja como
  `model_label` para o peer - dois nomes diferentes para o mesmo arquivo são
  dois modelos, aqui e no planejador de pipeline.

Limite conhecido: se a máquina de destino ainda não tem o arquivo, a cópia usa o
sentido **puxar**, que `transfer::safe_model_path` prende à pasta de fatias de
quem envia. Um modelo inteiro mora quase sempre fora dela (biblioteca do LM
Studio, store do Ollama), e por isso o `note` do passo avisa para mandar o
arquivo pela tela de Transferências antes.

## 1.2 Tensor parallel (caminho C): o grupo

`kind: "tensor_parallel"` (`DeployKind::TensorParallel`) implanta um modelo num
grupo de máquinas por tensor parallelism: um **primary** (dono do GGUF e hop do
pipeline) + K **workers** (cada um extrai o próprio shard de tensor da própria
cópia local). O pedido carrega `tp` (`TpLaunch`): `{size, primary_node_id,
worker_node_ids}` - o operador escolhe o **primary** e os **workers** por
node_id entre as máquinas no mapa (a tela oferece selects, não um campo de URL), e o nó deriva a
`rank0_url` da porta de inferência anunciada pelo próprio primary
(`Machine.inference_url`). A varredura lê o modelo inteiro do primary, e o
`launch_targets` expande o passo em **um alvo por rank** (o primary com rank 0 e
cada worker com rank 1..size-1), todos apontando a mesma `rank0_url` para a
coletiva.

Antes de aceitar, o plano valida em duas camadas: `tp_group_checks` (tamanho,
primary/workers no cluster e com o modelo inteiro, sem repetição, primary com
URL de inferência) roda no próprio `plan`; e `tp_shardability_check` pergunta ao
primary (rota `POST /api/ui/tp/check`) se o GGUF dele dá para partir - MoE/SSM/
weight-tied ou dimensões que não dividem recusam o plano **na tela**, com um
motivo que nomeia o tensor, em vez de deixar o job falhar no apply.

`POST /api/ui/tp/check` é rota de operador como o resto de `/api/ui/*`: a
chamada tem que levar a chave de conexão (`bearer_auth`). Ir sem ela rendia
`{"error":"missing Authorization header"}`, que é JSON válido sem `ok`, e o
código anunciava isso como "não tem a checagem de shard (binário antigo?)" -
mandando o operador conferir versões com as duas máquinas no mesmo commit
(17/08). Hoje a resposta é lida por `tp_check_verdict`, que repete o `error` do
peer quando ele existe e só fala em binário antigo quando o corpo não explica
nada.

O lado de `apply` está pronto: `ApplyRequest.tp` (a `TpIdentity` de `network.rs`)
faz **cada rank** extrair o próprio shard por `plan::extract_tp_shard` - o rank 0
inclusive, porque `role` (`primary`/`worker`) endereça a coletiva e não decide
quem corta. Enquanto essa condição foi `role == "worker"` (`tp_shard_to_extract`,
17/08), o primary caía no ramo `skip_split` e servia o GGUF inteiro: largura
cheia de um lado do all-reduce, meia largura do outro, e o `_tp0of2.gguf` nunca
existia no disco. A `TpIdentity` carrega ainda `members` - todos os ranks em
ordem, com `node_id` e URL de inferência -, porque **o rank 0 precisa pôr os
workers no mesmo decode**: TP só funciona em lockstep, e o gateway fala só com o
rank 0. Membro sem porta de inferência anunciada é recusa no plano
(`tp_members`), não travamento na primeira barreira. O nó também anuncia o papel via
`Network::set_tp` e registra a coletiva (`register_tp_reduce`). Ver
`docs/subsystems/fatiamento.md` (seção do shard por tensor) e
`docs/subsystems/rede-p2p.md`.

Cada membro também carrega o `PeerId` libp2p e a capability do túnel anunciada
pelo servidor. Assim, o `broadcast_to` do rank 0 e o reduce da coletiva podem
repetir a chamada pelo swarm quando as máquinas estão em redes sem rota HTTPS
direta entre si.

`AddNodeRequest` também carrega `kind`: somar uma segunda máquina a um modelo
inteiro já no ar é uma rota paralela, igual a somar uma máquina a uma fatia.

## 2. A varredura (`survey`)

`survey` junta, para cada máquina do cluster, `GET /api/ui/models` (catálogo,
timeout `PEER_TIMEOUT * 4`) e `GET /api/ui/node` (identidade/hostname, timeout
`PEER_TIMEOUT`). Monta `Vec<Machine>`:

- `Machine { node_id, hostname, mem_free_mb, gpu_free_mb, ram_free_mb,
  disk_free_mb, rtt_ms, load, held: Vec<HeldShard> }`.
- `HeldShard` é uma fatia do conjunto naquela máquina, com `total_layers` como
  `Option` - `None` é o estado normal de fatia antiga (sem metadata `pom.*`),
  e fica `Option` até `model_size` porque "não sei" e "zero" são fatos
  diferentes.
- `mem_free_mb` é VRAM livre numa máquina com GPU discreta, RAM disponível em
  qualquer outra; `0` = a máquina não respondeu, e nada é inferido disso.
- `disk_free_mb` é o espaço para o arquivo, que é o que uma cópia precisa e o que
  `transfer.rs` checa antes de escrever um byte.
- `gpu_free_mb` e `ram_free_mb` são expostos separadamente para a tela sugerir
  GPU primeiro e RAM como capacidade de contingência. `mem_free_mb` continua
  sendo a capacidade efetiva usada pelas verificações do planner: VRAM discreta
  quando disponível, senão RAM.
- `rtt_ms` da máquina é a medição vista pelo gateway para orientar a escolha
  automática; `0` identifica o próprio nó e `null` quer dizer que ainda não
  houve medição. As arestas de `route_options` usam uma matriz separada e
  direcional, agregada dos relatórios `/api/ui/network/rtt`, para não exibir a
  latência de um terceiro peer como se fosse o hop real. `load` é a quantidade
  de requisições em andamento observada pelo `LoadTracker`.
- `label()` - hostname, ou os primeiros 12 caracteres do `node_id` quando o
  hostname é vazio ou repetido.

`holds(start, end)` acha a fatia exata na máquina; `overlapping_layers` acha
sobreposição entre uma faixa e a coleção de outra.

## 3. O plano (`plan`)

`plan_with_options(group, machines, selection, params, merge, kind, tp, strategy,
deployment_name, preferred_node_ids)` monta o `Plan`. Sem `selection` (caminho
padrão), a estratégia escolhe `auto_chain` ou `auto_chain_complete`, e
`auto_assign` considera capacidade combinada de GPU/RAM, carga, RTT e peers
preferidos; um peer sem a fatia local também pode receber a cópia; com
`selection`, cada `SelectedSlice { start_layer, end_layer, target_node_id,
n_gpu_layers, offload_kqv }` é validado por posição:

- **`target_node_id` vazio é recusa, não convite** para o nó escolher.
- **Máquina com `n_gpu_layers`/`offload_kqv`**: só o que é propriedade do hardware
  viaja por máquina. O cache KV é dimensionado pela janela de contexto, e uma
  caixa com 16 GB de VRAM e uma sem GPU no mesmo pipeline não podem estar as duas
  certas sobre onde ele cabe. Tudo o mais (`RuntimeParams`) é do pipeline inteiro
  - contexto, batch e pedaço decidem o orçamento de prompt que o gateway cobra, e
  a amostragem só roda no último peer.
- **Ajuste automático de memória**: `RuntimeParams.fit_memory` fica ligado por
  padrão em `shards` e `whole`. O nó chama o `common_fit_params` do llama.cpp
  antes da carga usando a capacidade total `n_ctx * max_sessions`, e pode
  reduzir as camadas GPU para evitar OOM. Um
  `n_gpu_layers` diferente de `99` é explícito e desliga esse ajuste de camadas.
  Em `tensor_parallel`, o planner normaliza `fit_memory` para desligado porque
  o fit nativo não suporta `split-mode tensor`.
- **Uma fatia que não existe em máquina nenhuma** -> `blocked` ("a fatia L{}-{}
  não existe em máquina nenhuma do cluster"). `plan.blocked` e `steps` vazio são
  os dois recusos de 400 do `deploy`.
- **Máquina ocupada não é erro de `plan`**: `alternatives`/`merged_alternatives`
  listam toda máquina como alvo de cada faixa com o motivo (`ok`/`note` - "já tem
  a fatia", "só N GB livres em disco", "receberia N GB"). Ocupação é deliberadamente
  fora do `ok`: uma máquina que já serve outra faixa é exatamente a que o operador
  quer usar como rota paralela, e escondê-la tornaria a redundância impossível.
  Um peer sem a fatia local também aparece quando tem espaço para receber a
  cópia. A capacidade usa GPU primeiro e RAM como fallback.

**`steps` sai sempre em ordem de camada**, plano recusado incluído, e
`position` é numerado depois dessa ordenação. A ordem não é estética: a ordem do
pipeline *é* a ordem das camadas, o `run` lê a ordem de lançamento direto dessa
lista, e a tela alinha o plano com linhas que ela mantém ordenadas por camada. O
laço de agrupamento anda por máquina de destino, então a ordenação é aplicada uma
vez no fim - sem ela, uma máquina com duas faixas emitia a segunda na frente de
tudo que estava no meio, e a tela de implantar emudecia (2026-08-16). Pela mesma
razão **`target_node_id` vazio nunca agrupa**: "sem máquina" não é uma máquina, e
juntar toda faixa sem destino sob o mesmo id vazio produzia uma recusa que
nomeava uma máquina sem nome.

O `Plan` carrega `total_layers` com `total_layers_inferred` (deduzido da cobertura
quando a fatia não declara), `transfer_bytes`, `steps` (cada um com `ready`
quando a máquina já tem a fatia, e `merged` quando é unidade fundida com `parts`),
`blocked`, `merge`, `available`, `machines`, `missing`, `overlap`, `warnings`, e
os dois números de engenharia que derivam de `params` (`prompt_tokens` e
`chunk_tokens`, das mesmas fórmulas do gateway - `prompt_budget_of` e
`prompt_chunk_of`), `deployment_name`, `strategy`, `route_options` e
`load_distribution`. `route_options` mostra somente a ponte entre máquinas
selecionadas em estágios consecutivos, com o RTT direcional conhecido para o
próximo estágio; não inventa latência quando a origem não publicou a rota.
Réplicas
exatas não criam um hop extra: pertencem ao mesmo estágio e o roteador de
inferência escolhe a cópia com menor RTT medido, equilibrando também a carga em
voo. `load_distribution` separa carga atual de carga projetada pelo plano, sem
abrir uma segunda execução concorrente.

### O modelo e seu tamanho

`model_size` resolve o total de camadas do modelo com precedência
**Declared > Inferred > Unknown**: uma fatia que declara `pom.original_block_count`
manda; sem declaração, o total é deduzido do alcance da cobertura no cluster
inteiro (nunca chute de uma fatia só); sem nada disso, `Unknown`. Isso é o que
impede o bug de 2026-08-10, quando o plano se dizia completo e todo `apply` morria
em `NodeConfig::validate` depois de o `launch` já ter parado a inferência das
máquinas alvo. Fatias sem total são nomeadas "L{}-{} em {}" (`nameless`) e só
aparecem assim.

## 4. Execução (`deploy_endpoint` -> `run`)

1. `deploy_endpoint` re-varre (`survey`), re-planeja, recusa `blocked`/vazio,
   cria o job ("Implantar {deployment_name} em {n} nós") e spawna a execução.
2. `run` monta a lista de `Pull`s - uma unidade de movimento por fatia que tem
   que chegar a um alvo. Para passo fundido, um `Pull` por part faltante; a
   máquina "tem" a unidade fundida quando já tem todas as partes, e aí o merge
   roda localmente com nada cruzando a rede. Fatia que não tem de onde vir é erro
   ("a fatia L{}-{} não tem de onde vir").
3. Os bytes viajam pelo transporte de transferências (ver `transferencia.md`):
   HTTP `Range` com retomada por `.part`, MD5 conferido antes do arquivo tomar o
   nome final, progresso no sino vindo dos nós (fechar a aba não interrompe
   nada). A checagem de disco da implantação conta a **união** das fatias mais o
   que falta - a máquina precisa de espaço para o modelo junto *e* para as partes.
4. Cada alvo recebe o `ApplyRequest` via `apply`/`launch` (detalhe na seção 6).
5. `done` = "o modelo está atendendo na rede"; erro vira job `failed`.

## 5. Rotas do deploy

| Rota | Papel |
|---|---|
| `POST /api/ui/deploy/plan` | prévia (leitura, chamada a cada edição) |
| `POST /api/ui/deploy` | executa (re-varre, recusa `blocked`/vazio, job) |
| `POST /api/ui/deploy/node` | adiciona uma máquina a um pipeline já no ar |
| `POST /api/ui/deploy/node/remove` | tira uma máquina do modelo (libera a memória) |
| `POST /api/ui/undeploy` | tira a pipeline inteira do ar |

## 6. Adicionar e remover máquina

`add_node_endpoint` recebe uma faixa + máquina alvo e chama `webui::launch` com
um único `LaunchTarget` (`skip_split: true` - a fatia já existe e o passo é só
carregar). Dois detalhes escondidos na chamada:

- **`peer_index`/`peer_count`** - a posição é a do pipeline, não "este nó sozinho":
  sem isso o nó somado se diz o último da divisão.
- **`primary_node_id`** - quem é a entrada do pipeline não muda por somar um nó:
  só quem fica com a camada 0 recebe o prompt. `primary` só é marcado quando
  `start_layer == 0`.
- Resposta `{applied, overlapping_layers, peer_index, peer_count}` - sobreposição
  é reportada, não recusada (a máquina entra e a tela mostra o que colide).

`remove_node_endpoint` ("essa máquina não está servindo este modelo" se o alvo não
serve) calcula `uncovered_after_removal` contra as máquinas restantes. **Quem
libera a memória é o dono dela**: alvo == self usa `runtime.stop()`; senão
encaminha `POST /api/ui/unload` ao gateway do alvo (a mesma rota do botão de
descarregar uma máquina). Resposta `{removed, uncovered_layers}`; vazio significa
que outra máquina cobre a mesma faixa - a rede perde uma rota paralela e segue
servindo; não-vazio é buraco, e a confirmação é explícita.

## 7. Desimplantar (undeploy)

`POST /api/ui/undeploy` é o contrário de implantar, em um ato só. Remover máquina
por máquina era clicar na ordem certa torcendo para não esquecer nenhuma - e uma
esquecida é RAM e VRAM presas numa fatia que não serve mais para nada, porque o
pipeline só planeja com a cobertura fechada.

**O que isto NÃO faz: mexer no disco.** Nenhuma fatia é apagada. Desimplantar é
devolver a memória; os arquivos continuam onde estão e a mesma implantação volta
com um clique em vez de com 17 minutos de transferência por máquina. Apagar
fatias é o que a tela de Modelos faz, com o seu próprio pedido de confirmação, e
é uma decisão diferente desta.

- `UndeployRequest.group` vazio = "a pipeline que estiver no ar"; duas pipelines
  no ar é 400 - nunca adivinhar qual.
- O `undeploy_endpoint` re-varre o cluster (nunca confia na tela) e responde 200
  com um `UndeployOutcome { node_id, hostname, start_layer, end_layer, released,
  error }` por máquina, sempre - inclusive para a que não respondeu: um peer
  calado é `released: false` com o motivo em `error`, nunca contado como sucesso.
  Metade desimplantada é o estado real do cluster, e escondê-lo atrás de um erro
  deixaria o operador sem saber em quais máquinas ainda há memória presa.
- A liberação vai pelo `/api/ui/unload` do próprio peer (a mesma rota do botão de
  uma máquina só), que continua valendo para qualquer nó.

## 8. Aplicar e servir uma fatia (`webui.rs`)

`POST /api/ui/apply` (aplicado pelo `deploy` a cada alvo) leva um `ApplyRequest`:
`{ start_layer, end_layer, total_layers, peer_index, peer_count, primary,
skip_split, merge: Option<Vec<MergeSource>>, params }`. Os derivados que a máquina
precisa saber:

- `is_first`/`is_last` - papel na pipeline (a cauda tem a cabeça de saída).
- `is_last_peer`/`single_peer` - `single_peer` = `peer_count <= 1 && is_first &&
  is_last`.
- `no_embeddings` - auto-detect: só o último peer de uma pipeline multi-peer pula
  o embedding; um checkbox na tela força em todos.

`run_apply` decide o caminho:

- `merge` -> `merge_parts` (temp com sufixo único + rename no fim; reaproveita
  arquivo já completo e invalida o catálogo).
- `skip_split` ou `(is_first && is_last && peer_count <= 1)` -> serve o arquivo
  como está (a fatia já existe em disco).
- senão -> `extract_shard` (fatiamento local, job "fatiando", progresso por
  `set_job_progress`, `invalidate_catalog`, outcome `reused`/`gerado`).

Aplicada, a fatia é anotada em `<config>.last-slice.json` como `pending` pelo
`runtime::apply` - e só vira `serving` quando o `/health` do servidor de
inferência responde `model_loaded` (`spawn_health_watch`). É esse estado que
decide o que um reinício retoma (só `serving`), não o `apply`. Ver
`fatiamento.md` seção 6 e o `AGENTS.md` de `inference/`.

`/api/ui/plan` (`plan_split_endpoint`) planeja por orçamento do cluster e **delega**
ao peer dono do GGUF quando `layout_node_id` aponta para outra máquina: limpa o
marcador de hop e faz `POST {base}/api/ui/plan` - o peer que tem o arquivo é quem
calcula quantas camadas cabem na memória de cada um.

### Liberar uma fatia

`POST /api/ui/release` nunca é recusado (uma máquina pode sempre sair do modelo).
Com `reassign` (padrão), `find_release_candidate` acha outra máquina com a mesma
fatia para assumir; resposta `{ reassigned_to }` ou `{ uncovered: true }` quando
ninguém cobre - e o planejador recusa pipeline com buraco, então a tela avisa.

## 9. O que a tela mostra

- **Ajustes sempre abertos, à direita** - os doze números e interruptores que
  governam a
  implantação ficam visíveis junto da escolha de máquinas; escolher máquinas com
  eles escondidos é escolher às cegas.
- **Alternativas** - cada faixa mostra toda máquina possível como alvo com o
  motivo (`alternatives`), o que torna a troca de dono de uma fatia um clique.
- **A última implantação de cada modelo fica no `localStorage`** e volta
  preenchida, **depois** do primeiro plano novo - a memória é de outro dia, e o
  que não sobrevive ao plano atual (`usableSelection`) simplesmente não volta.
- **`merge` fundido aparece como uma linha** que nomeia a união (ex. `m_0_15.gguf`)
  e ocupa só o `select` das máquinas que não têm todas as partes.

## 10. Testes

Usam o testkit de `gateway` (`crates/node/src/backend/gateway.rs`, ver
`api-http.md`): `state_with` com a tabela de peers semeada à mão e um `fake_peer`
axum na loopback (no caso do undeploy, contando os hits em `/api/ui/unload`).

- **Plano**: `merge_joins_adjacent_slices_on_one_machine` (a linha da máquina
  fundida nomeia a união e não transfere bytes); um buraco no conjunto é
  reportado; slicings sobrepostos não são empilhados pelo planejador; um conjunto
  que ninguém tem é recusado.
- **Adicionar/remover**: `split_position` (máquina no meio não é a cauda; no fim
  é; trocar a mesma faixa mantém a contagem); sobreposição reportada mas não
  recusada; faixa que preenche um buraco não sobrepõe nada; sobreposição parcial
  reporta só as camadas compartilhadas.
- **Por máquina**: `per_machine_kv_cache_rides_along_with_the_slice` -
  `n_gpu_layers`/`offload_kqv` viajam com a fatia e não viram cluster-wide;
  `nvidia_memory_comes_from_vram_and_everything_else_from_ram`.
- **Undeploy**: `any_node_may_undeploy_for_now` documenta a porta aberta de
  propósito (ver defeitos).

## 11. Defeitos anotados

- **DEFEITO ANOTADO: undeploy não mexe no disco** (`deploy.rs:1766-1770`) - é
  deliberado e documentado no próprio código ("O que isto NÃO faz: mexer no
  disco"): desimplantar devolve memória, os arquivos continuam e a mesma
  implantação volta com um clique. Apagar fatias é a tela de Modelos, com pedido
  de confirmação próprio.
- **DEFEITO ANOTADO: `undeploy_authority` deixa qualquer papel desimplantar**
  (`deploy.rs:1794` retorna `Ok(())` para todo papel) - deliberado por ora; o doc
  da função já traz o corpo da restrição "somente o NÓ PRINCIPAL desimplanta" para
  quando chegar a hora, e o teste `any_node_may_undeploy_for_now`
  (`deploy.rs:2242`) documenta a porta aberta de propósito, para que fechá-la seja
  uma edição consciente e não um efeito colateral. O portão é o papel do nó que
  recebeu o clique, não de quem foi chamado - as chamadas aos peers usam
  `/api/ui/unload`, que continua valendo para qualquer nó. Registrado também em
  `crates/node/src/backend/AGENTS.md` (Defeitos anotados).
