# utils/ - apoio sem estado de rede

Caminhos de configuração, a TUI de linha de comando, o respawn do processo e o
monitor de sistema. Nada aqui fala com peers; tudo é usado por `main.rs` e
(no caso de `config` e `sysmon`) por praticamente todo o resto do crate.

## Arquivos

- `config.rs` - o `Config` persistido em `rdma-p2p.json` e a resolução do
  project root / diretório da UI.
- `display.rs` - a TUI (banner, assignment, mapa de camadas, tabela de peers,
  health).
- `respawn.rs` - reiniciar este processo preservando o argv do arranque.
- `sysmon.rs` - amostragem de RAM/VRAM/disco por plataforma, com cache.

## Funções-chave

`config.rs`:
- `project_root` - `$RDMA_P2P_HOME` → cwd que "parece o projeto" → subindo do
  diretório do executável. É de onde saem `models/`, `venv/` e a config; sem
  isto um `ssh host '/caminho/rdma-p2p'` lia outra config e não achava fatia
  nenhuma.
- `Config::load`/`save`/`config_path` - `$RDMA_P2P_CONFIG` ou
  `<project_root>/rdma-p2p.json`.
- `normalize_peer_addr` - completa a porta que falta em endereço digitado
  (IPv6 puro sem porta vira `invalid port value` no resolver, repetido para
  sempre no log); mantém o que está gravado igual ao que se imprime.
- `ui_dist_dir` - `$RDMA_P2P_UI_DIST` ou `<project_root>/apps/frontend/dist`
  (a UI é lida do disco em runtime por `webapp.rs`).
- `model_dir_paths`/`shard_dir_path` - onde procurar modelos inteiros e onde
  escrever/ler fatias. `model_dirs` ausente = detectar (LM Studio + Ollama);
  lista **vazia** = não procure em lugar nenhum; `shard_dir` em branco cai para
  `<project_root>/models`.
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
  a UI polia mais rápido do que os números mudam).
- `read_memory` - Linux `/proc/meminfo` (MemAvailable, não MemFree); macOS
  `sysctl hw.memsize` + `vm_stat` (active + wired + compressor = "Memory Used"
  do Activity Monitor, e devolve wired para os buffers do Metal); Windows
  `GlobalMemoryStatusEx` declarado à mão para não arrastar `windows-sys`.
- `read_gpu`/`nvidia_smi` - VRAM total/em uso e processos por placa NVIDIA;
  Apple Silicon reporta `GpuKind::Unified` com a nota honesta (sem VRAM
  dedicada para medir) em vez de inventar número.
- `sample_disk` - `df -m` no project root (não no cwd); Windows
  `GetDiskFreeSpaceExW`. É o que a tela usa para saber se outra fatia cabe.

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

Nenhum.
