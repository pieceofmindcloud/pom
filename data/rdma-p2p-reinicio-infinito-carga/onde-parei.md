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

## O que falta - o desenho da correcao, ja fechado, sem uma linha escrita

Invariante a impor: **o arquivo de anotacao existe se e somente se a fatia
nomeada nele foi vista carregada e servindo.**

1. `runtime.rs` `apply()`: **remover** o `lastslice::save(&cfg)` e por no lugar
   um `lastslice::clear()` - ao subir a geracao, nada esta servindo.
2. `runtime.rs` `spawn_health_watch()`: passar o `cfg` para a task e chamar
   `lastslice::save(&cfg)` junto do `status.set_ready(...)` - o unico ponto do
   codigo que prova que a fatia esta no ar.
3. `runtime.rs` `spawn_supervisor()`: no ramo que desiste (`ServerState::Failed`),
   chamar `lastslice::clear()`. Caso real, nao defensivo: uma fatia que ficou
   Ready (anotada) e depois quebrou 10x nao pode ser ressuscitada.
4. Contador atravessando reinicios: **nao e preciso**. Com (1)+(2) o `take()`
   consumir o arquivo ja da exatamente **uma** retentativa por reinicio, e so
   um carregamento bem-sucedido rearma. O laco morre sozinho.
5. Visibilidade (criterio de aceite "informa com clareza por que a fatia nao
   subiu"): estava avaliando gravar tambem o **estado** na anotacao
   (`pending` no apply / `serving` no ready) para que o arranque seguinte possa
   dizer "a fatia X nao chegou a carregar na tentativa anterior" em vez de subir
   vazio em silencio. **Decisao ainda aberta** - ver "Duvida em aberto".

### Testes a escrever
- Regressao do laco: a partir de um `apply` que nunca fica Ready, o arquivo de
  anotacao **nao** existe; e `take()` no arranque seguinte devolve `None`.
- O teste existente `anotada_no_apply_e_esquecida_no_stop`
  (`crates/node/src/models/lastslice.rs`) **precisa ser reescrito**: o nome e o
  corpo afirmam justamente o comportamento que causa o bug ("o apply anota").
  A garantia de `c7b70d8` que tem que continuar viva e outra: *fatia servindo
  saudavel + queda -> volta servindo*. Escrever essa, e nao apagar a protecao.
- Andaime pronto para rota de backend: `gateway::testkit` (ver CLAUDE.md).

## Arquivos tocados

- `data/rdma-p2p-reinicio-infinito-carga/` (novo) - este resumo, os scripts de
  reproducao e os dois logs. **Nenhum arquivo de `crates/` foi alterado.**

## Proximo passo exato

1. Reescrever o teste `anotada_no_apply_e_esquecida_no_stop` em
   `crates/node/src/models/lastslice.rs` para a garantia certa, e adicionar o
   teste de regressao do laco (falhando).
2. Aplicar os itens 1-3 do desenho acima em
   `crates/node/src/inference/runtime.rs`.
3. `cargo test` + `cargo clippy` no que foi tocado.
4. Rodar `repro/setup.sh` + `repro/ciclo.sh` de novo: o ciclo 2 tem que mostrar
   `anotacao=ausente` e `restauracoes no log: 0`.

## Duvida em aberto (talvez needs-decision)

O item 5 e escolha de produto: depois de uma fatia que nunca carregou, o no deve
(a) subir vazio e calado, ou (b) subir vazio e **dizer** por que, gastando um
campo novo na anotacao. Inclinacao: (b), porque o brief pede a razao visivel ao
operador. Se o custo for julgado alto, (a) sozinho ja satisfaz o criterio de
"nao reinicia indefinidamente".
