# Preview de capacidade sem bloquear o arranque: plano de implementação

> **Para agentes de implementação:** executar este plano tarefa por tarefa,
> com teste falho antes da implementação e verificação nova antes da entrega.

**Goal:** garantir que o nó sempre abre o gateway enquanto mantém a prévia de
capacidade da tela Implantar útil e limitada.

**Architecture:** o endpoint de preview usa apenas a leitura de GGUF, a conta
analítica de peso + KV e o retrato atual de VRAM. O `fit_plan` e
`llama_rs_fit_params` ficam somente no caminho de carregamento real. A prévia
tem um limite de admissões sem fila infinita, e o coordenador limita o fan-out
por `Step` ao consultar peers.

**Tech Stack:** Rust, Tokio, Axum, `cargo test`, `cargo check`, documentação
Markdown em pt-BR.

**Spec:** launch brief v1 desta tarefa; não há spec dedicada separada.

## Global Constraints

- Não alterar o checkout nem os arquivos de `llama.cpp`.
- Não desfazer `dc0882f`, `83b6242`, `03abd4e`, `27fda8b`, `c5933d8`,
  `b5bc0ad`, `7ace9d5`, `3ca13e2` ou `4388557`.
- O preview não pode chamar `llama_backend_init`, `llama_rs_fit_params` ou
  `llm::preview_fit`.
- O carregamento real mantém o `fit_plan` e suas decisões nativas.
- Não editar cópias geradas em `apps/frontend/src/docs` nem `CHANGELOG.md`.
- Atualizar a documentação das pastas e dos subsistemas tocados no mesmo
  commit.

## Diagnóstico confirmado

O `gdb` anexado somente para leitura ao processo remoto mostrou o fio
principal em `parking_lot::condvar::Condvar::wait_until_internal` chamado por
`tokio::runtime::park::Inner::park`, sem frame de mutex do aplicativo. Os
workers também estavam estacionados, enquanto o fio DHT aguardava `recvfrom`.
Isso prova que a amostra remota era um runtime sem tarefa acordada, não um
deadlock no `operation` mutex.

O defeito causal no código é anterior: `enrich_capacity_previews` dispara uma
requisição por `Step`, todas em `join_all`; cada handler chama
`spawn_blocking(llm::preview_fit)`. Assim, nove passos produzem nove chamadas
ao mesmo caminho nativo de fit, fora do mutex de `LlmEngine`, concorrendo com
o `spawn_blocking` que faz o carregamento real. `llama_backend_init` não tem
uma fronteira de serialização no lado Rust, e o registro de backends e o
guard global de logging usados pelo fit não formam contrato de concorrência
para chamadas paralelas. A espera do DHT não é a causa: `start_p2p_stack`
inicia o recebimento em background e o gateway é lançado sem esperar a
descoberta terminar.

## Tasks

### Task 1: Escrever a regressão antes da implementação

**Concluída.** O teste de arquitetura falhou contra `ec353ff` porque o handler
continha `preview_fit`; depois da remoção passou. Os testes de limite e ordem de
arranque também ficaram cobrindo as fronteiras novas.

Modificar os testes de `webui` para provar que:

1. a estimativa de capacidade não entra no caminho `llm::preview_fit`;
2. o limite de admissões recusa excesso sem aguardar uma fila;
3. o fan-out do coordenador observa o limite de chamadas simultâneas;
4. o bind do gateway continua independente do carregamento do modelo.

O teste de arquitetura deve falhar contra `ec353ff` com uma asserção explícita
de que o endpoint não pode conter a chamada nativa. O teste de limite deve
usar o mesmo gate que a rota usará, preenchê-lo e verificar `429` sem esperar.

Executar primeiro os testes focados e registrar o RED.

### Task 2: Trocar a prévia nativa pela estimativa analítica

**Concluída.** O endpoint agora lê apenas metadata GGUF, calcula peso + KV e
consulta a VRAM viva. `fit_plan` continua somente no carregamento real.

Remover a chamada a `spawn_blocking(llm::preview_fit)` do endpoint. Manter a
conta existente de `range_cost`, a amostra de `sysmon` e a distribuição por
VRAM livre. O retorno deve marcar a divisão como estimada e não alegar que o
contexto ou as camadas efetivas foram decididos pelo llama.cpp. Remover tipos e
comentários de preview que ficarem sem consumidores, sem introduzir warnings.

### Task 3: Impor fronteiras de concorrência

**Concluída.** Há quatro vagas imediatas por processo e o fan-out do plano usa
quatro futures em `buffer_unordered`, sem fila ilimitada.

Adicionar um semáforo de processo com `try_acquire` para o endpoint, devolvendo
`429 Too Many Requests` quando as vagas estiverem ocupadas. No coordenador,
trocar `join_all` irrestrito por `buffer_unordered` com limite pequeno e
documentado. Nenhuma dessas fronteiras pode ser usada pelo carregamento real.

### Task 4: Documentar a decisão e verificar

**Concluída.** A documentação Rust, frontend e subsistemas foi atualizada. O
`cargo check -p node`, os testes focados e as baterias exigidas passaram sem
avisos novos. A suíte integral foi interrompida depois de permanecer presa por
mais de dez minutos em `prefix_reuse_lab`, fora do escopo desta alteração.

Atualizar `crates/node/src/backend/AGENTS.md`,
`crates/node/src/inference/AGENTS.md`, `docs/subsystems/implantacao.md` e
`docs/subsystems/inferencia.md` para dizer que a prévia é analítica e que o
fit nativo é exclusivo do load. Rodar os testes focados, `cargo check` sem
warnings novos e as baterias `mtp`, `fit_memory`, `sampler_behaviour`,
`device_placement`, `cycle` e `gateway`. Reexecutar o teste de regressão sem
`--ignored`.

### Task 5: Entregar na branch pedida

Criar `feat/implantar2` a partir da base de trabalho que inclui `ec353ff`,
aplicar as correções em um commit sem coautor automático e publicar somente
essa branch na `origin`. Não tocar `feat/implantar`, `main` e não abrir PR.
