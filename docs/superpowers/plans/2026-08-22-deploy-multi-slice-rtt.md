# Mapa de implantação com múltiplas faixas e RTT - Plano de implementação

> **Para agentes:** use a skill `superpowers:executing-plans` ou `superpowers:subagent-driven-development` para executar este plano task a task. Os passos usam o formato de checklist.

**Objetivo:** aceitar planos válidos que atribuem várias faixas à mesma máquina, recusar cobertura incompleta e preservar a direção e o RTT de cada aresta no mapa.

**Arquitetura:** a validação continuará no planejador Rust, usando a cobertura global por camada como fonte de verdade. A opção `merge` continuará unindo apenas grupos adjacentes quando solicitada, mas não será usada como requisito para faixas separadas. O canvas derivará uma única lista de estágios ordenados e desenhará somente pontes entre estágios consecutivos, ignorando autoarestas e calculando endpoints conforme a posição relativa dos nós.

**Tecnologias:** Rust, testes unitários de `crates/node`, React 18, TypeScript, Vitest, Testing Library, Vite e Chrome via `chrome-devtools-axi`.

**Especificação:** requisitos do launch brief `FIRSTMATE_OP: v1 launch-brief` recebido em 2026-08-22.

## Restrições globais

- Não executar deploy real, não reiniciar a ksia e não alterar scripts fora deste worktree.
- Não alterar `CHANGELOG.md` nem arquivos gerados.
- Preservar mensagens i18n existentes para planos inválidos.
- Não relaxar validação de gap, sobreposição, ordem inválida, faixa fora do modelo ou faixa inexistente.
- Não fazer push, PR, merge ou deploy remoto.

## Evidência antes da implementação

Reprodução feita no navegador em `http://127.0.0.1:5199/implantar`, com um nó local simulado atrás do proxy da UI, sem chamada de deploy:

- Caso do screenshot: dez faixas `L0-3` a `L36-39`, `ksia` recebendo `L0-3` até `L32-35` e `ksia-2` recebendo `L36-39`. A UI exibiu `ksia ficou com duas faixas (L0-3 e L32-35)` e `Implantar` desabilitado.
- Todas as faixas numa máquina: as dez faixas em `ksia`. A UI exibiu a mesma regra falsa, agora com `L0-3 e L4-7`, e `Implantar` desabilitado.
- Gap real: `ksia` recebeu `L0-3`, `ksia-2` recebeu `L8-11`. A UI exibiu `as camadas L4-7 não estão em nenhuma fatia escolhida` e `Implantar` desabilitado.
- Geometria: no caso do screenshot, a inspeção DOM encontrou nove rótulos `RTT -` sobrepostos, provenientes de autoarestas entre estágios consecutivos que permaneciam em `ksia`; a única transição entre máquinas era `ksia -> ksia-2`.

## Arquivos envolvidos

- `crates/node/src/backend/deploy.rs`: predicado de cobertura/atribuição e testes do planner.
- `apps/frontend/src/components/DeployNetworkCanvas.tsx`: derivação de estágios, arestas, endpoints e rótulos RTT.
- `apps/frontend/src/routes/DeployModel.test.tsx`: regressões E2E da tela para seleções válidas e inválidas.
- `apps/frontend/src/components/DeployNetworkCanvas.test.tsx`: regressões do grafo visual e da direção/RTT.
- `AGENTS.md`, `docs/subsystems/implantacao.md`, `crates/node/src/backend/AGENTS.md`, `apps/frontend/src/routes/AGENTS.md` e `apps/frontend/src/components/AGENTS.md`: documentação atualizada para o comportamento entregue.

### Task 1: Fixar a validação no planner com testes RED

**Arquivos:** `crates/node/src/backend/deploy.rs`.

- [x] Substituir o teste que espera recusa por uma regressão que aceite faixas não contíguas no mesmo destino.
- [x] Adicionar regressão para todas as faixas em uma única máquina.
- [x] Alterar o teste de gap para exigir `blocked` e manter a faixa em `missing`.
- [x] Confirmar o RED dos testes novos; a execução Rust ficou separadamente bloqueada na compilação de `llama-cpp-sys-2`.

### Task 2: Corrigir o planner minimamente

**Arquivos:** `crates/node/src/backend/deploy.rs`.

- [x] Fazer cada faixa virar seu próprio grupo quando `merge` está desligado.
- [x] Quando `merge` está ligado, formar grupos somente de corridas adjacentes da mesma máquina; faixas separadas permanecem passos independentes e válidos.
- [x] Transformar o primeiro gap em `blocked`, mantendo a mensagem existente em `warnings` quando aplicável.
- [x] Manter a ordenação por camada e a numeração de `position` depois da expansão dos grupos.
- [x] Fazer o auto-planner reutilizar uma máquina quando não houver peer livre, sem bloquear cobertura completa.
- [ ] Rodar os testes focados do planner e a suíte Rust relevante. Bloqueado pela dependência de build `llama-cpp-sys-2`.

### Task 3: Fixar o grafo visual com testes RED

**Arquivos:** `apps/frontend/src/components/DeployNetworkCanvas.tsx`, `apps/frontend/src/components/DeployNetworkCanvas.test.tsx`.

- [x] Adicionar teste de três estágios que exija somente `A -> B` e `B -> C`, com RTTs próprios e sem autoaresta.
- [x] Adicionar teste de endpoint que mova o receptor para a esquerda e confirme que a seta ainda começa no emissor e termina no receptor.
- [x] Confirmar falha RED pelo desenho atual de autoarestas e pela geometria sempre ancorada na direita do emissor/esquerda do receptor.

### Task 4: Corrigir a construção e geometria das arestas

**Arquivos:** `apps/frontend/src/components/DeployNetworkCanvas.tsx`.

- [x] Derivar estágios únicos, ordenados por `start_layer` e `end_layer`, em uma só função usada pela filtragem e pelas arestas fallback.
- [x] Ignorar `from_node_id === to_node_id` ao criar fallback, pois não há hop de rede nem RTT de aresta.
- [x] Não criar fallback que atravesse uma etapa sem máquina quando a cobertura está incompleta.
- [x] Manter `from_node_id`, `to_node_id` e `rtt_ms` da opção de rota sem substituí-los por métrica do destino.
- [x] Escolher borda direita/esquerda conforme a posição relativa dos cards e calcular a curva e o rótulo RTT a partir dos mesmos endpoints.
- [x] Rodar os testes frontend focados e confirmar GREEN.

### Task 5: Cobrir a tela e atualizar documentação

**Arquivos:** `apps/frontend/src/routes/DeployModel.test.tsx`, documentação listada acima.

- [x] Adicionar regressões da tela para duas faixas separadas na mesma máquina, todas as faixas numa máquina, gap e sobreposição.
- [x] Atualizar as descrições para não dizer que uma máquina só pode servir uma faixa nem que gap é apenas aviso.
- [x] Rodar `npm test` (39 arquivos, 239 testes) e `npm run build` (typecheck e Vite); não há script de lint, e `cargo test` relevante permanece bloqueado pela compilação de `llama-cpp-sys-2`.
- [x] Fazer QA visual no navegador em viewport largo e estreito, incluindo mapa válido de três máquinas com múltiplos saltos, caso de uma máquina e gap; a seta/RTT ficou alinhada, o botão refletiu o plano e o mapa estreito rolou internamente sem overflow da página.
- [x] Revisar o diff, criar um único commit local na branch da tarefa e confirmar worktree limpo (após rebase local sobre `main`).
