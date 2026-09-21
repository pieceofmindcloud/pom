# Variantes de backend na imagem Docker

> **Para agentes de implementação:** executar cada tarefa neste plano em ordem e manter as caixas atualizadas.

**Objetivo:** impedir que serviços sem NVIDIA exijam `libcuda.so.1`, mantendo uma imagem e um build Compose comuns para todos os nós.

**Arquitetura:** o Dockerfile compila três variantes do `pom`: CUDA+Vulkan para nós NVIDIA, somente Vulkan para Radeon e CPU sem backends de GPU para nós sem acelerador. O entrypoint existente seleciona o executável por `POM_BIN`; os Compose versionados e o gerador remoto da Ksia atribuem explicitamente a variante adequada.

**Tecnologias:** Docker Compose, Dockerfile multi-stage, Cargo features, shell e Python 3.

**Especificação:** inbox Firstmate `[key=node_cpu_libcuda]` e relato do Salem recebido em 2026-09-21.

## Restrições globais

- Todos os serviços continuam usando a mesma imagem e contexto de build.
- Ksia mantém GPUs NVIDIA 0 e 1 isoladas entre seus três serviços GPU.
- Salem mantém NVIDIA em `node-gpu` e `node-gpu2`, Vulkan Radeon em `node-gpu-3` e CPU sem GPU em `node-cpu`.
- `node-cpu` não recebe reserva nem dispositivo de GPU.
- Não executar deploy na Ksia.
- Publicar diretamente em `origin/feat/enterprise`, sem pipeline, PR ou force-push.

---

### Tarefa 1: reproduzir o contrato dos Compose e escrever a regressão

**Arquivos:**
- Criar `scripts/test-docker-compose.py`.
- Usar `docker-compose.yml`, `docker-compose.salem.yml` e `docker-compose.ksia.yml` como entradas reais de Compose.

- [x] Escrever o teste para executar `docker compose config --format json` nos três arquivos e confirmar quatro serviços, imagem/build idênticos, as seleções `POM_BIN` e a visibilidade de dispositivos.
- [x] Rodar `python3 scripts/test-docker-compose.py`; confirmar falha porque os Compose ainda não selecionam binários por serviço.

### Tarefa 2: compilar e selecionar três binários na mesma imagem

**Arquivos:**
- Modificar `Dockerfile`.
- Modificar `docker-compose.common.yml`, `docker-compose.yml`, `docker-compose.salem.yml` e `docker-compose.ksia.yml`.
- Modificar o gerador em `/Users/jairpereira/scripts/pom-docker.sh` para marcar serviços sem GPUs como CPU-only.

- [x] Compilar `pom` com `cuda,vulkan`, somente `vulkan` e sem features, copiando os resultados para caminhos distintos da imagem runtime.
- [x] Manter o CUDA como `POM_BIN` padrão; atribuir `/usr/local/bin/pom-vulkan` ao `node-gpu-3` Salem e `/usr/local/bin/pom-cpu` aos serviços CPU.
- [x] Atualizar o gerador dinâmico da Ksia para herdar o ambiente comum e sobrescrever `POM_BIN` para serviços sem atribuição NVIDIA.
- [x] Rodar `python3 scripts/test-docker-compose.py` e confirmar a topologia e os binários esperados.

### Tarefa 3: documentar e validar a imagem

**Arquivos:**
- Atualizar `scripts/AGENTS.md`, `docs/subsystems/build-execucao.md` e este plano.
- Atualizar `docs/AGENTS.md` para indexar o plano.

- [x] Rodar `bash scripts/test-docker-entrypoint.sh` para verificar seleção de executável pelo entrypoint.
- [x] Validar `docker compose config` para Salem e Ksia; o daemon Docker local não está ativo, então o build da imagem não pode rodar neste worktree.
- [x] Executar `git diff --check`, revisar o Compose efetivo e confirmar que não houve deploy remoto.
- [x] Commitar, fazer fetch e rebase em `origin/feat/enterprise`, publicar sem force-push e conferir o SHA via `git ls-remote`.
