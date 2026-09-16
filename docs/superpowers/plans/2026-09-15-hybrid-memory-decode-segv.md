# Regressão do SIGSEGV em decode híbrido

## Objetivo

Eliminar a causa do crash observado em `llama_batch_allocr::ubatch_add` durante
decode de um modelo híbrido com `n_ctx=750000` e seis sessões, e deixar uma
regressão executável no checkout local do fork `pom-tensor-parallel`.

## Diagnóstico confirmado

`llama_batch_allocr::ubatch_add` e `split_equal` usam `int64_t` nos índices,
offsets e tamanhos derivados de posição. A auditoria e os testes sob ASan não
encontraram overflow para posições próximas de 750000. O `n_tokens=516`
observado com seis sequências é a forma retangular `6 * 86` da reserva do
grafo, e não uma escrita fora de um ubatch de 512.

O conjunto de recursos do log identifica Kimi-K3: o fork contém Gated Delta
Net, Lightning Indexer e DeepSeek V4 HC, mas esses operadores também estão no
upstream atual. Kimi-K3 combina KDA recorrente com MLA e usa
`llama_memory_hybrid`.

O pai `origin/feat/implantar3` passou a pedir snapshots recorrentes para
decodificação especulativa. O fork, porém, não declarava `LLM_ARCH_KIMI_K3` em
`llm_arch_supports_rs_rollback`; o contexto silenciosamente reduzia
`n_rs_seq` a zero. Além disso, o builder K3 antigo gravava somente a última
janela de convolução e o estado final do `build_delta_net`, em vez de
materializar os grupos de snapshot e usar `build_recurrent_attn`. O estado KDA
ficava incompatível com rollback/replay multi-sequência. O crash aparecia no
alocador durante um ubatch posterior, que era o ponto onde a corrupção se
manifestava, não a origem do cálculo de posição.

## Mudança mínima

- Declarar Kimi-K3 compatível com rollback recorrente.
- Fazer o builder K3 gravar cada uma das `1 + n_rs_seq` janelas de convolução
  no grupo correto da memória recorrente.
- Usar `build_recurrent_attn` para que o estado e os snapshots sejam
  atualizados pela mesma lógica de rollback dos demais modelos híbridos.
- Adicionar um teste CTest que gera o GGUF K3 mínimo e repete quatro decodes
  com seis sequências, contexto 750000, microlote 512 e rollback/replay.
- Manter testes separados do alocador para contexto grande, seis sequências e
  formatos aleatórios de `split_equal`.

## Evidência de reprodução e validação

Antes do patch, o teste K3 falhava de forma determinística em
`rollback failed at round 0 seq 0`, demonstrando que o contrato de rollback
pedido pelo contexto não existia para essa arquitetura. Não foi possível
reproduzir localmente o SIGSEGV exato sem o GGUF e o backend da máquina de
produção. O teste direto do alocador e os modelos híbridos pequenos Qwen3.5 e
Nemotron-H não reproduziram crash sob ASan.

Depois do patch, passaram sob ASan:

- `test-hybrid-memory-batch` com o GGUF Kimi-K3 gerado;
- `test-recurrent-state-rollback` com Kimi-K3;
- o mesmo cenário com Qwen3.5 e Nemotron-H;
- os testes de grande contexto e fuzz determinístico de `test-batch-alloc`.

## Entrega

Documentar a invariável em `docs/subsystems/inferencia.md`, atualizar o
README e o `AGENTS.md` do wrapper e registrar as mudanças nativas no checkout
local do fork. Validar o diff, formatar o workspace Rust, criar os commits
locais e deixar a branch `fm/rdma-crash-hybrid-memory-decode` pronta para o
firstmate. Não fazer push nem abrir PR.
