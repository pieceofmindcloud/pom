# utils/ - apoio sem estado de rede

> **Mudança de crate:** `brand.rs` virou o crate `crates/brand`; `sysmon.rs`
> foi para `crates/llama-engine` (usa a FFI do ggml para listar GPUs). `node`
> re-exporta ambos em `main.rs` (`use ::brand as brand;`,
> `pub use llama_engine::sysmon`), então `crate::brand::…` e `crate::sysmon::…`
> seguem valendo. `sysmon` recebe a raiz do projeto por `sysmon::set_disk_root`
> no arranque, em vez de chamar `config::project_root()`.
> `KvCacheType` saiu de `config.rs` para `llama_engine::kv_cache` e é
> re-exportado por `crate::config`.

Caminhos de configuração, a TUI de linha de comando, o respawn do processo e o
monitor de sistema. Nada aqui fala com peers; tudo é usado por `main.rs` e
(no caso de `config` e `sysmon`) por praticamente todo o resto do crate.

## Arquivos

- `config.rs` - o `Config` persistido em `pom.json` e a resolução do project
  root, do diretório do `pom.db` e da UI. `POM_DB_DIR` aponta o banco para um
  volume; sem ele, fica na raiz do projeto. O estado operacional usa
  `backend::db::PomDb`; os campos legados continuam serializáveis em `pom.json`
  para não destruir a cópia de segurança antiga. O campo obrigatório
  `profile` é o perfil oficial do nó; um `pom.json` antigo sem ele exige novo
  setup, sem migração.
- `display.rs` - a TUI (banner, assignment, mapa de camadas, tabela de peers,
  health).
- `respawn.rs` - reiniciar este processo preservando o argv do arranque.
- `sysmon.rs` - amostragem de RAM/VRAM/disco por plataforma, com cache.
- `thermal.rs` - política de temperatura, histerese e cooldown como máquina de
  estados pura, sem dependência de fornecedor ou hardware.

## Funções-chave

`config.rs`:
- `brand.rs` - a identidade do produto, lida de `brand.json` na raiz do
  repositório em tempo de compilação (`include_str!`). É a fonte única do nome,
  do slogan e dos arquivos de logo, e **todo nome derivado sai daqui**:
  `config_filename()` (`pom.json`), `env_var("HOME")` (`POM_HOME`),
  `gguf_key("start_layer")` (`pom.start_layer`), `dns_record()` (`_pom`) e o
  nome das chaves de identidade. Renomear o produto é editar `brand.json` - e,
  só para o executável, o `[[bin]]` do `Cargo.toml`, que o cargo exige literal.
  O frontend lê o mesmo arquivo (`apps/frontend/scripts/build-brand.mjs`), então
  tela e binário não têm como discordar sobre o nome.

- `project_root` - `$POM_HOME` → cwd que "parece o projeto" → subindo do
  diretório do executável. É de onde saem `models/`, `venv/` e a config; sem
  isto um `ssh host '/caminho/pom'` lia outra config e não achava fatia
  nenhuma.
- `Config::load`/`save`/`config_path` - `$POM_CONFIG` ou
  `<project_root>/pom.json`.
  `Config::save` grava com permissão exclusiva do proprietário (`0600` no
  Unix), inclusive quando o arquivo já existia; o campo opcional
  `huggingface_token` é segredo e o `Debug` de `Config` o mascara. O runtime
  lê chaves, política térmica e token do `pom.db`, sem usar esses campos como
  fallback.
- `normalize_peer_addr` - completa a porta que falta em endereço digitado
  (IPv6 puro sem porta vira `invalid port value` no resolver, repetido para
  sempre no log); mantém o que está gravado igual ao que se imprime.
- `ui_dist_dir` - `$POM_UI_DIST` ou `<project_root>/apps/frontend/dist`
  (a UI é lida do disco em runtime por `webapp.rs`).
- `model_dir_paths`/`shard_dir_path` - onde procurar modelos inteiros e onde
  escrever/ler fatias. `model_dirs` ausente = detectar (LM Studio + Ollama);
  lista **vazia** = não procure em lugar nenhum; `shard_dir` em branco cai para
  `<project_root>/models`. `shard_max_gb` limita somente a pasta de fatias e
  assume 100 GB quando falta em um `pom.json` antigo.
- A configuração não possui teto absoluto de saída. A capacidade efetiva é
  observada no runtime depois do carregamento do modelo e chega ao gateway por
  `n_ctx`; o gateway agrega o menor valor entre os peers vivos. O único limite
  de geração definido no deploy é `max_new_tokens`, que limita uma perna.
  Consulte `docs/subsystems/implantacao.md`, seção 1.3, e
  `docs/subsystems/inferencia.md`, seção 9.1.
- `cache_type_k`/`cache_type_v` - tipo numérico do cache K/V para todos os
  contextos llama.cpp desta instalação. O padrão é `f16`; os valores aceitos
  são `f32`, `f16`, `bf16`, `q8_0`, `q4_0`, `q4_1`, `iq4_nl`, `q5_0` e
  `q5_1`. Esses campos vivem no `pom.json` e não fazem parte dos parâmetros
  por máquina de uma implantação.
- `DEFAULT_INFERENCE_PORT` (8181) - anunciado desde o arranque mesmo antes de
  a fatia ser atribuída (sem ele o `apply` recusava todo modelo).

`display.rs`:
- `print_banner`/`print_node_info` - identidade no arranque.
- `print_assignment` - faixa de camadas e memória; diz "managed by inference
  server" quando o Python é o dono da memória.
- `print_layer_map` - grade de cobertura por camada (ME verde, peer azul,
  descoberto cinza).
- `print_network_health` - % de cobertura e lista de camadas faltantes.

`respawn.rs`:
- `remember_argv`/`original_argv` - captura o argv (sem argv[0]) no início do
  main; é o que preserva `--advertise-addr` etc. num restart (o respawn ingênuo
  com `Command::new(current_exe())` perdia os argumentos).
- `respawn_self` - spawna `current_exe` + argv original e sai (exit 0); se o
  spawn falhar retorna o erro e o nó segue vivo para reportar.

`sysmon.rs`:
- `sample` - leitura com cache de 1,2 s (`nvidia-smi` custa um spawn por leitura;
  a UI polia mais rápido do que os números mudam). As consultas frequentes ao
  `nvidia-smi` rodam em `spawn_blocking` com `std::process::Command`, sem usar
  o reaper SIGCHLD do Tokio, para não interromper o socket UDP da DHT pkarr.
- `read_memory` - Linux `/proc/meminfo` (MemAvailable, não MemFree); macOS
  `sysctl hw.memsize` + `vm_stat` (active + wired + compressor = "Memory Used"
  do Activity Monitor, e devolve wired para os buffers do Metal); Windows
  `GlobalMemoryStatusEx` declarado à mão para não arrastar `windows-sys`.
- `read_gpu`/`nvidia_smi` - VRAM total/em uso e processos por placa NVIDIA;
  `current_process_gpu_memory_mb` faz a sondagem síncrona do uso do PID atual
  para correlacionar o processo com o snapshot do fit, e devolve `None` quando
  o driver não informa a linha.
  Apple Silicon reporta `GpuKind::Unified` com a nota honesta (sem VRAM
  dedicada para medir) em vez de inventar número. Essa variante só é construída
  no macOS, e fora dele o `dead_code` acusava - daí o `cfg_attr(not(macos),
  allow)` nela. Tirá-la por `cfg` seria pior: `gpu_kind` é campo serializado, e
  `deploy::free_memory_mb` lê o de **outras** máquinas para decidir se a fatia
  cabe.
  o nome segue uma política comum: descrição do backend correspondente,
  `pci.ids` no Linux e identificador da placa como último recurso. O wrapper
  `llama_rs_backend_devices_json` expõe a descrição e o `device_id` do ggml;
  o `device_id` PCI é usado para associar a descrição Vulkan à Radeon certa em
  máquinas com várias placas. Apple Silicon reporta `GpuKind::Unified` com a
  nota honesta (sem VRAM dedicada para medir) em vez de inventar número, usando
  a descrição Metal e depois `system_profiler` para o nome. Essa variante só é
  construída no macOS, e fora dele o `dead_code` acusava - daí o
  `cfg_attr(not(macos), allow)` nela. Tirá-la por `cfg` seria pior:
  `gpu_kind` é campo serializado, e `deploy::free_memory_mb` lê o de
  **outras** máquinas para decidir se a fatia cabe.
- `sample_disk` - `df -m` no project root (não no cwd); Windows
  `GetDiskFreeSpaceExW`. `sample_disk_at` recebe o destino configurado e mede
  o volume que contém esse caminho, inclusive quando a pasta ainda não existe.
  A tela usa essa variante para conferir se a saída de uma fatia cabe no disco
  real apontado pelo operador.

`sysmon.rs` também publica `GpuStat.temperature_c`: NVIDIA vem da mesma linha
`nvidia-smi` que enumera VRAM e utilização, Radeon vem de `amdgpu`/hwmon, e
GPU unificada fica explicitamente sem leitura. `None` significa temperatura
desconhecida e nunca é convertido em alarme pela proteção térmica.

## Ligações

- **`config`** é usado por quase tudo: `main.rs`, `network/identity.rs`
  (`key_path`), `models/catalog.rs` (pastas varridas), `backend/transfer.rs`
  (`safe_model_path`), `backend/webapp.rs` (`ui_dist_dir`), `backend/deploy.rs`,
  `webui.rs`.
- **`display`** só em `main.rs` (banner + loop de refresh).
- **`respawn`** é chamado pelo `backend/devupdate.rs` (atualizar e reiniciar
  pela interface) e por `main.rs` no arranque.
- **`sysmon`** em `main.rs` (hostname) e `webui.rs` (endpoints `/api/ui/*`).

## Defeitos anotados

- `impl Display for Role` (`config.rs`) devolve rótulos humanos fixos em pt-BR
  (`Principal`, `Nó`), e é esse texto que sai no `role` de `/api/ui/node` e
  aparece cru na tela de rede, ao lado do `profile`. Duas consequências: a
  string visível não passa pela i18n (a tela em inglês mostra "Nó"), e o mesmo
  campo `role` é um **token** (`principal`/`node`) em `/admin/nodes`, então os
  dois endpoints falam línguas diferentes. O `profile` já nasce token e é
  traduzido na tela (`t("profile.<perfil>")`); `role` deveria seguir o mesmo
  caminho. Não corrigido aqui porque muda o contrato de `/api/ui/node`.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
