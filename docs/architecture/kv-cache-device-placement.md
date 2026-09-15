# Cache KV em dispositivo separado dos pesos

## Resultado

Na versão vendorizada atual do llama.cpp, sem alterar o checkout do fork, não
é possível colocar o cache KV em uma GPU específica diferente da GPU que
recebe os pesos do mesmo modelo. A tarefa não adiciona `context_gpu`,
`kv_cache_gpu` nem um controle correspondente na tela de implantação, porque
essas opções não teriam efeito real.

O caso do captain, com os pesos na RTX 3060 e o KV na Radeon RX 7800 XT, exige
uma decisão antes de qualquer implementação nativa. O status desta tarefa usa
`needs-decision [key=kv-cache-device-placement]`.

## Evidência no contrato público

O header `vendor/llama-cpp-sys-2/llama.cpp/include/llama.h` oferece:

- `LLAMA_SPLIT_MODE_NONE`, `LAYER`, `ROW` e `TENSOR` (`:198-203`). Os modos
  `LAYER` e `ROW` dizem explicitamente que dividem camadas e KV entre GPUs.
- `llama_model_params.devices`, `split_mode`, `main_gpu` e `tensor_split`
  (`:307-322`). Esses parâmetros escolhem os dispositivos e a divisão dos
  pesos. `main_gpu` só escolhe a GPU do modelo no modo `NONE`.
- `llama_model_tensor_buft_override` (`:302-305`), uma regra por padrão de
  nome para tensores do modelo.
- `llama_context_params.type_k`, `type_v`, `offload_kqv` e `kv_unified`
  (`:421-470`). Não existe ali `kv_device`, `kv_buft` ou uma lista de
  dispositivos do cache.

O `ggml_backend_sched` também não oferece uma saída do contrato público do
`llama_context` para substituir a alocação do KV. O scheduler é criado pelo
contexto depois que a memória já foi montada, em
`vendor/llama-cpp-sys-2/llama.cpp/src/llama-context.cpp:584-607`.

## Caminho efetivo no llama.cpp

1. `llama-model.cpp:1350-1384` calcula os pontos de divisão a partir de
   `n_gpu_layers` e `tensor_split` e salva um `dev_layer` para cada camada.
2. `llama-kv-cache.cpp:210-223` percorre as camadas do cache. Com
   `offload=true`, chama `model.dev_layer(il)` e usa
   `ggml_backend_dev_buffer_type(dev)`. Com `offload=false`, mantém o buffer
   CPU.
3. `llama-kv-cache.cpp:274-285` aloca os buffers agrupados por esse buffer
   type. Não há uma segunda escolha de dispositivo para o KV.
4. `llama-model-loader.cpp:1237-1254` consulta `tensor_buft_overrides` apenas
   enquanto cria os tensores de peso do GGUF. Os tensores `cache_k_l*` e
   `cache_v_l*` ainda não existem nessa etapa.

Isso também explica por que `split_mode=row` não resolve o pedido: ele pode
usar um buffer dividido para os pesos, mas a criação do cache continua ligada
à colocação da camada. Fazer `offload_kqv=false` resolve apenas o estouro de
VRAM levando o KV para RAM.

## Fronteira atual do POM

O wrapper Rust mantém a mesma separação:

- `crates/llama-engine/src/llm/model.rs:429-447` passa `devices`,
  `tensor_split` e `tensor_buft_overrides` na carga do modelo.
- `crates/llama-engine/src/llm/config.rs:1012-1022` cria os parâmetros do
  contexto com contexto, sessões, cache unificado, `offload_kqv` e tipos K/V.
- `crates/llama-engine/src/mtp.rs:168-187` repete a mesma política para o
  contexto MTP.

Não existe no POM um ponto seguro para acessar uma estrutura interna de
`llama_context` e trocar buffers depois da construção. Fazer isso por cast de
ponteiro, alocação manual de GGML ou cópia parcial de KV seria um hack sem
contrato de sincronização e não foi implementado.

## Alternativas

### Disponíveis agora, sem fork

- **KV em RAM:** definir `offload_kqv=false`. Mantém os pesos na colocação
  escolhida, mas deixa a Radeon ociosa para esse uso e aumenta o custo de
  acesso ao cache.
- **Divisão conjunta:** usar `split_mode=layer`, `row` ou `tensor` com as duas
  GPUs. Usa a VRAM total, mas distribui pesos, computação e KV conforme as
  regras nativas; não atende ao requisito de pesos exclusivamente na RTX e KV
  exclusivamente na Radeon.
- **Reduzir a pressão do KV:** diminuir contexto/sessões ou usar tipos K/V
  menores. Isso já é suportado pelo POM, mas não cria uma colocação separada.

### Necessária para atender o pedido exato

Uma tarefa futura autorizada a alterar o fork precisa introduzir um contrato
explícito para a colocação do KV, por exemplo um `ggml_backend_buffer_type_t`
ou uma lista de dispositivos em `llama_context_params`. A implementação teria
de cobrir todas as famílias de memória nativas, incluindo `llama_kv_cache`,
cache recorrente e memória híbrida, manter ambos os backends vivos no
scheduler, garantir as cópias entre o dispositivo dos pesos e o dispositivo
do KV e publicar a colocação efetiva para diagnóstico. Só depois disso o POM
poderia adicionar um campo persistido, validação de índice/device e o controle
na tela de implantação.

## Decisão pedida

Escolher uma das opções:

1. aceitar temporariamente KV em RAM ou a divisão conjunta existente;
2. autorizar uma tarefa futura, separada, para criar a API e a implementação
   de KV em dispositivo independente no fork do llama.cpp;
3. manter o comportamento atual até essa decisão.
