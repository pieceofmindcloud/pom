# Operação: subir e verificar um cluster

Manual do operador. Build e variáveis de ambiente ficam em
`build-execucao.md`; este arquivo é a subida, a verificação e as armadilhas
medidas em campo.

## Como as peças se encaixam

- **Um processo por máquina.** O binário Rust sobe o servidor de inferência
  embutido (`inference/infer_server.rs`) quando `--inference-port` é passado; o
  modelo é carregado em-processo por `inference/llm.rs`. Não há processo Python -
  `old/python-inference/` é legado.
- **As camadas saem do nome do arquivo.** `parse_layers_from_filename()` lê os
  dois últimos campos separados por `_`: `qwen_0_15.gguf` → 0-15,
  `qwen_16_31.gguf` → 16-31. Não é preciso `--start-layer`.
- **O nome do modelo tem que bater entre os peers.**
  `ModelProfile::guess_from_name()` reduz o nome do arquivo a um rótulo
  (qualquer nome contendo `qwen` vira `qwen35`). O planejador
  (`routing/pipeline.rs`) só encadeia peers com `model` idêntico - shards com
  rótulos diferentes nunca formam pipeline.
- **Portas:** UDP P2P `--port` (42777), libp2p TCP = UDP + 10000 (52777),
  gateway HTTP `--gateway-port` (8080, servido por *todos* os nós), inference
  server `--inference-port` (8181). Máquinas distintas podem usar os mesmos
  números.
- **Contexto por sessão:** `--n-ctx` é a janela completa de cada sessão.
  `--max-sessions` (padrão 1) multiplica a capacidade total do cache KV, sem
  reduzir a janela individual (`n_ctx_total = n_ctx * max_sessions`, em
  `llm.rs`); o orçamento de prompt de cada requisição é `n_ctx` menos
  `--max-new-tokens` (ver `inferencia.md`, "Limite de prompt").
- **Tipo do cache KV:** `cache_type_k` e `cache_type_v` ficam no `pom.json` de
  cada instalação e valem para todos os contextos daquela instalação. O padrão
  é `f16`; os tipos aceitos e a confirmação em `/health` estão em
  `inferencia.md`. Eles não são escolhas por máquina no deploy.
- **Snapshots de KV cache:** o diretório onde cada peer grava e relê snapshots
  de KV, como o `--slot-save-path` do llama-server. Propriedade da máquina, não
  do deploy. Três fontes, nesta precedência: a flag `--slot-save-path`, a
  variável `POM_SLOT_SAVE_PATH` e o campo `slot_save_path` do `pom.json`. A env
  var é por processo e não é persistida no `pom.json`. Ausente em todas desliga
  o recurso e `POST /v1/session/slot` / `POST /admin/slots/{name}` respondem
  "não configurado". Ver `inferencia.md` §3.1.1 e `api-http.md`.
- **Cache de prefixo persistente:** `slot_persist_prefix_cache` no `pom.json`
  (ou `POM_SLOT_PERSIST_PREFIX_CACHE=1`) faz o gateway gravar o KV retido ao
  fim de cada turno e restaurá-lo num miss depois de um reinício, sem chamada
  manual. Desligado por padrão; exige `slot_save_path` em todos os hops. Uma
  conversa que cresce mantém um só snapshot (supersessão); turnos curtos não
  regravam (debounce). `slot_auto_max_bytes` limita o disco (padrão 2 GiB) e
  `slot_auto_ttl_secs` / `POM_SLOT_AUTO_TTL_SECS` o TTL (padrão 7 dias, renovado
  a cada uso). Ver `inferencia.md` §3.1.1.

## Subida (cluster de duas máquinas)

```bash
# peer 1 (camadas 0-15, gateway):
./old/scripts/run-p1.sh

# peer 2 (camadas 16-31):
ssh root@<peer2> 'cd /caminho/pom && ./old/scripts/run-p2.sh'
# BOOTSTRAP=<ip-do-peer1>:42777 sobrescreve o endereço do peer 1.
```

O gateway HTTP é ligado antes do carregamento do modelo, então a UI e a API de
controle ficam disponíveis durante o arranque. O carregamento do modelo leva
~1 min; espere o `/health` do servidor de inferência antes de mandar
requisição:

```bash
until curl -sf localhost:8181/health; do sleep 5; done
```

### Verificação

```bash
curl -s localhost:8080/peers | python3 -m json.tool   # 2 peers, 0-15 e 16-31
ssh root@<peer2> 'nvidia-smi --query-compute-apps=pid,used_memory --format=csv'
./old/scripts/request.sh "Explique em uma frase o que e inferencia distribuida."
# → "source": "pipeline:<id-peer1>→<id-peer2>"
```

`source` começando com `pipeline:` e com **dois** hops é a única prova de que a
inferência foi conjunta. Um hop só, ou `"source": "none"`, significa que o
planejador não fechou a cobertura `0..total_layers`.

## Bateria de rotas e forma da saída

Para separar o efeito da forma da saída do efeito da origem de rede, rode
`./scripts/bench-rotas.sh <rotulo>`. O script executa uma chamada de
aquecimento por origem, depois três repetições em cada combinação de
`mac-local`, `mac-ksia`, `ksia-local`, `salem-ksia` com os prompts
`p-curto/p-longo` e `repetitivo/variado`. Ele omite `model` por padrão,
então o gateway usa o modelo que a rede estiver servindo naquele momento. Use
`POM_BENCH_MODEL=...` somente para uma comparação que exija um modelo
explícito.

O CSV tem uma linha por aquecimento, medição ou falha. `phase=warmup` fica
fora das medianas; `status=unavailable` no aquecimento impede novas tentativas
nessa origem e preserva o restante da matriz. `fichas_por_chamada` é
calculada em cada linha como
`trace.output_tokens / trace.hops[0].decode.calls`. Para `salem-ksia`,
defina `SALEM_PASS` no ambiente no momento da execução; a senha não pertence
ao script. Os prompts longos são preenchidos por texto determinístico e a
tabela final mostra as medianas de `fichas/chamada`,
`generation_tok/s`, `ttft_ms`, `node_p50_ms` e `total_ms`.

`--n-gpu-layers` (padrão 0) decide quantas camadas vão para a VRAM; sem a
feature de build correta (`metal`/`cuda`) o modelo roda todo em CPU. Para
conferir o offload, veja a VRAM durante o carregamento - a TUI mostra
"Mem loaded" mas não discrimina GPU/CPU.

## Pegadinhas encontradas

- **`git pull` numa máquina de outro usuário.** Se os arquivos são de
  `<usuario>` e o acesso é como `root`, rode
  `git config --global --add safe.directory <caminho>`. Repositório privado
  precisa de *deploy key* read-only e remoto `git@github.com:...`.
- **Shards sem metadata `pom.*`** (splits antigos) não corrigem
  `total_layers` via `/health`; o valor cai no palpite de `guess_from_name`.
  Shards novos (splitter Rust) gravam `pom.original_block_count` e são a via
  confiável. Quantas camadas o modelo tem é **`Option`, não número**, do catálogo
  até o plano: `catalog.rs` deduz o total da cobertura do conjunto quando a fatia
  não declara (`total_layers_inferred`) e `deploy::model_size` faz o mesmo no
  cluster. Zerar esse "não sei" foi o bug de 2026-08-10: o plano se dizia
  completo e todo `apply` morria em `NodeConfig::validate`, depois de `launch` já
  ter parado a inferência das máquinas alvo.
- **A TUI limpa a tela a cada `--refresh` segundos.** Para ver logs, redirecione
  para arquivo (`./old/scripts/run-p1.sh > /tmp/p1.out 2>&1`) e leia de lá.
- **`pkill -f "target/release/pom"` por ssh mata o próprio shell do ssh** -
  o padrão casa com a linha de comando do `bash -c` remoto, e o ssh volta com
  exit 255 sem ter matado o peer. Use um padrão que não se auto-inclua, ou
  `pgrep -fa` antes para conferir o que vai morrer.
- **Porta de inferência ocupada:** o supervisor tenta libertá-la
  (`runtime::free_port`, `lsof -ti :PORT` no Unix, `netstat -ano` + `taskkill` no
  Windows, sondando até 5 s). O sintoma de falha está no log do nó
  (`encerrou com erro: Address already in use`). Confira as rotas de verdade:
  `curl -s localhost:8181/health` tem que responder; o servidor expõe
  `/v1/prefill` e `/v1/step` (não há `/openapi.json` - é Rust, não FastAPI).
- **Nunca lance o binário por caminho absoluto e pronto.** `models/`,
  `pom.json` e `venv/` saem do project root (`utils/config.rs`:
  `$POM_HOME` → cwd se parecer o projeto → diretório do executável,
  subindo de `target/release`). Um `ssh host '/caminho/pom'` roda em `$HOME`,
  lê outra config e não acha fatia nenhuma. A linha `Project root: ...` no
  arranque diz qual foi escolhido; confira-a antes de suspeitar do catálogo.

## Dois peers na mesma máquina - a armadilha é a identidade

Serve para dar janela grande sem arriscar a RAM da máquina menor. O que se perde
é a validade da medição de rede - o salto vira loopback. Bytes por salto
continuam valendo; tempo de rede, não.

Portas diferentes **não bastam**. A identidade fica em
`<project_root>/.pom-node.key`, então dois processos no mesmo diretório
carregam a mesma chave, anunciam o mesmo `node_id` - e o cluster indexa peers por
`node_id`. O segundo peer precisa de outra *casa*, via `POM_HOME`:

```bash
B=/caminho/pom-b
mkdir -p $B && cd $B
for f in models apps; do ln -s ../pom/$f $f; done
# a config nao e herdada: sem ela o no sobe em modo setup e nao carrega modelo.
# copie a do peer A trocando gateway_port, p2p_port e bootstrap_nodes.
POM_HOME=$B ../pom/target/release/pom --model ./models/qwen/qwen_16_31.gguf \
    --port 42778 --bootstrap 127.0.0.1:42777 --gateway-port 8091 --inference-port 8182
```

A UI é lida do disco em `<POM_HOME>/apps/frontend/dist`, daí o symlink de
`apps` - sem ele o segundo peer sobe mas responde 500 na interface. Confira com
`/api/ui/cluster`: dois `node_id` **distintos**.

## Inferência pela internet - o que está verificado

Descoberta e endereços: ver `rede-p2p.md`. O túnel de inferência
(`relay::send_frame_or_tunnel` → `network/infer_tunnel.rs`) foi verificado pela
internet em 2026-08-16: a conexão fecha em ~70 ms e o servidor de inferência do
peer remoto responde. As sondas ao vivo são
`infer_tunnel::tests::tunnel_reaches_a_live_peer` e
`relay::tests::the_tunnel_carries_a_real_frame_to_a_live_peer`, as duas
`#[ignore]` e dirigidas por `POM_TUNNEL_PROBE`. Elas não disparam na LAN
saudável.

Duas ressalvas honestas: as máquinas se alcançavam por IPv6 global, então o que
foi exercitado é "pela internet", não "atrás de um NAT que bloqueia"; e o desvio
só começa depois que o HTTP direto desiste - **75 s medidos no macOS**, porque
`relay::send_frame` fixa 600 s por requisição e sobrepõe o do cliente. Falta,
para uso real na internet: autenticação/TLS no `/v1/infer` e no gateway, e uma
decisão de produto sobre o prazo do HTTP direto.

## Ajuste opcional de TCP entre peers

Em nós Linux, o operador pode experimentar estes ajustes para transferências
entre peers distantes:

```bash
sudo sysctl -w net.ipv4.tcp_congestion_control=bbr
sudo sysctl -w net.ipv4.tcp_slow_start_after_idle=0
```

`tcp_slow_start_after_idle=0` evita que o kernel descarte a janela de
transmissão depois de uma pausa, mesmo com a conexão aberta. Isso pode ajudar
porque o prefill trafega em rajadas. É uma recomendação opcional de operação,
nunca um requisito: os ajustes exigem privilégio de administrador e não são
dependência do POM, que deve funcionar bem sem eles.

A medição disponível até agora, de 1,3 para 4,1 MB/s em um arquivo de 214 KB,
foi no sentido de download e não permite atribuir o ganho a um fator isolado.
Não há número de desempenho prometido. A transferência de arquivos do próprio
POM via libp2p chegou a cerca de 40 MB/s entre duas máquinas; portanto, 126 MB
seriam cerca de 3 s se o caminho de inferência usasse esse mesmo transporte.
Essa é uma condição, não uma medição do caminho de inferência: as estimativas
anteriores de 20 a 90 s vieram de HTTP em objeto pequeno e não decidem a
escolha. Compare execuções pelo `hidden_state_bytes_*` de cada travessia e pelo
`hidden_state_dtype` efetivo na telemetria.

## Conectando um harness

OpenCode (verificado): `opencode.json` com provider `@ai-sdk/openai-compatible`,
`baseURL` `http://<peer>:<porta>/v1`, e um agente com prompt curto e ferramentas
desligadas - o prompt do agente padrão não cabe no orçamento de prompt.

```bash
OPENCODE_CONFIG=/caminho/opencode.json opencode run --pure --agent curto "..."
```

Números medidos: o OpenCode 1.18.21 manda ~2200 caracteres na chamada de título
e ~54k no agente padrão, com `max_tokens: 32000`, ferramentas e
`stream_options.include_usage`. Config que fez funcionar para o agente padrão:
`--n-ctx 32768 --n-batch 3072 --max-new-tokens 256` → orçamento de 32768 menos
256 tokens. Se o prompt excede o orçamento, `/v1/chat/completions` agora
responde HTTP 413 antes do SSE; isso deixa o erro visível no OpenCode em vez de
deixá-lo aguardando um evento de erro que não tem formato de chunk.

O Pi só expõe base URL para Azure; não há como apontá-lo para um endpoint
OpenAI-compatível arbitrário por variável de ambiente.

## Monitor de carga (`crates/llama-engine/src/sysmon.rs`)

Linux: RAM de `/proc/meminfo`, VRAM e processos de `nvidia-smi` (conferido
contra `nvidia-smi` na mão: bate exato). As chamadas frequentes ao utilitário
rodam fora do reaper de processos do Tokio, para não interromper o socket UDP
do DHT pkarr com `EINTR`. macOS: RAM de `sysctl` + `vm_stat`;
memória unificada **não** reporta VRAM dedicada - a tela diz isso e mostra a
memória wired, que é onde ficam os buffers do Metal, em vez de inventar um
número.

**Radeon (Linux).** Nome, VRAM, ocupação e temperatura saem do sysfs do
`amdgpu` (`/sys/class/drm/card*/device`), sem binário externo - o equivalente
do `nvidia-smi` seria o `rocm-smi`, que só existe com o ROCm inteiro instalado,
e o backend usado é o Vulkan. Quem segura VRAM sai de `/proc/<pid>/fdinfo`
(`drm-resident-vram`, ou `drm-memory-vram` nos kernels antigos), desduplicado
por `drm-client-id`. A leitura é honestamente **o que este usuário pode ler**:
processo de outro dono recusa o `fdinfo`, e o nó - que é o que interessa -
está sempre na lista.

**Apple Silicon.** O acelerador publica `PerformanceStatistics` no IORegistry,
sem root e sem `powermetrics`: `ioreg -r -d 1 -w 0 -c AGXAccelerator` dá
`Device Utilization %` e `In use system memory`, que viram `utilization_pct` e
`vram_used_mb` (com `vram_total_mb` em `None`, porque VRAM dedicada não
existe ali). Temperatura continua desconhecida de propósito: ela mora no SMC,
atrás de API privada, e "não sei" é a única resposta honesta sem privilégio.

**Discos.** `disk` é o volume onde as fatias moram - o que responde "cabe outra
fatia?". `disks` é toda partição montada, de `df -k -P`, sem os pseudo sistemas
de arquivos (`tmpfs`, `devfs`, `overlay`…) e sem repetir ponto de montagem. É
`-k`, e não `-m`: o `-P` fixa blocos de 512 bytes no macOS e ignora o `-m`, o
que faria todo volume aparecer 2048 vezes maior. Windows não tem `df` e
continua reportando só o volume do projeto.

## Comparar POM com llama.cpp puro

O benchmark operacional está em `scripts/bench-llamacpp.sh`. Ele recebe o
snapshot `*.config.json` do `pom-bench`, seleciona por padrão o deployment
`serving`/`is_default` do modelo ativo e só aceita `kind=whole`, pois o
`llama-server` simples não reproduz o RPC de `shards` ou `tensor_parallel`.
Antes de iniciar qualquer processo na ksia consulta `nvidia-smi` e exige, por
padrão, 14.000 MiB livres em cada GPU; quando o POM ocupa a memória, termina
com `status=blocked_vram` sem tocar no processo do POM. O servidor usa a porta
dedicada 8090 em `127.0.0.1`, e o cleanup é limitado ao PID que o harness
criou.

Uso mínimo, a partir da raiz do repositório:

```bash
scripts/bench-llamacpp.sh ~/scripts/reports/<data>-<modelo>.config.json \
  --deploy-id <deploy-id> --output-dir ./bench-llamacpp-results
```

O resultado é um CSV com uma linha por warmup, medição ou erro, mais um
`summary.md` com medianas dos cinco prompts de `bench-rotas.sh`. O campo
`generation_tok_s` vem de `timings.predicted_per_second`; `prefill_ms` e
`prefill_tok_s` vêm de `timings.prompt_ms` e `timings.prompt_per_second`;
`ttft_ms` é o tempo observado até o primeiro evento SSE e `total_ms` é o tempo
de parede da chamada local na ksia. Consulte `scripts/bench-llamacpp.sh --help`
para os overrides de `n_ctx`, caminhos remotos e política de VRAM.

No runtime POM, `n_ctx` é a janela de cada sessão. Por isso o harness passa
`-c n_ctx * max_sessions` e `-np max_sessions`; `offload_kqv=true` fica no
default do llama.cpp e somente `false` passa `-nkvo`. Um `llama-server`
preexistente só é reutilizado quando o `CMakeCache.txt` confirma a árvore do
fork com CUDA e `LLAMA_BUILD_SERVER=ON`; builds necessários usam diretório
externo para não alterar a árvore fonte.
