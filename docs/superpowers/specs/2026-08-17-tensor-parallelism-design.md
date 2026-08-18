# Tensor parallelism customizado (caminho C)

Data: 2026-08-17

Dar ao nó uma segunda dimensão de paralelismo sem transferir peso pela rede:
cada worker extrai o próprio shard de tensor da própria cópia local do GGUF, e
a execução troca apenas ativações (all-reduce). É a extensão da invariante que
já existe no fatiamento por pipeline: nenhum peso cruza a rede.

## O resultado do spike (2026-08-17)

O único hook que o llama.cpp expõe na execução do grafo é o
ggml_backend_sched_eval_callback, que o projeto já usa para capturar o residual
l_out-N. Lendo ggml/src/ggml-backend.cpp (ggml_backend_sched_compute_splits,
linhas 1685-1715), o callback:

- percorre os nós do grafo e pergunta (ask=true) quais o chamador quer em
  isolamento;
- computa o trecho, sincroniza o backend e avisa (ask=false) que o dado está
  pronto para LEITURA;
- se o chamador devolver false no ask=false, ABORTA o restante do grafo.

Ou seja: ele lê dado de tensor e pode parar o grafo, mas não consegue inserir
um nó novo nem alimentar um valor de volta ao meio do grafo. Não há como fazer
a redução de ativação no meio do forward por esse hook.

Consequência: tensor parallelism customizado exige FORKAR o llama.cpp em três
pontos:

1. um operador/backend ggml que execute a redução em rede (all-reduce) como
   nó do grafo;
2. patch em build_llama (src/llama-graph.cpp) para inserir esse nó depois da
   projeção de saída da atenção (wo) e depois da projeção down do MLP
   (ffn_down), antes do residual add e da norm;
3. patch no loader (src/llama-model-loader.cpp / llama-model.cpp) para
   carregar um GGUF com tensor parcial (linhas/colunas), identificado por
   metadata pom.tp_rank.

As peças do projeto (fatiador de shard, coletivas, anúncio/roteamento/
implantação) não dependem do fork e podem andar em paralelo.

## Modelo mental (duas dimensões ortogonais)

- Pipeline (dimensão 1): faixas de camadas; estado oculto f16 viaja entre
  estágios. Não muda.
- Tensor (dimensão 2): dentro de um estágio, N workers seguram frações dos
  MESMOS tensores (por linhas/colunas) e trocam reduções de ativação. Um é o
  primário (o hop do pipeline), os outros são workers.

## Regras e invariantes

1. Um hop de pipeline = um primário + opcionalmente K workers. O roteamento
   continua com um hop por estágio.
2. Nenhum peso trafega na rede: cada worker extrai o próprio shard da própria
   cópia local do GGUF.
3. A ativação trafega por token (2 all-reduce por bloco, f16/zstd para
   reduzir). É inerente ao TP e domina o custo contínuo.
4. Mesma versão de binário no primário e nos workers.
5. Todo campo novo de anúncio leva #[serde(default)].
6. O split de tensor respeita o bloco do tipo ggml (blck_size); tipos
   desconhecidos são recusados, nunca estimados.

## Classificação de tensores (Llama, Megatron-style)

- ROW-parallel (split por linhas, precisa all-reduce): attn_output.weight,
  ffn_down.weight, e o output head (output.weight).
- COLUMN-parallel (split por colunas, sem reduce): attn_q/k/v.weight,
  ffn_gate.weight, ffn_up.weight.
- Não shardado (igual em todos os shards): attn_norm.weight, ffn_norm.weight,
  token_embd.weight, output_norm.weight.

## Fora de escopo (v1)

- Data parallelism e context/sequence parallelism.
- Re-sharding dinâmico em runtime.
- TP pela internet (medir em LAN primeiro).
