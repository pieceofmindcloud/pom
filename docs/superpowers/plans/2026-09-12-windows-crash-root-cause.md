# Investigação da violação de acesso no carregamento Windows

Data: 2026-09-12

## Evidência recebida

O Event Viewer registrou `0xc0000005` em `ucrtbase.dll`, no processo
`pom.exe`, no mesmo segundo em que o log anotou
`modelo carregado (/health respondeu model_loaded)`. O erro
`Chrome_WidgetWin_0` com código 1412 apareceu durante a limpeza da janela e,
isoladamente, não prova que o WebView2 foi a origem.

## Caminho real confirmado no código

Não há um `infer_server` subprocesso nem uma fronteira IPC. `main.rs` inclui
`inference/infer_server/mod.rs`, e `runtime.rs` chama `crate::infer_server::run`
dentro de uma task Tokio no mesmo `pom.exe`. No Windows, somente o shell Tao/Wry
fica na thread de UI; o serviço Tokio roda numa thread de serviço do mesmo
processo.

O caminho de readiness é:

1. `infer_server::run` carrega `LlmEngine` em `spawn_blocking`, constrói
   `InferState` e publica a rota `GET /health`.
2. `spawn_health_watch` faz `GET /health`, recebe o corpo HTTP e executa
   `resp.json::<serde_json::Value>()`.
3. O watcher exige duas respostas consecutivas com
   `body["model_loaded"] == true`.
4. A mensagem de log citada só é escrita depois do parse completo e depois de
   `runtime_capacity`, correção de `total_layers` e anotação `Serving`.
5. Depois da mensagem restam `status.set_ready`, `net.set_idle(false)` e
   `net.announce`. São transições Rust e transporte de rede, sem chamada FFI do
   llama.cpp.

Dentro de `handle_health`, ainda sob o mutex de `InferState`, as fronteiras
nativas são chamadas nesta ordem:

1. `LlmEngine::load_params`: getters do `llama_context` e leitura de Flash
   Attention.
2. `LlmEngine::placement`: `llama_rs_model_placement_json`, que percorre
   `model->tensors_by_name`, buffers GGML, dispositivos e camadas internas do
   llama.cpp.
3. `LlmEngine::context_memory`: `llama_rs_context_memory_json`, que percorre
   `llama_get_memory_breakdown(ctx)` e os tipos de buffer do contexto.

`json_from_wrapper` copia a string retornada pelo wrapper C++ e a libera pelo
par correspondente `llama_rs_string_free`. A inspeção confirmou que o wrapper
usa `malloc` e que a mesma ponte C++ usa `free`; não foi encontrada uma
mistura demonstrável de alocadores nessa fronteira.

## Hipóteses enfraquecidas ou descartadas

- IPC, parsing de resposta de subprocesso ou callback de conclusão entre dois
  processos: não existem neste caminho atual.
- Atualização de status da UI como origem direta de corrupção nativa: o
  `StatusWriter` só altera `Arc<RwLock<RuntimeStatus>>` e notifica Tokio. O
  WebView consulta a API; não há callback Rust para a janela na transição
  `Ready`.
- Callback MoE ou captura de residual como causa imediata para o modelo do
  relato: esses callbacks são usados durante decode. O callback de captura é
  instalado no contexto somente quando há exportação de residual, e o callback
  MoE só é instalado para o caminho MoE. O modelo citado, Llama 3.2 1B denso,
  não fornece evidência de que esses caminhos estejam ativos.
- O erro 1412 como causa suficiente: a mensagem é compatível com uma limpeza
  de janela após a morte do processo. Ainda não há dump para excluir
  definitivamente um defeito independente no WebView2.

## Hipóteses avaliadas antes dos símbolos

1. Corrupção ocorrida dentro de `llama_model_load_from_file`,
   `llama_new_context_with_model` ou backend/driver Windows, detectada somente
   quando uma chamada posterior toca a heap.
2. Access violation em um dos três snapshots nativos de `/health`. Os wrappers
   acessam estruturas internas do fork e capturam exceções C++ comuns, mas não
   capturam uma exceção SEH de acesso inválido. Os marcadores existentes em
   `server.rs` distinguem `load_params`, `placement` e `context_memory` quando
   `RUST_LOG=debug` chega ao arquivo.
3. Corrupção em uma task de rede ou no shell Tao/Wry ocorrendo no mesmo segundo
   da confirmação. A coincidência temporal não permite atribuí-la ao llama.cpp.
4. Há uma fragilidade de ordem no `Drop` de `LlmEngine`: o `Arc` do estado do
   callback MoE é descartado antes de o callback do contexto principal ser
   desregistrado. O caminho explícito `unload` faz a ordem inversa, e o caso
   denso do relato não demonstra que esse callback exista. Deve ser corrigido
   apenas com uma reprodução ou teste de ciclo de vida que o relacione ao
   crash, para não transformar uma hipótese em causa raiz.

## Limitação de confirmação na primeira investigação

Não há GGUF no worktree para executar a carga completa localmente. A
compilação cruzada Windows também não é uma reprodução: esta máquina não tem o
SDK MSVC instalado, e o workflow Windows está fora de escopo sem autorização
explícita do captain. Portanto, a causa raiz do `0xc0000005` ainda não está
confirmada por este checkout.

## Próxima evidência necessária

Executar na máquina real do captain, sem o workflow Windows, com Application
Verifier/PageHeap, WinDbg e `!analyze -v`. Como o binário usa
`windows_subsystem = "windows"`, `RUST_LOG=debug` deve ser acompanhado pelo
`<project_root>/pom.log` gravado diretamente pelo tracing, além do log de
`infer_server_<porta>.log`; redirecionar `*> pom-debug.log` no PowerShell pode
ficar vazio. O stack nativo deve indicar se a falha foi
em `llama_model_load_from_file`, num getter/snapshot de `/health`, em
`net.announce`/Mainline ou no teardown Tao/Wry. Se o captain não quiser usar o
workflow, a mesma coleta pode ser feita localmente com Application Verifier ou
PageHeap e o dump resultante.

## Atualização após a primeira-chance do WinDbg

O captain confirmou `ucrtbase!strlen+0x31` com `rax=0` e os frames do POM ainda
sem símbolos. A primeira leitura apontou corretamente para uma conversão de
`const char *` nulo na fronteira nativa, mas ainda não distinguia placement,
context memory e inventário de devices.

Os módulos `dataexchange`, `twinapi`, `CoreMessaging` e `PROPSYS` não
correspondem a uma API de notificação chamada pelo POM: o código próprio só usa
Tao, Wry e `tray-icon` para janela e bandeja, com strings Rust válidas e
conversões UTF-16 no código das dependências. O WebView2 continua hipótese
secundária até que o frame 01 seja resolvido.

Não foi possível gerar aqui um PDB Windows equivalente para o binário original:
o target Rust MSVC está instalado, mas o host macOS não tem o toolchain C/MSVC
necessário para linkar `ring` e o binário. O captain fechou essa lacuna com um
build Windows diagnóstico com PDB (`/DEBUG:FULL`) do topo da branch.

## Causa raiz confirmada no WinDbg

O frame simbolicado caiu em `llama_rs_backend_devices_json`, no
`vendor/llama-cpp-sys-2/wrapper_common.cpp`. A expressão
`ggml_backend_reg_name(reg)` era passada diretamente como o valor textual de
`{"backend", ...}`. Em uma máquina sem GPU, o registro enumerado pelo GGML
devolveu `nullptr`; o construtor de string do nlohmann::json chamou
`strlen(nullptr)`, produzindo `ucrtbase!strlen+0x31` e a violação `0xc0000005`.

A cadeia confirmada foi:

1. `llama_engine::sysmon::backend_gpu_devices` chama a ponte C++.
2. `llama_engine::sysmon::sample` coleta o snapshot periódico.
3. `pom::webui::catalog_cache::node_info` também pode coletar a mesma lista.

O `model_loaded` do infer_server coincidiu com a coleta do snapshot e não era
uma confirmação IPC que executasse um callback de carregamento no shell. O erro
1412 do WebView2 foi consequência do teardown depois do access violation, não
o frame causador confirmado.

## Correção e auditoria da fronteira nativa

`wrapper_common.cpp` agora centraliza a política em
`llama_rs_backend_device_json`: nomes e descrições nulos viram string vazia,
enquanto `device_id` ausente continua JSON `null` por ser identificador
opcional. A mesma guarda cobre os nomes de device, buffer e buffer type usados
pelos snapshots de placement e context memory, e o nome do registro usado pela
seleção padrão de devices. A enumeração ignora handles nulos antes de consultar
propriedades.

Foi feita uma varredura de todos os `wrapper*.cpp` e `wrapper*.h` do crate. Só
`wrapper_common.cpp` contém implementação C++ e todos os retornos textuais do
backend encontrados estão protegidos; `props.device_id` só entra em `strcmp`
quando os dois ponteiros existem. O teste nativo da lista de devices é
habilitado em Linux e Windows CPU-only, e o teste do ramo de lista vazia é
reproduzível também no macOS sem inicializar o backend Metal.
