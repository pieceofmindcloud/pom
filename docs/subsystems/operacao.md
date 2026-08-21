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

## Subida (cluster de duas máquinas)

```bash
# peer 1 (camadas 0-15, gateway):
./old/scripts/run-p1.sh

# peer 2 (camadas 16-31):
ssh root@<peer2> 'cd /caminho/pom && ./old/scripts/run-p2.sh'
# BOOTSTRAP=<ip-do-peer1>:42777 sobrescreve o endereço do peer 1.
```

O carregamento do modelo leva ~1 min; espere o `/health` do servidor de
inferência antes de mandar requisição:

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

## Conectando um harness

OpenCode (verificado): `opencode.json` com provider `@ai-sdk/openai-compatible`,
`baseURL` `http://<peer>:<porta>/v1`, e um agente com prompt curto e ferramentas
desligadas - o prompt do agente padrão não cabe no orçamento de prompt.

```bash
OPENCODE_CONFIG=/caminho/opencode.json opencode run --pure --agent curto "..."
```

Números medidos: o OpenCode manda ~2200 caracteres com um agente mínimo (sem
ferramentas) e ~8000 no agente padrão. Config que fez funcionar:
`--n-ctx 32768 --n-batch 3072 --max-new-tokens 256` → orçamento de 3072 tokens.

O Pi só expõe base URL para Azure; não há como apontá-lo para um endpoint
OpenAI-compatível arbitrário por variável de ambiente.

## Monitor de carga (`utils/sysmon.rs`)

Linux: RAM de `/proc/meminfo`, VRAM e processos de `nvidia-smi` (conferido
contra `nvidia-smi` na mão: bate exato). As chamadas frequentes ao utilitário
rodam fora do reaper de processos do Tokio, para não interromper o socket UDP
do DHT pkarr com `EINTR`. macOS: RAM de `sysctl` + `vm_stat`;
memória unificada **não** reporta VRAM dedicada - a tela diz isso e mostra a
memória wired, que é onde ficam os buffers do Metal, em vez de inventar um
número.
