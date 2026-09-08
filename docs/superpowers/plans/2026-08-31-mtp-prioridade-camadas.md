# Prioridade de camadas sobre MTP - Implementation Plan

> Executado nesta mesma sessão, seguindo TDD e verificação antes da conclusão.

**Goal:** Garantir que a seleção de camadas do modelo principal nunca reduza a colocação na GPU para reservar VRAM ao MTP, mantendo o MTP em VRAM quando houver espaço e usando o companheiro em RAM ou indisponibilidade quando não houver.

**Architecture:** Separar o encaixe obrigatório do modelo principal da sondagem opcional do contexto MTP. O primeiro usa a margem por dispositivo sem custo MTP; o segundo só é aceito se conservar a colocação integral do plano principal. O plano carregado passa a declarar se o alvo deve carregar MTP em VRAM ou deixá-lo para o fallback em RAM.

**Tech Stack:** Rust, `llama_rs_fit_params`, testes unitários de `crates/node/src/inference/llm.rs`, documentação pt-BR em `docs/subsystems/inferencia.md` e `crates/node/src/inference/AGENTS.md`.

**Spec:** Priorizar camadas principais, contexto principal e somente então MTP por dispositivo. Preservar reserva, fallback RAM, telemetria, interruptor de recusa e dimensionamento de KV/estado recorrente. Não modificar `llama.cpp`.

## Global Constraints

- Não alterar `llama.cpp`, `kv_seq_budget` ou o dimensionamento do estado recorrente.
- Não desligar MTP por padrão; quando o encaixe integral couber, o MTP deve continuar em VRAM.
- Não mexer em arquivos gerados nem em `CHANGELOG.md`.
- Atualizar a documentação da pasta e do subsistema no mesmo commit.
- Verificar sem afirmar resultados não executados; preservar falhas amplas preexistentes.

## Task 1: Reproduzir a inversão com uma política testável - concluída

**Files:** `crates/node/src/inference/llm.rs`

- Adicionar um teste que represente o caso observado: o encaixe principal permanece integral, enquanto o encaixe conjunto teria de reduzir camadas; esse caso deve escolher RAM/indisponível para MTP.
- Cobrir também MTP desmarcado e o caso em que ambos os encaixes integrais cabem.
- Rodar o teste novo contra o código-base para confirmar o sinal vermelho antes da implementação.

## Task 2: Separar fit principal e sondagem MTP - concluída

**Files:** `crates/node/src/inference/llm.rs`

- Extrair a configuração/execução do fit para permitir uma execução sem MTP e outra opcional com a reserva por dispositivo.
- Fazer o plano principal ser a autoridade para `n_gpu_layers`, split de dispositivos e overrides.
- Aceitar o plano MTP em VRAM somente quando ele não alterar a colocação integral do plano principal.
- Propagar no `FitPlan` a decisão de carregar MTP em VRAM ou deixar o alvo sem MTP para o companheiro em RAM.
- Manter o caminho de carregamento do companheiro explícito e a telemetria/razões existentes.

## Task 3: Cobrir as combinações de orçamento - concluída

**Files:** `crates/node/src/inference/llm.rs`

- Testar espaço sobrando, disputa sem espaço para ambos, ausência de espaço também para o recuo em RAM, MTP desmarcado e múltiplos dispositivos.
- Garantir por teste que um fit conjunto reduzido nunca prevalece sobre o plano principal integral.
- Manter os testes de reserva e de distribuição por dispositivo que documentam o custo do segundo contexto.

## Task 4: Apurar e documentar o buffer de cálculo - concluída

**Files:** `docs/subsystems/inferencia.md`, `crates/node/src/inference/AGENTS.md`

- Documentar que o custo reportado vem do `compute` do scheduler, separado do cache KV.
- Registrar a relação observada no código upstream entre a reserva do grafo/attention mask e a janela `n_ctx`, sem alterar a justificativa do cache.
- Se não houver uma redução segura sem redesenhar o pareamento do contexto, manter o dimensionamento e explicar o motivo.

## Task 5: Verificação e entrega - concluída

- Rodar `cargo fmt --check` no escopo tocado e `cargo check` sem novos avisos.
- Rodar as baterias focadas `mtp`, `fit_memory`, `sampler_behaviour`, `device_placement` e `cycle`, registrando limitações de ambiente sem inventar resultados.
- Revisar o diff, fazer `git fetch origin`, rebasear sobre `origin/feat/implantar`, integrar/publicar somente nessa linha e registrar o SHA no status.

Verificação executada: `rustfmt --check` do `llm.rs`, `cargo check -p node
--bin pom` e as cinco baterias focadas passaram. O `cargo fmt --all -- --check`
amplo continua apontando apenas os cinco arquivos preexistentes fora do escopo.

## Test Commands

- `rtk cargo test -p node --bin pom mtp -- --nocapture`
- `rtk cargo test -p node --bin pom fit_memory -- --nocapture`
- `rtk cargo test -p node --bin pom sampler_behaviour -- --nocapture`
- `rtk cargo test -p node --bin pom device_placement -- --nocapture`
- `rtk cargo test -p node --bin pom cycle -- --nocapture`
- `rtk cargo check -p node --bin pom`
- `rtk cargo fmt --check`
