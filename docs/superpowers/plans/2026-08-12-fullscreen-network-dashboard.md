# Fullscreen Network Dashboard Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Tornar o Fluxo um canvas de rede sem margens, reordenável pelo operador e alternável com os detalhes completos das máquinas.

**Architecture:** `App` entrega uma área sem padding apenas à rota raiz. `Dashboard` controla os dois modos e a faixa de comando sobre o canvas. `ModelNetworkCanvas` recebe os dados reais, cria posições determinísticas, persiste a preferência local por identidade de deployment e desenha cards e arestas na mesma superfície de coordenadas.

**Tech Stack:** React, TypeScript, Tailwind, Framer Motion, Vitest.

## Global Constraints

- Posições são locais ao navegador e são apagadas quando a identidade de deployment muda.
- Canvas não muda a rota selecionada pelo roteador nem a configuração dos peers.
- Fluxo não tem card externo, margens externas ou rolagem da página.
- Detalhes mostra apenas cards completos e pode rolar normalmente.

---

### Task 1: Layout local de nós

**Files:**
- Create: `apps/frontend/src/lib/networkLayout.ts`
- Create: `apps/frontend/src/lib/networkLayout.test.ts`

- [x] Escrever e executar teste vermelho para `layoutKey(cluster, model)`, que muda quando muda `active_model` ou a lista ordenada de `node_id`.
- [x] Implementar chave de deployment, leitura, gravação e remoção de posições no `localStorage`.
- [x] Escrever e executar teste vermelho para `fallbackPosition(index)`, garantindo posição visível e estável para cada card.
- [x] Implementar o posicionamento padrão determinístico.

### Task 2: Canvas de fluxo reordenável

**Files:**
- Modify: `apps/frontend/src/components/ModelNetworkCanvas.tsx`
- Modify: `apps/frontend/src/components/MachineIcon.tsx`

- [x] Escrever o teste vermelho de persistência de arrasto contra `networkLayout`.
- [x] Trocar o card genérico por card de nó com hostname, `MachineIcon`, `Dot`, estado, faixa e gauges.
- [x] Remover o nome repetido do modelo do card e usar somente faixa de layers.
- [x] Implementar arrasto por Pointer Events, com posição limitada ao canvas e persistida pela chave do deployment.
- [x] Desenhar arestas com as posições correntes dos cards, e controles flutuantes de modelo, zoom, fit e reset.

### Task 3: Dashboard de tela cheia e alternância

**Files:**
- Modify: `apps/frontend/src/App.tsx`
- Modify: `apps/frontend/src/routes/Dashboard.tsx`

- [x] Fazer a rota raiz ocupar a altura disponível sem container, margem ou rolagem no modo Fluxo.
- [x] Colocar métricas, gauge e Fluxo/Detalhes em barra sobreposta acima do canvas.
- [x] Em Detalhes, desmontar o canvas e renderizar somente os cartões completos.
- [x] Deixar `PipelineRail` preservado no código, porém invisível na tela de Fluxo.

### Task 4: Verificação

**Files:**
- Modify: `docs/superpowers/plans/2026-08-12-fullscreen-network-dashboard.md`

- [x] Executar `npm --prefix apps/frontend run test` e `npm --prefix apps/frontend run build`.
- [x] Compilar `cargo build --release --bin rdma-p2p`.
- [x] Rodar localmente em 8099 e inspecionar Fluxo, Detalhes, tamanho da superfície e controles pelo navegador.
