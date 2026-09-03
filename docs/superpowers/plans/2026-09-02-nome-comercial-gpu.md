# Nome comercial da GPU - Plano de implementacao

> **Para agentes:** executar cada etapa com TDD e verificar a saida antes de
> marcar a etapa como concluida.

**Objetivo:** Fazer AMD, NVIDIA e macOS exibirem o nome comercial fornecido
pela fonte generica de maior autoridade disponivel, preservando um fallback
identificavel e a associacao correta em maquinas com varias placas.

**Arquitetura:** O wrapper comum do llama.cpp expoe os dispositivos reais do
backend como JSON, incluindo descricao e identificador PCI. O sysmon resolve o
nome com uma politica comum: descricao do backend correspondente, base
`pci.ids` no Linux, e identificador da placa; NVIDIA conserva o nome do
`nvidia-smi` e macOS usa a descricao Metal, com `system_profiler` como fallback.

**Tecnologias:** Rust, Tokio, serde_json, C++ do wrapper llama.cpp, testes
unitarios do binario `pom`.

**Especificacao:** Brief `FIRSTMATE_OP v1 launch-brief` desta tarefa.

## Restricoes globais

- Nao escrever tabelas fixas de modelos de GPU.
- Nao deixar uma GPU sem nome legivel nem remover a placa quando fontes falham.
- Resolver por identificador PCI quando disponivel para nao cruzar duas placas.
- Atualizar AGENTS.md e README de subsistema afetados no mesmo commit.
- Confirmar as 10 falhas de teste preexistentes e nao executar no-mistakes.

### Tarefa 1: Resolver nomes de GPU por fonte comum

**Arquivos:**
- Modificar: `vendor/llama-cpp-sys-2/wrapper_common.h`
- Modificar: `vendor/llama-cpp-sys-2/wrapper_common.cpp`
- Modificar: `crates/node/src/utils/sysmon.rs`
- Modificar: `crates/node/src/utils/AGENTS.md`
- Modificar: `docs/subsystems/build-execucao.md` se a documentacao de build
  precisar registrar a ponte do backend
- Testar: `crates/node/src/utils/sysmon.rs` e testes do binario `pom`

**Interfaces:**
- Produz: `llama_rs_backend_devices_json(char **out_json)` com cada
  dispositivo real, seu backend, descricao e `device_id` opcional.
- Produz: helpers Rust puros para selecionar o nome e associar descricao por
  `device_id`, testaveis sem hardware ou executaveis externos.

- [x] Escrever testes falhos para a precedencia backend, fallback PCI, fallback
  por identificador e duas placas com IDs PCI distintos.
- [x] Rodar os testes novos e confirmar falha pela politica ausente.
- [x] Implementar a ponte JSON e a politica comum no sysmon.
- [x] Atualizar AMD, NVIDIA e macOS para consumirem a politica sem mudar os
  indices publicados nem confundir dispositivos.
- [x] Rodar os testes novos, a suite do node, formatacao e build relevante.
- [x] Revisar a documentacao afetada e marcar este plano como concluido.
- [x] Commitar em `fm/rdma-nome-real-das-placas`.
- [x] Rebasear em `origin/feat/implantar` antes da publicacao autorizada.
