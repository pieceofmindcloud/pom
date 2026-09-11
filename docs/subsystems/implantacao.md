# Subsistema: implantação na rede

O subsistema põe em serviço um conjunto de fatias que já existe, distribuindo-as
pelas máquinas do cluster. Vive em `crates/node/src/backend/deploy.rs`
(planejamento e sequenciamento) e usa `crates/node/src/backend/webui.rs`
(aplicação da fatia num nó, `apply`/`launch`/`release`), `crates/node/src/models/`
(catálogo, `plan_by_size`/`extract_shard`, `splitter::merge`) e
`crates/node/src/backend/transfer.rs` (movimentação dos bytes). Toda a lógica é
"a escolha do operador é parte do pedido": o nó sugere, o operador decide, e o
deploy re-varre o cluster em vez de confiar na prévia.

O registro durável de cada implantação fica em `pom.db`, em
`POM_DB_DIR` quando definido ou na raiz do projeto por padrão. O JSON validado
continua completo para permitir listar e recuperar a intenção do operador;
desimplantação marca o registro inativo e guarda um tombstone. A pasta legada
`deployments/` não é apagada nem escrita pelo runtime. A cópia opcional é feita
somente pelo script descartável `scripts/migrate_pom_db.py`.

## 1. Visão geral: o plano, a fusão e a ordem

Duas rotas fazem o trabalho grosso: `POST /api/ui/deploy/plan` (leitura da
topologia, chamada após edição estrutural) e `POST /api/ui/deploy` (executa;
re-varre o cluster). O pedido carrega `group` (o modelo original, pela chave técnica
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
de fatias cria limites novos de GGUF: para isso continua existindo a tela de
Fatiar. O modo de modelo inteiro é a exceção controlada: quando o operador pede
um prefixo para outro peer, o backend consulta as fronteiras reais do GGUF e
materializa as faixas no peer que possui o arquivo.

Na tela de implantação, trocar de fatias para `whole` limpa o mapa e não
escolhe uma máquina automaticamente. O operador adiciona a máquina desejada
pela faixa inferior; a interface mantém a regra de uma máquina para esse modo.
Mudanças de `RuntimeParams`, dos overrides de GPU/KV por máquina no inspetor e
do nome operacional não alteram a topologia: a tela não reinicia o `plan` nem
pede uma nova prévia de capacidade a cada controle. A chave do plano considera
somente faixas e máquinas; os overrides continuam nas linhas locais e entram
no deploy. A projeção antiga fica oculta até o `POST /api/ui/deploy`, que faz o
preflight completo com os valores atuais antes de iniciar a subida.

## 1.3 Capacidade de saída derivada do runtime

O campo `max_new_tokens` dentro de `params` é o tamanho de uma perna do
backend. Não existe um segundo teto absoluto no `pom.json`: o orçamento total
da requisição vem de `max_tokens` do cliente e do espaço restante da janela
`n_ctx` do runtime ativo. O gateway costura as pernas invisivelmente até o
cliente parar, atingir seu orçamento ou o contexto não comportar mais saída.

Passo operacional exato antes de testar uma implantação longa:

1. Na tela de implantação, defina `n_ctx` e `max_new_tokens` em `params` para
   os valores que o captain escolheu, por exemplo `n_ctx: 100000` e
   `max_new_tokens: 32000`.
2. Aplique a implantação e aguarde o `/health` de **cada peer** confirmar
   `model_loaded` e `runtime_capacity`. Um peer que ainda está reiniciando não
   entra na capacidade publicada.
   O `/health` do gateway continua respondendo durante o enriquecimento frio do
   catálogo; a sonda opcional do Ollama tem prazo curto e não transforma esse
   trabalho em bloqueio do readiness HTTP.
3. Consulte `/v1/models` autenticado e confirme
   `context_length: 100000` e
   `top_provider.max_completion_tokens: 100000` quando esse for o contexto
   efetivo. Exemplo sem expor a chave:

   ```bash
   curl -fsS -H "x-api-key: ${POM_API_KEY}" \
     http://ksia:8080/v1/models | jq '.data[] | {id, context_length, max_completion_tokens: .top_provider.max_completion_tokens}'
   ```

O gateway anuncia o menor `n_ctx` confirmado entre os peers da pipeline
planejada do modelo e mantém `max_new_tokens` apenas como tamanho de perna. Um
peer ocioso escolhido pela pipeline é acordado por `reload_model` antes do
prefill. Todos os peers ociosos planejados recebem o evento em paralelo, e a
barreira espera o `/health` por até 15 minutos para que uma carga fria conclua;
se esse prazo vencer, a requisição falha com erro explícito. Peers vivos de
rotas alternativas não bloqueiam a capacidade. O
limite local do cliente, como `maxTokens: 5000` no Pi, continua independente
dessa capacidade. Um `pom.json` legado que contenha o campo removido é recusado
na leitura, em vez de ser aceito com um valor padrão escondido.

Três regras saem da arquitetura, não do gosto:

- **Ordem do pipeline = ordem das camadas.** Quem fica com a camada 0 recebe o
  prompt inteiro de cada requisição. Escolher a ordem é o mesmo ato que escolher
  quem fica com qual faixa - não há segundo botão.
- **Um nó pode servir várias faixas distintas.** Cada faixa válida vira uma
  etapa do pipeline, mesmo quando a mesma máquina recebe faixas separadas.
  Com `merge` ligado, apenas faixas adjacentes no mesmo nó viram uma unidade
  fundida (`splitter::merge`, Rust), a máquina junta as fatias num arquivo só e
  serve a união. O passo fundido carrega `parts`, um por fatia.
  A mesma faixa exata em nós diferentes é uma réplica do mesmo estágio e forma
  uma rota paralela válida.
- **Sobreposição parcial e buraco global são recusa.** Duas faixas com cortes
  diferentes sobre a mesma camada tornariam a resposta ambígua; cópias exatas
  são ignoradas na cobertura lógica e ficam disponíveis para o roteador. Uma
  atribuição válida precisa cobrir cada camada exigida exatamente uma vez.

## 1.4 Proteção térmica da implantação

Cada nó grava `thermal` no `pom.db`, com `limit_c` padrão de 85 °C,
`hysteresis_c` de 8 °C e `cooldown_secs` de 30 segundos. O monitor usa a mesma
lista `SystemLoad.gpus` que a tela exibe. Uma leitura conhecida acima do limite
descarrega o modelo, remove o anúncio das camadas e publica o estado
`thermal`; a configuração da implantação fica preservada para a retomada.

A retomada só acontece quando todas as GPUs têm leitura conhecida no máximo em
`limit_c - hysteresis_c` (77 °C no padrão) e permanecem nessa condição durante
o cooldown. Leitura ausente, falha do driver ou GPU sem sensor não dispara
desimplantação e também não prova que é seguro religar.
`POST /api/ui/stop-inference` limpa a recuperação pendente, para que uma
desimplantação manual não seja anulada pelo monitor. Cada transição registra
temperatura observada, limites e tempo no log do servidor.

## 1.5 Descarregamento quando o peer fica ocioso

`RuntimeParams.unload_if_idle` e `idle_minutes` são persistidos por deployment.
Quando habilitado, cada peer registra localmente a última requisição em
`backend/reqlog.rs`. O monitor existente, fora do caminho quente e sem criar
timer ou `sleep` de inferência, avalia esse carimbo e chama o descarregamento
local. O deployment continua ativo: o peer vivo anuncia `idle`, com a faixa
preservada, em vez de `serving`. A identidade do deployment continua no
catálogo de modelos enquanto todos os peers correspondentes estiverem
`idle`, mesmo sem `runtime_capacity`; isso permite que uma requisição por nome
seja planejada e acorde os peers antes do primeiro salto. Um peer em
reinicialização (`runtime_capacity` ausente sem `idle`) continua fora do
catálogo até confirmar o `/health`.

Na primeira requisição depois de `idle`, o gateway recarrega o runtime local e
reinicia a atividade. Envia em paralelo o evento autenticado `reload_model` a
cada peer ocioso planejado; o evento é idempotente para uma segunda requisição
que acorde a mesma máquina, e cada destinatário carrega sua própria fatia.
Assim, a primeira requisição pode esperar a carga, mas os hops não são
aquecidos sequencialmente nem ficam esperando uns pelos outros. O status volta
a `serving` quando o `/health` confirma o modelo carregado. `idle` é por peer,
não por deployment: uma máquina ociosa não muda o estado das demais.

## 1.1 Modelo inteiro: o caso degenerado, não um segundo caminho

`kind: "whole"` (`DeployKind`, `deploy.rs`) implanta **um GGUF não fatiado em
uma ou mais máquinas**. Cópias da faixa completa são réplicas do mesmo estágio
e rotas paralelas válidas para balanceamento por RTT/carga. Não existe segundo fluxo: `survey` lê o modelo do `models` do
`/api/ui/models` de cada peer (em vez do `shard_sets`) e o converte, em
`parse_whole`, num `HeldShard` de faixa `0..total_layers-1`. Daí para frente é
tudo o mesmo código - `candidates`, `model_size`, a aritmética de cobertura, o
guarda de disco/memória, a fase de cópia e `launch_targets`. A cobertura fecha
por construção, "um nó serve uma ou mais faixas" é trivialmente satisfeita, e cada alvo sai
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

O planejamento de um modelo inteiro não espera o catálogo completo. Se o
inventário rápido já mostrou o arquivo, mas a resposta ainda não tem
`total_layers`, o survey pede `POST /api/ui/model-metadata` nesse peer e lê
somente o cabeçalho do caminho selecionado. O restante da pasta pode continuar
com `metadata_complete: false`; isso não transforma vizinhos sem relação em
pré-requisito para a implantação.

### 1.1.1 Escolha de camadas a partir do modelo inteiro

`POST /api/ui/deploy/plan` pode receber `whole_slice` com
`{source_node_id, target_node_id, end_layer}`. A origem precisa anunciar o
modelo inteiro selecionado. `model-layer-options` lê o GGUF nessa origem e
devolve os fins de prefixo aceitos: qualquer fim de camada inteira em layout
denso, ou somente as fronteiras arquiteturais de `arch_boundaries` em layout
híbrido. O frontend usa esses pontos como posições do slider; nunca converte o
slider em um passo fixo.

Ao executar, `deploy` cria uma faixa virtual para o prefixo e outra para o
restante, chama `slice-ranges` na origem e espera o fatiador selar um manifesto
com `sha256` antes de iniciar `transfer/pull`. Se uma faixa já existir no
destino, ela permanece na lista de camadas disponíveis e não há slider para
ela. Falha ou ausência de leitura do GGUF é uma recusa nominal, com o motivo
visível no inspetor.

O preflight também confere o contrato de saída que o loader exige. Para a
arquitetura `deepseek4`, `llama.cpp` exige `token_embd.weight`,
`output_norm.weight`, `output.weight`, `output_hc_fn.weight`,
`output_hc_base.weight` e `output_hc_scale.weight` em uma instância carregável;
como ele não conhece `pom.start_layer` nem `pom.end_layer`, a origem copia os
pesos reais da cabeça para cada faixa não final antes de iniciar a implantação.
O preflight recusa somente quando a origem não permite essa cópia, listando o
tensor ausente ou a causa concreta. Contagens de camadas que o loader compara
com o índice local também são rebaseadas para a faixa; `nextn_predict_layers`,
que conta a partir do fim, fica zerada fora da faixa final.

## 1.2 Tensor parallel: grupo de ranks

O caminho TP prefere a rodada já fatiada em `shard_sets[].tp_parts`. Cada
parte cobre o modelo inteiro e carrega `tp_rank` e `tp_size`; a interface agrupa
por largura e mostra `1of2`, `2of2`, ou `1of4` até `4of4`. Essas partes não são
etapas de pipeline e não podem virar uma única faixa `L0-31`.

`kind: "tensor_parallel"` (`DeployKind::TensorParallel`) implanta um modelo num
grupo de máquinas por tensor parallelism: um **primary** (dono do GGUF e hop do
pipeline) + K **workers** (cada um extrai o próprio shard de tensor da própria
cópia local). O pedido carrega `tp` (`TpLaunch`): `{size, primary_node_id,
worker_node_ids}` - o operador escolhe o **primary** e os **workers** por
node_id entre as máquinas no mapa (a tela oferece selects, não um campo de URL), e o nó deriva a
`rank0_url` da porta de inferência anunciada pelo próprio primary
(`Machine.inference_url`) e preserva os demais candidatos de endereço. A
Quando não há `tp_parts`, a varredura lê o modelo inteiro do primary, e o
`launch_targets` expande o passo em **um alvo por rank** (o primary com rank 0 e cada worker com rank 1..size-1),
todos apontando a mesma `rank0_url` e sua lista de candidatos para a coletiva.
Quando há `tp_parts`, `survey` resolve cada alvo pela faixa, pelo rank e pelo
tamanho do grupo. Se o rank já está no alvo, não há cópia; caso contrário o
planner encontra a mesma parte em outro peer e transfere pelo `sha256`.
Quando a seleção explícita também lista um worker como se fosse uma etapa de
pipeline, o planejador remove essa linha antes de montar `steps`: worker é
somente rank da coletiva e cada máquina entra uma vez em `launch_targets`.
Se o túnel libp2p não puder discar, o rank tenta cada endereço HTTPS conhecido
do destino, o que cobre peers atrás de NAT e containers no mesmo host.

Antes de aceitar, o plano valida em duas camadas: `tp_group_checks` (tamanho,
primary/workers no cluster, ranks disponíveis, sem repetição e URLs de
inferência) roda no próprio `plan`; quando usa modelo inteiro,
`tp_shardability_check` pergunta ao primary (rota `POST /api/ui/tp/check`) se o GGUF dele dá para partir - MoE/SSM/
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

O lado de `apply` diferencia os dois formatos. Para `tp_parts`, o `.pom` é
buscado e mesclado antes dos bytes, cada alvo aponta para o arquivo já existente
e `skip_split: true` impede uma nova extração. Sem partes pré-fatiadas,
`ApplyRequest.tp` (a `TpIdentity` de `network.rs`) faz **cada rank** extrair o
próprio shard por `plan::extract_tp_shard`, inclusive o rank 0. A
`TpIdentity` carrega ainda `members` - todos os ranks em
ordem, com `node_id` e URL de inferência -, porque **o rank 0 precisa pôr os
workers no mesmo decode**: TP só funciona em lockstep, e o gateway fala só com o
rank 0. Membro sem porta de inferência anunciada é recusa no plano
(`tp_members`), não travamento na primeira barreira. O nó também anuncia o papel via
`Network::set_tp` e registra a coletiva (`register_tp_reduce`). Ver
`docs/subsystems/fatiamento.md` (seção do shard por tensor) e
`docs/subsystems/rede-p2p.md`.

Cada membro também carrega o `PeerId` libp2p, os candidatos UDP anunciados e a
capability do túnel pelo servidor. Assim, o `broadcast_to` do rank 0 e o reduce
da coletiva podem reidratar o swarm mesmo quando o processo que executa o rank
não recebeu o mesmo anúncio, e repetir a chamada pelo swarm quando as máquinas
estão em redes sem rota HTTPS direta.

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
  diferentes. Uma parte TP também carrega `tp_rank` e `tp_size`; partes com a
  mesma faixa só são deduplicadas quando o rank e a largura também coincidem.
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

`holds(start, end)` acha uma fatia comum e `holds_tp(start, end, rank, size)`
acha o rank TP exato. `overlapping_layers` acha sobreposição entre uma faixa e
a coleção de outra.

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
  - contexto, batch, pedaço e precisão do estado oculto decidem o orçamento de
  prompt que o gateway cobra, e a amostragem só roda no último peer. A precisão
  é `f16` por padrão e pode ser escolhida como `f8` para medição.
- **Capacidade de precisão**: `f8` só pode ser implantado quando todos os peers
  selecionados anunciam `inference/frame-v1-dtype-f8` em
  `inference_frame_capabilities`. O planner bloqueia o deploy com a razão do
  peer incompatível; a validação no caminho de inferência mantém a mesma
  proteção contra configurações divergentes depois do deploy.
- **Ajuste automático de memória**: `RuntimeParams.fit_memory` fica ligado por
  padrão em `shards` e `whole`. O nó consulta a memória livre e total do
  backend por dispositivo e usa `RuntimeParams.fit_memory_margin_mb` para
  definir a reserva de segurança de cada um, padrão 256 MiB. O livre já vem
  descontado dos processos existentes, então essa margem não é uma segunda
  cobrança da ocupação. O `common_fit_params` do llama.cpp roda antes da carga
  usando a capacidade total `n_ctx * max_sessions`, e pode reduzir as camadas
  GPU para evitar OOM. Um
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
laço de agrupamento anda em ordem de camada e só junta entradas consecutivas
quando `merge` está ligado, então faixas separadas na mesma máquina continuam
etapas independentes. Pela mesma razão **`target_node_id` vazio nunca agrupa**:
"sem máquina" não é uma máquina, e cada faixa sem destino mantém seu próprio
passo e motivo.

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

### Visão opcional

O catálogo registra `mmproj_path` quando encontra, no mesmo diretório, o GGUF
vizinho que segue a convenção do llama.cpp. Para uma rodada de fatias, o
`ShardSet` também expõe o projetor adjacente a uma fatia ou ao modelo inteiro
de origem identificado por `model_label`; o `Plan` resolve o caminho para o
alvo escolhido na camada 0 sem uma nova varredura do disco. Sem esse caminho,
a tela mantém a visão indisponível e o backend recusa `vision: true` com a
mensagem de que nenhum `mmproj` foi encontrado.

`DeployRequest.vision` só pode ser marcado quando o plano tem projetor. Nesse
caso, `deploy::lifecycle` marca `use_mmproj` exclusivamente no alvo cujo
`start_layer` é `0`. Só a cabeça carrega `--mmproj <caminho>` e transforma a
imagem em estado oculto; os peers seguintes recebem esse estado e nunca tocam
na imagem crua. A escolha e o caminho local são persistidos em `NodeConfig`,
para que o chat e as rotas de administração conheçam a visão ativa.

### O modelo e seu tamanho

`model_size` resolve o total de camadas do modelo com precedência
**Declared > Inferred > Unknown**: uma fatia que declara `pom.original_block_count`
manda; sem declaração, o total é deduzido do alcance da cobertura no cluster
inteiro (nunca chute de uma fatia só); sem nada disso, `Unknown`. Isso é o que
impede o bug de 2026-08-10, quando o plano se dizia completo e todo `apply` morria
em `NodeConfig::validate` depois de o `launch` já ter parado a inferência das
máquinas alvo. Fatias sem total são nomeadas "L{}-{} em {}" (`nameless`) e só
aparecem assim.

## 3.1 Pré-visualização de capacidade (VRAM projetada)

Depois que `plan_endpoint` monta os passos (e só se `blocked` está vazio),
`enrich_capacity_previews` (`backend/deploy.rs`) pergunta a cada alvo já
materializado localmente como aquele passo ocuparia a VRAM dele com os
parâmetros deste plano. É o que a tela Implantar usa para desenhar os cards
como um **preview do sistema depois da implantação**, não como o estado de
agora - uma barra por GPU, uso projetado contra o total, que se recalcula a
cada ajuste de contexto, lote ou sessões (debounce de 260 ms e um round trip,
como o resto do plano).

- **Uma chamada por `Step`** (`POST /api/ui/deploy/capacity-preview`, handler
  em `webui.rs`), só para passos `!merged` com `target_path` resolvido - um
  passo que ainda depende de cópia ou de junção não tem arquivo local para a
  conta examinar, e a pré-visualização nunca materializa nada. Passos em espera
  aparecem como "projeção após a cópia". Tensor Parallelism fica de fora nesta
  versão, pois a divisão real é `split-mode tensor`.
- **Estimativa analítica.** A rota lê `gguf::read_layout` e `gguf::read_info`,
  cobra peso e KV com `plan::range_cost` e distribui o resultado
  proporcionalmente à VRAM livre atual de cada GPU. A quantização do cache
  entra: `cache_type_k`/`cache_type_v` saem da config da instalação e o KV só
  é somado à VRAM quando `offload_kqv` está ligado. Sem isso, o cache mora na
  RAM do host e a projeção anota a diferença. O executor externo de MoE fica
  de fora porque a rota não abre o modelo nem calcula ownership de experts.
  O compute buffer também fica fora: o resultado é um sinal de planejamento,
  não uma substituição da carga real.
- **Divisão multi-GPU.** Sempre proporcional à VRAM livre atual. A resposta
  identifica a divisão como estimada e o carregamento real continua sendo a
  autoridade sobre camadas efetivas, contexto e layout final. A rota nunca
  chama `llama_backend_init`, `llama_rs_fit_params` ou `fit_plan`, e por isso
  não compete com a carga nativa.
- **Três estados visíveis, nenhum bloqueio.** Coerente com a regra "memória é
  estimativa" (`a_target_short_on_memory_warns_and_still_deploys`): **verde**
  cabe na projeção analítica; **amber** (`fits && degraded`) a conta crua não
  cabe, mas `fit_memory` está ligado e as camadas foram deixadas no padrão, de
  modo que a carga real pode aparar o pedido; **rósea** (`!fits`) a conta não
  cabe e não há ajuste automático autorizado. O amber é uma heurística explícita,
  não uma alegação sobre o resultado do llama.cpp. Os três estados são aviso e
  a tela ainda pede `Confirm` antes de implantar.

  O endpoint admite quatro prévias por processo. Quando todas as vagas estão
  ocupadas, responde `429` imediatamente; o coordenador limita seu fan-out a
  quatro chamadas simultâneas com `buffer_unordered`, sem formar fila nativa.

  Rósea abre um `Confirm` nomeando as máquinas antes de implantar. `canDeploy`
  não muda - buraco de cobertura e `blocked` continuam sendo os únicos
  bloqueios de verdade; a capacidade continua sendo apenas aviso.

`MachineView.capacity` (`MachineCapacity { fits, degraded, note, gpus }`) é a
soma por GPU dos passos daquela máquina - dois passos não-adjacentes na mesma
placa somam. `Step.capacity` (`CapacityPreview`) é o passo isolado.

## 4. Execução (`deploy_endpoint` -> `run`)

1. `deploy_endpoint` re-varre (`survey`), re-planeja, recusa `blocked`/vazio,
   cria o job ("Implantar {deployment_name} em {n} nós") e spawna a execução.
2. `run` monta a lista de `Pull`s - uma unidade de movimento por fatia que tem
   que chegar a um alvo. Para TP pré-fatiado, a unidade é cada `TpTarget` que
   ainda não tem o rank no destino. Para passo fundido, é um `Pull` por part
   faltante; a máquina "tem" a unidade fundida quando já tem todas as partes,
   e aí o merge roda localmente com nada cruzando a rede. Um rank ou fatia que
   não tem de onde vir é erro.
3. Os bytes viajam pelo transporte de transferências (ver `transferencia.md`):
   HTTP `Range` com retomada por `.part`, MD5 conferido antes do arquivo tomar o
   nome final, progresso no sino vindo dos nós (fechar a aba não interrompe
   nada). Para uma parte por hash, o receptor busca e mescla o `.pom` antes dos
   bytes de peso. A checagem de disco da implantação conta a **união** das fatias mais o
   que falta - a máquina precisa de espaço para o modelo junto *e* para as partes.
   O limite da pasta de fatias tolera subdiretórios filhos que o usuário do nó
   não consegue ler: soma o que foi possível medir e registra cada entrada
   ignorada em `debug`. Se a própria raiz não existe, não é diretório ou não
   pode ser lida, o `apply` falha informando o caminho, a causa e a orientação
   para corrigir a pasta ou suas permissões.
   Como a soma pode subestimar bytes que ficaram inacessíveis, ela serve apenas
   ao limite configurado da pasta; a capacidade física continua sendo conferida
   pelo espaço livre do sistema no `survey` e novamente antes de cada cópia em
   `transfer.rs`.
4. Cada alvo recebe o `ApplyRequest` via `apply`/`launch` (detalhe na seção 6).
5. `done` = "o modelo está atendendo na rede"; erro vira job `failed`.

## 5. Rotas do deploy

| Rota | Papel |
|---|---|
| `POST /api/ui/deploy/plan` | prévia (leitura, chamada a cada edição); enriquece os passos com a projeção de capacidade |
| `POST /api/ui/deploy/capacity-preview` | prévia por passo, local ao peer: estima por metadata GGUF e VRAM livre, sem chamar o fit nativo, e devolve a VRAM projetada por GPU (`webui.rs`) |
| `POST /api/ui/deploy` | executa (re-varre, recusa `blocked`/vazio, job) |
| `POST /api/ui/deploy/node/options` | catálogo e opções locais da máquina escolhida para uma alteração incremental |
| `POST /api/ui/deploy/node` | adiciona uma máquina a um pipeline já no ar |
| `POST /api/ui/deploy/node/remove` | tira uma máquina do modelo (libera a memória) |
| `POST /api/ui/undeploy` | tira a pipeline inteira do ar |

## 6. Adicionar e remover máquina

`add_node_options_endpoint` revarre somente o catálogo real da máquina
selecionada e devolve as fatias materializadas que ela tem, o modelo inteiro
correspondente quando existe e os defaults locais de GPU/KV. Ao trocar a
máquina no Painel, a tela refaz essa consulta e não reaproveita as opções de
outro peer nem o plano antigo enquanto o inventário ainda está sendo lido.

`add_node_endpoint` recebe uma opção + máquina alvo e chama `webui::launch` com
um único `LaunchTarget` (`skip_split: true` - o arquivo já existe e o passo é
só carregar). `cache_type_k`/`cache_type_v` são aplicados no `pom.json` local
ou pela API do peer antes do launch; `n_gpu_layers` e `offload_kqv` seguem como
overrides efetivos da máquina. Dois detalhes escondidos na chamada:

- **`peer_index`/`peer_count`** - a posição é a do pipeline, não "este nó sozinho":
  sem isso o nó somado se diz o último da divisão.
- **`primary_node_id`** - quem é a entrada do pipeline não muda por somar um nó:
  só quem fica com a camada 0 recebe o prompt. `primary` só é marcado quando
  `start_layer == 0`.
- Resposta `{applied, overlapping_layers, peer_index, peer_count}` - sobreposição
  é reportada, não recusada (a máquina entra e a tela mostra o que colide).

Quando o pedido identifica um deployment persistido, a resposta de
`add_node_endpoint` também traz `deploy_id`, `version` e `kind`. Sobreposição é
reportada, não recusada. Uma máquina inteira e uma ou mais fatias podem
coexistir no mesmo deployment, que passa a usar `kind: "mixed"` e expõe as
duas cabeças de camada 0 como entradas possíveis para o mesmo modelo.

`remove_node_endpoint` ("essa máquina não está servindo este modelo" se o alvo não
serve) calcula `uncovered_after_removal` contra as máquinas restantes. **Quem
libera a memória é o dono dela**: alvo == self usa `runtime.stop()`; senão
encaminha `POST /api/ui/unload` ao gateway do alvo (a mesma rota do botão de
descarregar uma máquina). Resposta `{removed, uncovered_layers}`; vazio significa
que outra máquina cobre a mesma faixa - a rede perde uma rota paralela e segue
servindo; não-vazio é buraco, e a confirmação é explícita. A resposta também
informa `deploy_id`, `version` e `serving`. A remoção anexa uma versão com o
alvo fora do conjunto. Enquanto restar uma máquina, `serving` continua
verdadeiro; a última remoção grava uma versão ativa vazia, encerra a seleção
como deployment vivo e preserva todo o histórico.

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

## 7.1 Reimplantar o mesmo modelo, e as versões de uma implantação

Uma implantação é **versionada** (`Deployment`, `DEPLOYMENT_VERSION = 2`).
`versions` guarda cada configuração que já rodou sob o mesmo `deploy_id` e
`active_version` diz qual está no ar; `nodes` é a lista achatada da versão
ativa. O que distingue uma versão é o `RuntimeParams` do deployment inteiro -
ajustes por máquina (`n_gpu_layers`/`offload_kqv`) entram no snapshot da versão
mas não forkam versão. Um registro v1 legado (sem `versions`) ganha a v1
sintetizada por `ensure_versions` em toda leitura.

Alterações incrementais de topologia são eventos de histórico independentes dos
parâmetros: adicionar ou remover máquina usa `append_version`, com
`active_version` novo e o conjunto de `nodes` daquela versão. O coordenador
publica o snapshot aos participantes atuais e aos peers que já conheceram o
deployment, para que uma máquina removida limpe seu registro sem descarregar
nem reiniciar as demais.

`POST /api/ui/deploy` faz uma pré-verificação antes de copiar qualquer arquivo
ou chamar `launch`. Para fatias, a prova é o `pom_id` do manifesto; para um GGUF
inteiro, é o SHA-256 calculado no arquivo selecionado. O nome do modelo nunca é
prova. Registros antigos sem identidade só podem usar o caminho canônico quando
o caminho pertence ao mesmo peer; se não houver prova, a ação é recusada com o
motivo visível e nenhuma máquina é descarregada.

**Reaproveitar o `deploy_id`.** Antes de cunhar um id novo, `deploy_endpoint`
procura no `pom.db` **local** uma implantação de mesma **forma**
(`deployment::matching_shape`): mesmo `kind`, mesma `DeploymentIdentity`
provada e o mesmo conjunto de `(node_id, faixa, tp_rank)`. Achou:

- `params` iguais aos de uma versão registrada -> aquela versão volta a ser a
  ativa, sem anexar outra;
- `params` diferentes -> **mesma `deploy_id`, versão nova** anexada
  (`upsert_version`).

Sem identidade provada, ou forma diferente (uma faixa ou máquina mudou), cunha
um `deploy_id` novo - trocar de máquina ou de faixa é assunto do Deploy Studio,
não uma versão. Quando a implantação ativa é comprovadamente a mesma, o fluxo
chama `/api/ui/unload` em todos os peers correspondentes e consulta
`/api/ui/node` até cada um anunciar `config: null`, estado `disabled` e nenhum
modelo ou deployment ativo. A espera tem limite de 30 segundos por rodada; se um
peer continuar ocupado, o job falha nomeando a máquina e seu último estado.
Falha de unload também bloqueia a nova implantação e preserva no job o motivo
retornado. A resposta do endpoint carrega `deploy_id` e `reused`.

### `POST /api/ui/deployments/:id/redeploy` (a tela "Implantações")

`RedeployRequest` carrega `params` (opcional), `version` (opcional) e
`per_node`. `version` reimplanta uma versão já registrada, sem anexar outra;
`params` novos reimplantam anexando - ou reativando - uma versão pela mesma
comparação do `deploy_endpoint`. Informe um dos dois (`redeploy_endpoint`
recusa com 400 quando faltam os dois ou a `version` não existe).
`RedeployRequest::resolve` muta o registro para refletir a versão escolhida e
devolve os parâmetros base; a subida usa `no.config` da versão ativa (os
parâmetros com os ajustes por máquina embutidos), então reimplantar uma versão
devolve a cada máquina exatamente o que ela rodava.

Mesma implantação, mesmo `deploy_id`, mesmas máquinas e faixas - **não
re-planeja** (re-planejar cunharia um `deploy_id` novo e quebraria clientes
presos a `modelo@deploy_id`). `run_redeploy` (`deploy.rs`):

1. **Fotografa** o `NodeConfig` vivo de cada nó (`model_path`, `model_label`,
   `deployment_name`, `no_embeddings`) - o registro guarda faixa, posição,
   `part_sha256` e `source_model_path` (o arquivo que aquele nó carregou), mas
   um registro antigo pode não ter nenhum caminho.
2. **Descarrega** toda máquina que ainda anuncia este `deploy_id`
   (`release_one` em paralelo, o mesmo `/api/ui/unload` do undeploy). Uma
   máquina órfã que saiu do plano também é liberada aqui.
3. **Sobe uma de cada vez**, esperando cada uma reportar `ready` antes da
   próxima (`wait_for_peer_serving_with_timeout`, teto de 30 min): sequencial
   de propósito, para não empilhar picos de VRAM. Cada nó recebe
   `/api/ui/reconfigure` com um `ReconfigureSlice` montado da fotografia + do
   registro; `reconfigure_local` reconstrói o `NodeConfig` a partir dele
   **sirva o nó algo ou não**. O arquivo é resolvido em três degraus: o
   `model_path` fotografado (ou `source_model_path` do registro); o
   `part_sha256` (`transfer::find_part`); e, sem hash de parte - modelo
   inteiro ou fatia de rodada antiga -, o catálogo local casando rótulo do
   modelo + faixa de camadas (`resolve_deployment_slice_file`). Um nó que não
   tem mais o arquivo por nenhum dos três devolve um erro nomeando a faixa,
   não um 500 mudo.
4. Grava no registro só os parâmetros que cada máquina aceitou, sincroniza o
   snapshot da versão ativa (`commit_active_nodes`) e publica.

O job carrega o resultado por máquina em `outputs` (`"<host>: <motivo>"`), que
a tela de Implantações mostra abaixo da contagem. Tensor Parallelism mantém o
caminho antigo (`reconfigure` no lugar, sem unload): a coletiva não sobrevive
a um unload e o registro não guarda o `TpLaunch` inteiro. O `resolve` das
versões roda antes do desvio para o caminho TP, então versão vale para os dois.

Limite conhecido: a busca de forma do `deploy_endpoint` olha só o `pom.db`
local. Um coordenador que não é participante da implantação existente não a
encontra e cunha id novo; a tela "Implantações", que junta os registros do
cluster, não tem esse limite.

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

- `merge` -> `merge_parts` (temp com sufixo único; reconstrói a metadata por
  camada, valida o GGUF final com `read_layout` antes do rename e de
  `finalize_run`, reaproveita arquivo já completo e invalida o catálogo).
- `skip_split` ou `(is_first && is_last && peer_count <= 1)` -> serve o arquivo
  como está (a fatia já existe em disco).
- senão -> `extract_shard` (fatiamento local, job "fatiando", progresso por
  `set_job_progress`, `invalidate_catalog`, outcome `reused`/`gerado`).

Aplicada, a fatia é anotada em `<config>.last-slice.json` como `pending` pelo
`runtime::apply` - e só vira `serving` quando o `/health` do servidor de
inferência responde `model_loaded` (`spawn_health_watch`). Quando o monitor de
ociosidade descarrega o modelo, anota `idle` e mantém a implantação ativa para
que `reload_model` possa restaurá-la. É esse estado que decide o que um
reinício retoma (`serving` ou `idle`), não o `apply`. Ver
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
- **Versões** (`models/deployment.rs`):
  `ensure_versions_sintetiza_a_v1_de_um_registro_legado`,
  `upsert_version_reusa_a_versao_de_params_iguais_e_anexa_quando_diferem`,
  `set_active_version_troca_os_nos_e_recusa_versao_inexistente`,
  `matching_shape_casa_por_identidade_e_faixas` +
  `matching_shape_ignora_registro_sem_identidade`,
  `versoes_sobrevivem_ao_serde`. Em `deploy.rs`:
  `redeploy_request_resolve_escolhe_ou_cria_versao` e
  `redeploy_de_versao_inexistente_e_recusado`.

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
- **DEFEITO ANOTADO: interferência na bateria paralela do plano HTTP** -
  `deploy::whole_route_tests::the_route_plans_a_whole_model_from_the_machines_own_catalog`
  falha na bateria completa paralela, mas passa isoladamente e com
  `--test-threads=1`. A causa é o par dessa rota com
  `webui::a_lista_de_maquinas_para_implantar_tambem_e_pinada` ou
  `webui::cluster_models_fala_com_os_peers_pelo_cliente_pinado`: esses testes
  alteram o global `peer_client::TLS_ATIVO`; a rota não segura
  `peer_client::POLITICA_GLOBAL`, então pode formar `https://` para o servidor
  HTTP falso e interpretar a falha como catálogo sem o modelo. A falha também
  ocorre na base `0adfa5b`, portanto não é regressão dos fixes do fatiador.
