# Esquecer rede nas informações da rede - Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Expor na tela Informações da rede uma ação local chamada “Esquecer rede”, separada de “Sair da rede”, com confirmação destrutiva e chamada à rota existente de reset.

**Architecture:** `NetworkSettings` mantém as duas ações em áreas distintas da tela: o botão existente continua chamando `/api/ui/leave`, enquanto o novo botão fica no painel de informações e chama `/api/ui/network/leave`. A confirmação reutiliza `Confirm`; depois da resposta de reset, a UI segue o fluxo existente de redirecionar para `/setup` enquanto o backend reinicia o nó.

**Tech Stack:** React 18, TypeScript, Vitest, Testing Library, CSV i18n, Tailwind utility classes.

**Spec:** Decisão do captain registrada na solicitação corrente.

## Global Constraints

- Não alterar o comportamento ou a rota de “Sair da rede” (`POST /api/ui/leave`).
- O texto deve distinguir saída remota do cluster de limpeza local com reinício.
- Toda mensagem visível nova deve existir em inglês e pt-BR em `apps/frontend/src/i18n/messages.csv`.
- O botão de reset deve pedir confirmação antes de chamar a API.
- Validar com o teste da rota, o build obrigatório da UI e inspeção visual no Chrome.

---

### Task 1: Cobrir a nova ação com teste regressivo

**Files:**
- Modify: `apps/frontend/src/routes/NetworkSettings.test.tsx`
- Read: `apps/frontend/src/routes/NetworkSettings.tsx`

**Interfaces:**
- Consumes: `NetworkSettings`, `api.post` mockado e `LocaleProvider` pt-BR.
- Produces: uma especificação executável que exige os dois botões com significados distintos, confirmação antes do reset e chamada a `/api/ui/network/leave` somente após confirmar.

- [x] **Step 1: Write the failing test**

Adicionar um teste em `describe("NetworkSettings")` que renderize a tela em pt-BR, confirme que existem os botões “Sair da rede” e “Esquecer rede”, clique somente em “Esquecer rede”, verifique o título/descrição destrutivos da confirmação e confirme que `api.post` ainda não foi chamado. Depois clicar no botão de confirmação “Esquecer rede” e esperar `api.post` ser chamado com `/api/ui/network/leave`, sem exigir chamada a `/api/ui/leave`.

- [x] **Step 2: Run test to verify it fails**

Run: `cd apps/frontend && npm exec -- vitest run src/routes/NetworkSettings.test.tsx`

Expected: FAIL porque a tela ainda não renderiza “Esquecer rede” nem abre sua confirmação.

### Task 2: Implementar a ação local na tela correta

**Files:**
- Modify: `apps/frontend/src/routes/NetworkSettings.tsx`
- Modify: `apps/frontend/src/i18n/messages.csv`

**Interfaces:**
- Consumes: `Confirm`, `api.post`, `useToast`, `window.location.assign` e o estado existente de `NetworkSettings`.
- Produces: botão localizado em `NetworkInfoPanel`, confirmação destrutiva, chamada de `/api/ui/network/leave` e redirecionamento a `/setup` após o backend aceitar o reset.

- [x] **Step 1: Add the localized copy**

Adicionar chaves `network.forgetNetwork`, `network.forgetNetworkTitle`, `network.forgetNetworkDescription`, `network.forgetNetworkFailed`, `network.forgetNetworkStarted` e `network.forgetNetworkStartedDetail` nas duas colunas do CSV, usando “Esquecer rede” e explicando que a ação apaga a configuração/raiz de confiança local, reinicia o nó e abre a configuração inicial novamente. Manter as chaves atuais de `network.leave` sem alteração.

- [x] **Step 2: Add the failing-to-green implementation**

Criar estado separado `forgetNetworkConfirm` e `forgettingNetwork` em `NetworkSettings`. A função de confirmação deve chamar `api.post("/api/ui/network/leave")`, mostrar o toast de início, fechar a confirmação e redirecionar para `/setup` com o mesmo atraso de 1500 ms usado pelo fluxo histórico. Em caso de erro, usar `toast.fromError` com a nova mensagem e não redirecionar.

Passar a `NetworkInfoPanel` uma callback `onForgetNetwork` e um estado `forgetting`. No `PanelHeader` desse painel, renderizar “Esquecer rede” como botão `danger` ao lado do botão de salvar ou do badge de leitura. O `Confirm` correspondente deve ficar em `NetworkSettings` e usar a descrição que explicita perda local irreversível e reinício. Não tocar em `toggleMembership` nem no botão “Sair da rede” do painel da máquina.

- [x] **Step 3: Run the focused test to verify it passes**

Run: `cd apps/frontend && npm exec -- vitest run src/routes/NetworkSettings.test.tsx`

Expected: PASS, com a confirmação impedindo o POST antes da decisão e o POST indo exclusivamente para `/api/ui/network/leave` depois da confirmação.

### Task 3: Validar contrato, layout e entrega local

**Files:**
- Modify: `apps/frontend/src/routes/AGENTS.md` only if the route behavior summary is stale.
- Modify: `apps/frontend/src/i18n/AGENTS.md` only if its message-group documentation needs a durable update.

**Interfaces:**
- Consumes: o teste regressivo, o build da UI e a aplicação local em `chrome-devtools-axi`.
- Produces: UI compilada e verificada, branch limpa com commit local.

- [x] **Step 1: Run the complete relevant validation**

Run: `cd apps/frontend && npm test`

Run: `cd apps/frontend && npm run build`

Expected: ambos terminam com código 0, sem falhas de teste ou TypeScript/Vite.

- [x] **Step 2: Validate the visual distinction**

Abrir `/admin-ui/config/rede` com `chrome-devtools-axi`, conferir que “Sair da rede” continua no painel “Esta máquina” e “Esquecer rede” aparece no painel “Informações da rede”. Abrir a confirmação e verificar que a descrição fala de limpeza local/reinício, sem executar a ação destrutiva.

- [x] **Step 3: Inspect, rebase and commit**

Run: `git diff --check`

Run: `git status --short --branch`

Run: `git rebase origin/feat/implantar3`

Run: `git add apps/frontend/src/routes/NetworkSettings.tsx apps/frontend/src/routes/NetworkSettings.test.tsx apps/frontend/src/i18n/messages.csv docs/superpowers/plans/2026-09-12-esquecer-rede-na-informacoes.md`

Run: `git commit -m "feat(ui): reexpose local network reset"`

Expected: somente os arquivos da mudança ficam no commit, sem push remoto.
