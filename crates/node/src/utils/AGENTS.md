# utils/ - apoio sem estado de rede

> **Mudança de crate:** `brand.rs` virou o crate `crates/brand`; `sysmon.rs`
> foi para `crates/llama-engine` (usa a FFI do ggml para listar GPUs). `node`
> re-exporta ambos em `main.rs` (`use ::brand as brand;`,
> `pub use llama_engine::sysmon`), então `crate::brand::…` e `crate::sysmon::…`
> seguem valendo. `sysmon` recebe a raiz do projeto por `sysmon::set_disk_root`
> no arranque, em vez de chamar `config::project_root()`.
> `KvCacheType` saiu de `config.rs` para `llama_engine::kv_cache` e é
> re-exportado por `crate::config`.

Caminhos de configuração, o respawn do processo e o monitor de sistema. Nada aqui fala com peers; tudo é usado por `main.rs` e
(no caso de `config` e `sysmon`) por praticamente todo o resto do crate.

## Arquivos

- `config.rs` - o `Config` persistido em `pom.json` e a resolução do project
  root, do diretório do `pom.db` e da UI. `POM_DB_DIR` aponta o banco para um
  volume; sem ele, fica na raiz do projeto. O estado operacional usa
  `backend::db::PomDb`; os campos legados continuam serializáveis em `pom.json`
  para não destruir a cópia de segurança antiga. O campo obrigatório
  `profile` é o perfil oficial do nó; um `pom.json` antigo sem ele exige novo
  setup, sem migração.
  `NetworkSettings` guarda nome, organização, descrição, regras, canais,
  visibilidade (`local`, `private`, `public`), política de acesso e localização
  geo (país, cidade, coordenadas e label canônico). `setup_pending` permite
  preservar preferências locais ao sair da rede e voltar ao wizard mesmo com
  `pom.json` presente.
  `ResourcePolicy` guarda os percentuais de RAM e CPU reservados ao sistema.
  O primeiro acesso usa `~/.pom/models` e `~/.pom/models_sliced` como padrões
  e grava `shard_max_gb` para limitar as fatias compartilhadas.
  `cluster_logs_min_level` também vive no JSON, aceita
  `trace`, `debug`, `info`, `warn` e `error`, e assume `info` quando ausente;
  ele só controla o que `CaptureLayer` enfileira.
- `crash.rs` - native crash report, installed first thing in `main`: on
  SIGSEGV/SIGBUS/SIGILL/SIGFPE/SIGABRT it writes the signal, the faulting
  address, the thread and the native stack as `binary(+offset)` to stderr
  (async-signal-safe calls only), then restores the default action and
  re-raises, so the exit status and core dump do not change. llama.cpp runs
  in-process and a native fault otherwise leaves no line in the node log.
  Resolve a frame with `addr2line -f -C -e <binary> <offset>` against the
  same build. A kernel line `in pom[X,...]` gives the **file** offset: add the
  difference between the executable `LOAD` segment's `p_vaddr` and `p_offset`
  (`readelf -lW`) before `addr2line`.
- `respawn.rs` - reiniciar este processo preservando o argv do arranque,
  criando uma nova sessão em Unix e redirecionando stdout/stderr para
  `<project_root>/pom.log` para que falhas logo após o reinício deixem rastro.
  `respawn_at` recebe o caminho capturado antes de uma troca atômica, evitando
  o caminho `(deleted)` que `current_exe()` pode devolver em Unix.
- `sysmon.rs` - amostragem de RAM/VRAM/disco por plataforma, com cache.
- `thermal.rs` - política de temperatura, histerese e cooldown como máquina de
  estados pura, sem dependência de fornecedor ou hardware. A validação exige
  corte de no mínimo 70 °C e cooldown de no máximo 60 segundos.

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
  fallback. Ao carregar, a credencial administrativa legada em texto é
  migrada uma vez para hash Argon2id; o arquivo nunca persiste a senha pura.
- `normalize_peer_addr` - completa a porta que falta em endereço digitado
  (IPv6 puro sem porta vira `invalid port value` no resolver, repetido para
  sempre no log); mantém o que está gravado igual ao que se imprime.
- `ui_dist_dir` - `$POM_UI_DIST` ou `<project_root>/apps/frontend/dist`
  (a UI é lida do disco em runtime por `webapp.rs`).
- `model_dir_paths`/`shard_dir_path` - onde procurar modelos inteiros e onde
  escrever/ler fatias. A configuração criada pelo wizard usa `~/.pom/models` e
  `~/.pom/models_sliced`; configurações existentes ainda conservam sua escolha.
  `model_dirs` vazio não procura em lugar nenhum. `shard_max_gb` limita somente
  a pasta de fatias e assume 100 GB quando falta em um `pom.json` antigo.
  `POM_MODEL_DIRS` (várias pastas, separadas como o `PATH`) e `POM_SHARD_DIR`
  vencem os campos do `pom.json` nesses dois acessores (`model_dirs_from_env`,
  `shard_dir_from_env`); não são persistidas, e vazias contam como ausentes.
  `POST /api/ui/paths` não grava o campo que uma variável define, e
  `GET /api/ui/paths` devolve o nome dela em `model_dirs_env`/`shard_dir_env`.
- `workspace_root_path` - raiz absoluta efetiva da workspace compartilhada por
  projetos do usuário e dados dinâmicos de plugins. O campo persistido é
  opcional para configurações antigas e o default é `~/pom_workspace`, sem
  pergunta no wizard e separado das pastas de modelos.
- A configuração não possui teto absoluto de saída. A capacidade efetiva é
  observada no runtime depois do carregamento do modelo e chega ao gateway por
  `n_ctx`; o gateway agrega o menor valor entre os peers vivos. O único limite
  de geração definido no deploy é `max_new_tokens`, que limita uma perna.
  Consulte `docs/subsystems/deployment.md`, seção 1.3, e
  `docs/subsystems/inference.md`, seção 9.1.
- `cache_type_k`/`cache_type_v` - tipo numérico do cache K/V para todos os
  contextos llama.cpp desta instalação. O padrão é `f16`; os valores aceitos
  são `f32`, `f16`, `bf16`, `q8_0`, `q4_0`, `q4_1`, `iq4_nl`, `q5_0` e
  `q5_1`. Esses campos vivem no `pom.json` e não fazem parte dos parâmetros
  por máquina de uma implantação.
- `cluster_logs_min_level` - limiar mínimo da captura de logs de implantação;
  o console mantém sua política `RUST_LOG` independente. A retenção é fixa no
  principal e não é uma preferência da auditoria de requisições.
- `DEFAULT_INFERENCE_PORT` (8181) - anunciado desde o arranque mesmo antes de
  a fatia ser atribuída (sem ele o `apply` recusava todo modelo).

`respawn.rs`:
- `remember_argv`/`original_argv` - captura o argv (sem argv[0]) no início do
  main; é o que preserva `--advertise-addr` etc. num restart (o respawn ingênuo
  com `Command::new(current_exe())` perdia os argumentos).
- `respawn_self` - spawna `current_exe` + argv original em uma sessão
  independente, com stdout/stderr em `pom.log`, e sai (exit 0); se abrir o log
  ou o spawn falhar retorna o erro e o nó segue vivo para reportar.
- `respawn_at` - mesma política de `respawn_self`, mas usando o executável
  fornecido pela atualização autorizada depois do rename.

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

`config.rs` mantém `validate_network_settings` como a validação única do bloco
de rede. Ela é usada tanto pelo wizard quanto pela edição posterior e exige
nome público, país e cidade quando a visibilidade é pública; redes locais e
privadas podem deixar esses campos vazios.

`NetworkSettings.accepted_enrollments` (`AcceptedEnrollments`, a list of
`EnrollmentRole`: `node`, `infer`) is what the network advertises as open to
new members. A missing field deserializes to every role, an empty list means
closed. It is an advertisement for the public directory only: admission
(`Network::public_peer_is_admitted`) does not read it and current members are
never affected.

`NetworkSettings.lobby_chat` opens the lobby room of the network Chat to
machines that are not members (`network/network_lobby.rs`). It is off by
default and only takes effect on a public network with the network Chat on.
`NetworkSettings.public_stats` publishes the served models, node count and
total VRAM to non-members, answered live by the principal; off by default and
public networks only.

## Ligações

- **`config`** é usado por quase tudo: `main.rs`, `network/identity.rs`
  (`key_path`), `models/catalog.rs` (pastas varridas), `backend/transfer.rs`
  (`safe_model_path`), `backend/webapp.rs` (`ui_dist_dir`), `backend/deploy.rs`,
  `webui.rs`.
- **`display`** só em `main.rs` (banner + loop de refresh).
- **`respawn`** é chamado pelo `backend/devupdate.rs` (atualizar e reiniciar
  pela interface) e por `main.rs` no arranque.
- **`sysmon`** em `main.rs` (hostname) e `webui.rs` (endpoints `/api/ui/*`).

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
