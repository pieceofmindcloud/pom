# Onde parei - laco infinito de reinicio quando a fatia nao cabe na memoria

Parado por ordem do captain (cota da sessao), 2026-08-15.
Branch: `fm/rdma-p2p-reinicio-infinito-carga`. **Nenhum codigo de producao foi
alterado ainda** - so a investigacao esta commitada. Nao ha stash pendente.

## O que ja fez - investigacao concluida, com prova

### 1. O laco infinito foi reproduzido de ponta a ponta

Reproducao em `repro/` (scripts) e nos dois logs deste diretorio. Roda com o
binario real, pelo caminho HTTP que o painel usa (`POST /api/ui/apply`), com uma
`RDMA_P2P_HOME` isolada e `--no-discovery` (nao anuncia na LAN).

```
=== ciclo 1: o operador implanta a fatia ===
  apply -> {"job_id":"4f4d..."}
  [ciclo 1] gateway=200  anotacao=PRESENTE      <- anotada ANTES de carregar
  restauracoes no log: 0
  [ciclo 1] processo morto com SIGKILL (simulando o OOM killer)
=== ciclo 2: o Docker religou o container ===
  [ciclo 2] gateway=200  anotacao=PRESENTE
  restauracoes no log: 1                        <- ressuscitou a mesma fatia
=== ciclo 3 ===
  [ciclo 3] gateway=200  anotacao=PRESENTE
  restauracoes no log: 1                        <- e de novo, para sempre
```

Limitacao registrada de proposito: nao ha nesta maquina GGUF grande nem hardware
para provocar OOM de verdade, entao as duas mortes foram **forcadas**:
- erro gracioso: um GGUF so de vocabulario (`ggml-vocab-qwen35.gguf`), que passa
  no `validate()` (o arquivo existe) e o llama.cpp recusa carregar - mesmo
  caminho de codigo de `llama_model_load_from_file` devolvendo null por VRAM
  cheia;
- morte do processo: `kill -9`, que e exatamente o que o OOM killer do cgroup
  faz - SIGKILL, sem executar codigo nenhum.

### 2. Os dois modos de morte, separados e medidos

| | Modo A - erro devolvido | Modo B - processo morre |
|---|---|---|
| o que acontece | `llama_model_load_from_file` devolve null, `bail!` | SIGKILL do OOM killer / abort dentro do CUDA |
| supervisor | 10 tentativas (~60 s), termina em `ServerState::Failed` | nao roda codigo nenhum |
| processo | **vivo** | morto |
| gateway/`/health` | **200 o tempo todo** (medido em t=2,10,25,45,70,95 s) | fora do ar |
| Docker religa? | **nao** (o processo nao saiu) | **sim** |
| laco infinito? | **nao** | **sim** |

Modo A medido em `modo-a-erro-gracioso.log`; o status final que o painel ve:

```
{"state":"failed","detail":"encerrou 10x: llama_model_load_from_file failed for ..."}
```

### 3. Qual modo a evidencia confirma - deducao fechada

O captain relata **o container sendo religado**. O Docker so religa quando o
processo sai. O modo A esta provado acima **nao sair** (gateway 200 em t=95 s,
estado `failed`). Logo a falha do captain **nao e o modo A**: e o **modo B**,
morte do processo sem executar codigo.

Isso tambem explica o segundo sintoma, "a aplicacao trava e para de responder":
nao e travamento dentro do processo - dentro de um processo vivo o no responde
ao painel o tempo todo (medido). O no fica sem responder porque **esta morto**,
e nunca fica utilizavel porque o ciclo nunca termina.

### 4. Causa raiz confirmada (a hipotese de firstmate se sustenta)

`crates/node/src/inference/runtime.rs`, `apply()` linha ~338: `lastslice::save(&cfg)`
e chamado **antes** de `spawn_supervisor` - a fatia e anotada em disco quando o
`apply` e *chamado*, nao quando ela passa a servir. Uma fatia que nunca carregou
fica, no disco, indistinguivel de uma que estava servindo saudavel. O arranque
(`main.rs:1076`) entao ressuscita as duas do mesmo jeito.

`take()` ja apaga o arquivo ao ler ("uma fatia que o no nao consegue mais servir
nao pode reaparecer a cada arranque", diz o proprio doc comment de `take_from`).
O defeito e que `apply()` **rearma** a anotacao logo em seguida. O contador
`MAX_RESTARTS` e de memoria e morre junto com o processo, entao no modo B nao ha
orcamento nenhum atravessando os reinicios.

Nao ha regressao no proposito de `c7b70d8`: o caso que ela protege (ksia-gpu3,
SIGSEGV) acontece com a fatia **ja servindo saudavel**, que continua sendo
anotada.


## A correcao, escrita e medida

Invariante imposto: **a anotacao vale como "retomavel" se e somente se a fatia
nomeada nela foi vista carregada e servindo.** O arquivo deixou de ser um
`NodeConfig` cru e ganhou um campo `state`:

| quem escreve | estado | efeito no arranque seguinte |
|---|---|---|
| `runtime::apply` | `pending` | nao retoma, e **diz por que** |
| `spawn_health_watch`, no `model_loaded` | `serving` | retoma a fatia |
| `stop`, e o ramo que desiste em `MAX_RESTARTS` | (apagado) | nada a retomar |

Um arquivo do formato antigo (sem `state`) conta como `serving`: so a rota de
reinicio o escrevia, num no que estava servindo - senao o proprio "Atualizar e
reiniciar" perderia a fatia na travessia de versao.

O contador que faltava (item 4 do desenho) continua nao sendo preciso: `take()`
consome o arquivo, entao ha **uma** tentativa por reinicio, e so um carregamento
bem-sucedido rearma.

`devupdate` deixou de anotar a fatia antes de reiniciar. Nao e limpeza: o
runtime ja mantem a anotacao com o estado que a fatia de fato alcancou, e
reescreve-la ali so poderia mentir - carimbar de servindo uma fatia que nunca
carregou e o mesmo laco entrando por outra porta.

### O que ficou medido

`repro/setup.sh` + `repro/ciclo.sh`, o mesmo caminho de antes (o binario real,
`POST /api/ui/apply`, SIGKILL entre os ciclos):

```
=== ciclo 1: o operador implanta a fatia ===
  [ciclo 1] gateway=200  anotacao=PRESENTE
  estado anotado: "state": "pending"      <- aplicada, ainda nao carregada
  restauracoes no log: 0
=== ciclo 2: o Docker religou o container ===
  [ciclo 2] gateway=200  anotacao=ausente <- consumida e nao retomada
  restauracoes no log: 0
=== ciclo 3 ===
  [ciclo 3] gateway=200  anotacao=ausente
  restauracoes no log: 0                  <- o laco morreu
```

A razao chega ao operador, nao so ao log. `GET /api/ui/node` no ciclo 2:

```json
{"state": "disabled",
 "detail": "a fatia unknown-llm 0-0 nao chegou a carregar na tentativa anterior e nao foi restaurada"}
```

### Limitacao da verificacao

Nao ha GGUF que carregue nesta maquina, entao o sentido oposto - fatia que
**carrega**, cai e volta servindo - nao foi provado com llama.cpp de verdade.
Esta coberto por dois testes: `fatia_vista_servindo_volta_depois_de_uma_queda`
(a semantica do arquivo) e `o_health_e_quem_promove_a_anotacao_para_servindo`
(a fiacao, com um `/health` de mentira respondendo `model_loaded`). O que
nenhum dos dois cobre e o llama.cpp de fato subir - vale medir no cluster do
captain na proxima subida.
