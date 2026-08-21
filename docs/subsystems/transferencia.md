# Subsistema: transferência de arquivos (fatias) entre máquinas

O subsistema move fatias GGUF entre máquinas do cluster, inteiro em
`crates/node/src/backend/transfer.rs`. Há dois sentidos que terminam no mesmo
download: **puxar** (este nó pede uma fatia a um peer) e **oferecer** (o dono
da máquina escolhe um arquivo, o peer aceita e então puxa). Os bytes viajam
pelo gateway HTTP já existente dos peers, com `Range` para retomada - não pelo
bitswap do libp2p, que saiu do swarm por ser protocolo órfão (ver `rede-p2p.md`).

Onde o arquivo cai depende do que ele é. Uma **parte** de um fatiamento vai para
`<shard_dir>/<modelo>-<hash curto do original>/<sha256>.gguf`, com o manifesto
`model.pom` gravado ao lado antes dos bytes, e é conferida pelo hash que foi
pedido. Um arquivo que não é parte de manifesto nenhum - um modelo inteiro -
cai em `<shard_dir>/recebidos/` e só toma o nome final quando o MD5 confere.

Ao receber um manifesto v2, o peer faz merge por `pom_id`: partes locais são
preservadas e partes remotas ausentes são adicionadas. Isto vale tanto para
`parts` quanto para `tp_parts`; nunca se substitui o manifesto local por uma
visão parcial do peer que enviou.

**O `.pom` é obrigatório para uma parte, e nos dois sentidos.** Quem manda recusa
oferecer uma fatia sem manifesto (`send_to_peer`), quem serve recusa entregá-la
(`servable_path`), e quem recebe recusa aceitá-la (`alvo_da_oferta`,
`spawn_download`). Não há caminho alternativo: uma fatia órfã é um arquivo que
ninguém sabe nomear, e o hash do modelo original - que decide em que pasta ela
mora - só existe no manifesto.

## 1. Os dois sentidos, e a regra de quem escolheu o arquivo

O que pode sair de uma máquina depende de **quem escolheu o arquivo**:

- **Puxar** (`POST /api/transfer/pull`): este nó pede a um peer um caminho que
  ninguém do outro lado escolheu. Por isso o caminho é confinado ao `shard_dir`
  (`models/` por padrão) via `safe_model_path`, que canonicaliza e exige
  `starts_with` no diretório raiz. O pedido viaja com o caminho, porque é um
  arquivo de catálogo.
- **Oferecer** (`POST /api/transfer/send`): a tela mostra somente arquivos
  `.gguf` presentes no catálogo local e envia o basename em `filename`. O
  backend varre novamente as pastas configuradas, exige uma correspondência
  única e só então resolve o caminho real. Caminhos absolutos, `..`, nomes
  duplicados, arquivos ausentes e extensões diferentes de `.gguf` são recusados.
  O peer nunca vê esse caminho: recebe só um id opaco (ver concessão abaixo) e
  busca por `?grant=<id>`.

Os dois sentidos passam por `ensure_gguf`, que exige a assinatura `GGUF` nos
primeiros 4 bytes **e** extensão `.gguf`. Blobs sem extensão do store do Ollama
não são transferíveis por esta tela.

## 2. A concessão (grant)

`TransferState::grants` é um mapa `id → PathBuf` criado **apenas** por
`send_to_peer`, que roda na tela desta máquina. O corpo da oferta leva o `grant`
e o receptor o devolve em `/api/transfer/manifest` e `/api/transfer/download`
via `?grant=<id>`. É o que deixa um arquivo de fora de `models/` sair sem abrir
o disco para quem tem a chave: o caminho é sempre a escolha local de alguém,
nunca um pedido remoto. O guarda `servable_path` também roda `ensure_gguf` no
caminho da concessão - era a única porta sem checagem de tipo.

## 3. O download: `.part`, `Range`, MD5

Aceitar uma oferta (`POST /api/transfer/offers/:id/accept`) **volta antes de o
outro lado responder**. `spawn_download`:

1. Registra o job na hora (`bytes_total` 0, barra indeterminada, detalhe
   "conferindo o arquivo na origem"). O diretório de destino - a pasta do
   modelo, para uma parte; `<shard_dir>/recebidos/`, para um arquivo avulso - é
   a única coisa que precisa dar certo já.
2. Numa task de fundo, `fetch_manifest` pede tamanho + MD5 em
   `/api/transfer/manifest` (`MANIFEST_TIMEOUT` = 30 min - um digest é uma
   leitura do arquivo inteiro num volume lento, e o prazo corre em segundo
   plano) e roda a **checagem de espaço em disco** ali mesmo: `size_bytes` +
   `DISK_HEADROOM_MB` (512) contra o espaço livre; recusa antes de encher o
   disco e falhar no meio.
3. `run_download` escreve `<name>.part`, retoma do `.part` existente com
   `Range: bytes=<já-baixado>-`, lê/escreve em blocos de `CHUNK` (1 MB), atualiza
   progresso a cada 500 ms (velocidade usa o que **andou nesta tentativa**, não
   o total - retomada com bytes que já estavam no disco inflaria o número),
   muda o estado para `verifying`, confere o MD5 e só então renomeia para o
   nome final. MD5 errado apaga o `.part` e falha o job; nunca passa um arquivo
   truncado como se fosse bom. Cancelar no meio apaga o `.part`.

O lado **remetente** também entra na lista de transferências
(`direction: "sending"`), porque é ele quem transmite os bytes e já sabe o
progresso (`served_stream` conta o que saiu; acabar o corpo antes do
`Content-Length` prometido é como ele honra um cancelamento). O mesmo id existe
nas duas máquinas com direções opostas - é assim que os dois lados falam da
mesma transferência. Estados do job: `running`, `verifying`, `done`, `failed`,
`cancelled`; os campos de `TransferJob` são `id`, `name`, `direction`, `peer_host`
(substitui `from_host`, que mentia no sentido de envio), `path`, `state`,
`bytes_done`, `bytes_total`, `bytes_per_sec`, `eta_secs`, `detail`,
`finished_at`, `model_label`, `start_layer` e `end_layer`.

**Uma fatia se apresenta pelo modelo e pelas camadas, nunca pelo hash.** O
`name` de uma parte é o sha256 dela, e hash não diz de que modelo é nem que
pedaço dele traz - a notificação mostrava `df48d094c3d1023c0dab9e…` e o
operador não tinha como decidir nada com isso. Quem sabe as duas coisas é o
`.pom`, e ele **chega antes de qualquer byte de peso**: viaja dentro da oferta
(`TransferOffer::pom`) no sentido push, e é pedido a `/api/transfer/pom` antes
do download no sentido pull. Em ambos os casos ele já está gravado na pasta do
modelo quando `spawn_download` registra o job, e é de lá que `slice_label_for_path`
lê o rótulo. Do lado que envia, `download` faz a mesma leitura ao lado do
arquivo servido. O hash continua na tela, como identificação secundária -
é ele que nomeia o arquivo em disco e identifica a parte entre máquinas.

**O envio fecha no último byte, não no fim do stream.** Quando o corpo satisfaz
o `Content-Length` prometido, o hyper para de puxar e larga o stream sem nunca
pedir o `None` que marcaria o fim - contar os bytes é o único sinal que sempre
chega. Sem esse fecho o job do remetente ficava `running` para sempre, com
`bytes_done` congelado no último relato de 500 ms (numa LAN rápida, zero): a
máquina que mandou o arquivo mostrava no sino uma transferência eterna que já
tinha terminado e sido verificada do outro lado.

Todo desfecho passa por `TransferState::finish`, que grava o estado, o motivo e
o `finished_at`, e **recusa reescrever um terminal já posto**. É o que faz
"cancelado por ksia" - decisão de alguém - sobreviver ao "falhou" anônimo que o
loop reporta logo depois ao ver o stream morrer. O carimbo de hora é o que
permite ao sino mostrar o que **acabou de** terminar: antes dele a tela listava
só o que estava em andamento, e concluída/falhada/recusada sumiam no instante
em que aconteciam.

## 4. A oferta não carrega digest

`send_to_peer` posta **só o `stat`**: nome + `size_bytes` real, e `md5` sempre
vazio. O campo continua viajando porque um peer de versão anterior o exige no
corpo do pedido, mas ninguém o lê. Hashing do arquivo inteiro **antes** da
oferta era o que segurava a notificação do outro lado por minutos: medido no
cartão SD do captain, `md5` de uma fatia de 4,2 GB custa 3 min 23 s (~20 MB/s).
O cache de digest por `(caminho, mtime, tamanho)` só mascarava, deixando o
segundo envio do mesmo arquivo instantâneo - daí o "às vezes 1 ou 2 minutos" do
relato. Quem confere o download é o receptor, com o MD5 que ele mesmo pede em
`/api/transfer/manifest` depois de aceitar.

## 5. Manifesto e checagem de disco dentro da tarefa

Pela mesma razão, o manifesto e a checagem de disco correm **dentro** da task de
download, não antes do clique de aceitar. Consultar o outro lado custa uma
leitura completa do arquivo na máquina de origem; fazer isso no aceite faria o
clique esperar minutos. Em troca, o operador ganha o job já na lista, em `done`
ou `failed` com o motivo no lugar onde ele procuraria.

## 5.1 O esquema de quem disca é o local, não o da oferta

A oferta traz `from_gateway`, montado pelo **remetente**. Ele gravava
`http://` fixo: com o cluster em TLS, quem aceitava discava em claro e o gateway
do outro lado respondia **403 em texto puro** - literalmente "este cluster exige
TLS entre nós; refaça a chamada em https". O `.json()` caía em cima dessa frase
e o operador via `manifesto inválido de ksia: error decoding response body`, que
não aponta para nada (18/08).

Duas defesas, porque as duas são necessárias:

- o remetente monta a URL com `peer_client::peer_base_url(.., tls_ativo())`, e
  não com um literal;
- quem aceita passa a URL por `normaliza_esquema`, que impõe a **política
  local** - um nó de versão anterior continua oferecendo `http://`, e o
  download funciona mesmo assim.

E `fetch_manifest` confere o status **antes** de tentar ler JSON, repetindo a
frase do outro lado. Erro de gateway costuma vir em texto puro; ler JSON em
cima dele troca a explicação de quem sabe por uma mensagem que não ajuda.

## 5.2 O veredito volta para quem ofereceu

Bater na porta e nunca saber se abriram era metade do relato de campo: quem
mandava uma fatia não recebia aviso nenhum de que a outra ponta tinha aceitado
ou recusado. A oferta já viajava com um `grant`, e ele é o único id que as duas
pontas compartilham antes de existir um job - `send_to_peer` guarda o que
ofereceu em `TransferState::offered` (`grant` → destino, nome, caminho,
tamanho) e o receptor devolve o veredito em
`POST /api/transfer/offers/answer` (`notify_offerer` → `receive_answer`).

- **Aceite** registra o job `sending` na hora, com o mesmo `job_id` que o
  receptor cunhou, e detalhe "aceito; aguardando o início do envio". Isso
  importa porque a distância entre o aceite e o primeiro byte é o tempo de o
  receptor pedir o digest - minutos num volume lento -, e era silêncio puro na
  tela de quem tinha oferecido.
- **Recusa** vira um job `cancelled` com o nome de quem recusou. Recusa não é
  falha: foi decisão de alguém, e a tela diz de quem.
- **Aceite que não pôde começar** (fatia sem `.pom`, disco cheio) também avisa,
  em vez de deixar o remetente esperando um download que nunca vem.

O aviso é best-effort e sai numa task: o clique de quem decidiu não espera a
outra máquina responder, e um remetente fora do ar não faz a recusa falhar na
tela de quem recusou. Um veredito cujo `from_node_id` não é o destino
registrado é recusado com 403 - descreve outra conversa. Uma oferta de peer
anterior ao `grant` não gera aviso: não há id compartilhado para nomeá-lo.

## 6. Cancelamento simétrico

`POST /api/transfer/jobs/:id/cancel` é um verbo só, com a mesma semântica nas
duas pontas: a tela local e o aviso que o outro lado manda usam a mesma rota. O
cancelamento é marcado num conjunto `cancelled` que sobrevive até para jobs que
ainda não existem aqui (o aviso do outro lado pode chegar antes de o download
começar). Quem cancela a partir da própria tela avisa o peer
(`notify_peer_cancel`, com `from_peer: true` para evitar o eco); se o aviso se
perder, o lado remoto cai no caminho de conexão morta e reporta o motivo real.
Um job cancelado por alguém nunca é sobrescrito por um "falhou" anônimo.

## 7. Aceite automático - só desenvolvimento

`POM_DEV_AUTO_ACCEPT_TRANSFERS` faz o **receptor** aceitar toda oferta sem
clique. É variável do processo que recebe, lida **uma vez** no arranque
(`OnceLock`), e desligada por padrão: ausente, vazia, `0` e `false` todas
continuam exigindo confirmação. O remetente não tem como pedi-la - quem abre o
portão é a máquina cujo disco será escrito. Se um aceite automático não
consegue começar o download, a oferta volta para a prateleira em vez de sumir
com um erro só no log. Não ligue numa máquina cujo disco importa.

## 8. As três entradas na tela

O transporte é o mesmo; muda quem escolhe o quê:

- **Fatias nesta máquina** (aba Modelos): lista **toda** fatia local, sempre, e
  o destino é um `select` só com os nós que ainda não a têm ("todas já têm"
  quando não falta em ninguém). Destino e etiqueta de disponibilidade levam o
  começo do `node_id` quando peers da mesma máquina repetem hostname.
- **Fatias entre as máquinas** (aba Nós): lista de **diferença** - só o que
  falta do outro lado. É a explicação de "não existe opção de enviar uma fatia"
  quando as máquinas têm as mesmas fatias: o painel é uma lista de diferença e
  não sobra linha para clicar.
- **Enviar um arquivo desta máquina** (aba Nós): combobox com nome e caminho
  completo para conferência visual, enviando somente o basename seguro.

O progresso vive no sino e vem dos nós, então fechar a aba não interrompe nada.

## 9. Rotas

Todas atrás de `ConnectionAuth` (chave de conexão, ver `api-http.md`):

| Rota | Direção | Papel |
|---|---|---|
| `GET /api/transfer/manifest` | serve | tamanho + MD5 do arquivo pedido (por `?path=` ou `?grant=`) |
| `GET /api/transfer/download` | serve | stream com `Range`, retomada e progresso |
| `POST /api/transfer/pull` | puxa | este nó pede uma fatia a um peer (caminho em `models/`) |
| `POST /api/transfer/offer` | oferece | peer registra uma oferta nossa; nada baixa até aceitar |
| `GET /api/transfer/offers` | recebe | lista as ofertas + `disk_free_mb` |
| `POST /api/transfer/offers/:id/accept` | recebe | inicia o download (volta antes de o outro lado responder) |
| `POST /api/transfer/offers/:id/reject` | recebe | descarta a oferta e avisa quem ofereceu |
| `POST /api/transfer/offers/answer` | oferece | o veredito do outro lado sobre uma oferta nossa (`grant`) |
| `GET /api/transfer/jobs` | ambos | jobs em andamento, ordenados por nome |
| `POST /api/transfer/jobs/:id/cancel` | ambos | cancela (idempotente, avisa o peer) |
| `POST /api/transfer/send` | oferece | tela local envia `{to_node_id, filename}` para um peer |

Os locators `?job=` e `?peer=` são opcionais de propósito: um peer de versão
antiga os ignora e a transferência funciona igual, só sem a visão do lado que
envia.

## 10. Testes

Usam o testkit de `gateway` (ver `api-http.md`): `state_with` com a tabela de
peers semeada à mão e um `fake_peer` axum na loopback que captura o corpo da
oferta. Os testes de latência da oferta provam que a oferta sai **sem** digest
(`md5` vazio) e que `auto_accept` fica fechado a menos que a variável seja
explicitamente aberta. Há também teste de segurança: `?path=` fora de `models/`
e um `grant` inventado são recusados.

## 11. Defeitos anotados

- **DEFEITO ANOTADO: `cancelled` nunca é podado** (`TransferState::cancelled`,
  `transfer.rs`) - ids cancelados entram no `HashSet` e nunca saem; o conjunto
  cresce sem limite pela vida do processo. Inofensivo na prática (poucos bytes
  por id), mas é uma fuga de memória.
- **DEFEITO ANOTADO: `grants` nunca expiram** (`TransferState::grants`,
  `transfer.rs`) - um `grant` criado por `send_to_peer` vive até o processo
  morrer, mesmo depois de o download concluir ou a oferta ser rejeitada. A
  mensagem de erro fala em "concessão ... expirada", mas não existe prazo.
- **DEFEITO ANOTADO (externo): `/admin/transfer-offers` é stub** (`admin.rs`) -
  hardcoda `{ "offers": [] }`; as ofertas reais estão em `GET /api/transfer/offers`.
  Detalhe em `api-http.md`.
