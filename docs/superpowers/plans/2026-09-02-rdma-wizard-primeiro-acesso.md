# Wizard de primeiro acesso do POM Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Entregar um protótipo POM de alta fidelidade, navegável e responsivo para os seis caminhos do wizard de primeiro acesso.

**Architecture:** Uma rota `/setup` no shell configurado mantém um estado `PomSetup` central e uma pilha de telas para navegação reversível. Mocks tipados ficam fora dos componentes; componentes de layout, cartões e telas recebem dados/callbacks e são reutilizados entre os fluxos.

**Tech Stack:** React 18, TypeScript, React Router 6, Tailwind 3, Framer Motion, lucide-react, Vitest e Testing Library já presentes no frontend.

**Spec:** `docs/superpowers/specs/2026-09-02-rdma-wizard-primeiro-acesso-design.md` e o contrato do captain em `/Users/jairpereira/Sites/firstmate/data/rdma-wizard-primeiro-acesso/especificacao-do-captain.md`.

## Global Constraints

- Preservar o bootstrap real de `routes/Setup.tsx` quando o nó estiver sem `pom.json`.
- Usar `/setup` dentro do `BrowserRouter` existente com basename `/admin-ui`.
- Não adicionar biblioteca de estado nem alterar APIs, protocolo P2P, inferência ou llama.cpp.
- Centralizar mocks em `routes/setupWizardMocks.ts` e modelar roles como flags não exclusivas.
- Usar os tokens e componentes POM existentes, suportar os dois temas, mobile e foco de teclado.
- Todo texto de interface passa pelo `useI18n` existente e mantém paridade en/pt-BR.
- Atualizar `AGENTS.md` e o README de subsistema afetado no mesmo commit.
- Rodar `npm run build`, os testes do frontend e validação manual antes de declarar conclusão.

---

### Task 1: Modelos e mocks do wizard

**Files:**
- Create: `apps/frontend/src/routes/setupWizardTypes.ts`
- Create: `apps/frontend/src/routes/setupWizardMocks.ts`
- Test: `apps/frontend/src/routes/SetupWizard.test.tsx`

**Interfaces:**
- Produces `PomSetup`, `WizardIntent`, `WizardStage`, `WizardStep`, `WizardNode`, `WizardModel`, `WizardNetwork`, `WizardCapability` e `WizardHealthItem`.
- Produces `INITIAL_SETUP`, `MOCK_HARDWARE`, `MOCK_NODES`, `MOCK_MODELS`, `MOCK_PUBLIC_NETWORKS`, `MOCK_CAPABILITIES`, `MOCK_HEALTH` e `MOCK_TEST_INFERENCE`.

**Steps:**

- [ ] Escrever o teste que importa os mocks e confirma hardware, dois computadores LAN, três modelos, três redes públicas, roles múltiplas e capabilities distintas.
- [ ] Rodar `npm test -- --run src/routes/SetupWizard.test.tsx` e confirmar falha porque os módulos ainda não existem.
- [ ] Criar os tipos discriminados e os objetos mock mínimos, mantendo strings de apresentação fora dos mocks para o i18n.
- [ ] Rodar o mesmo teste e confirmar que passa.
- [ ] Revisar que nenhum componente conhece endpoint real ou biblioteca de estado.

### Task 2: Layout e cartões reutilizáveis

**Files:**
- Create: `apps/frontend/src/components/setup-wizard/WizardLayout.tsx`
- Create: `apps/frontend/src/components/setup-wizard/WizardCards.tsx`
- Create: `apps/frontend/src/components/setup-wizard/AGENTS.md`
- Test: `apps/frontend/src/routes/SetupWizard.test.tsx`

**Interfaces:**
- `WizardLayout` recebe `stage`, `steps`, `onBack`, `onExit`, `children` e `footer`.
- `IntentCard`, `HardwareCard`, `NodeCard`, `ModelCard`, `NetworkCard` e `CapabilityList` recebem dados mock e callbacks.

**Steps:**

- [ ] Acrescentar ao teste um render do layout com `initialLocale="pt-BR"` e asserções para progresso, botão Voltar, regiões acessíveis e estados selecionado/conectado.
- [ ] Rodar o teste e confirmar falha pela ausência dos componentes.
- [ ] Implementar o layout com o header POM, rail de progresso, área com largura limitada, footer de ações e estados sem overflow horizontal.
- [ ] Implementar cartões com `aria-pressed`, badges semânticos, ícones lucide e grids responsivos, reutilizando os primitives de `components/ui.tsx`.
- [ ] Rodar o teste e confirmar o comportamento.

### Task 3: Orquestrador e fluxo rápido

**Files:**
- Create: `apps/frontend/src/routes/SetupWizard.tsx`
- Modify: `apps/frontend/src/App.tsx`
- Modify: `apps/frontend/src/components/nav.ts`
- Test: `apps/frontend/src/routes/SetupWizard.test.tsx`
- Test: `apps/frontend/src/App.test.tsx`

**Interfaces:**
- `SetupWizard` compõe as telas e atualiza `PomSetup` com `updateSetup(patch)`.
- `startIntent(intent)` inicializa roles e primeira etapa; `goNext()` empilha a próxima tela; `goBack()` remove a etapa atual.

**Steps:**

- [ ] Escrever teste de `/setup` no `App` configurado, usando mock de `routeLoaders.setupWizard`, e teste de navegação pelo Quick Start até o resumo.
- [ ] Rodar `npm test -- --run src/routes/SetupWizard.test.tsx src/App.test.tsx` e observar falhas esperadas.
- [ ] Registrar o lazy loader e a rota `/setup` sem alterar a branch de bootstrap `adminSurface.data === "setup"`; marcar `/setup` como full bleed e adicionar o link de Configurações.
- [ ] Implementar Welcome, escolha de objetivo e as telas Quick Start/pessoal até hardware, rede local, modelo, pipeline, diagnóstico e resumo.
- [ ] Implementar histórico de etapas, Voltar, sair para `/`, transições discretas e progresso derivado do fluxo ativo.
- [ ] Rodar os testes e corrigir apenas problemas introduzidos pelo wizard.

### Task 4: Os outros cinco caminhos

**Files:**
- Modify: `apps/frontend/src/routes/SetupWizard.tsx`
- Modify: `apps/frontend/src/routes/setupWizardTypes.ts`
- Modify: `apps/frontend/src/components/setup-wizard/WizardCards.tsx`
- Modify: `apps/frontend/src/components/setup-wizard/WizardLayout.tsx`
- Test: `apps/frontend/src/routes/SetupWizard.test.tsx`

**Interfaces:**
- Cada intent expõe uma sequência `WizardStep[]` determinística e termina em `summary` ou `complete`.
- `selectCapability`, `selectNode`, `selectModel`, `selectNetwork` e `setResource` atualizam apenas o estado central.

**Steps:**

- [ ] Escrever testes que iniciam cada um dos cinco intentos restantes, clicam Continue em todas as etapas e confirmam um heading de resumo ou sucesso.
- [ ] Rodar o teste para confirmar que cada caminho ainda falha antes da implementação.
- [ ] Implementar empresa com organização, segurança, convite MOCK/QR visual, hardware, modelo, usuários, API, health check e resumo.
- [ ] Implementar contribuição com controles de GPU/RAM/CPU, disponibilidade, horário, código/rede pública/endereço e browser de redes mock.
- [ ] Implementar entrada em rede com código, QR, redes disponíveis, endereço manual, conexão e recursos condicionados pelas capabilities mock.
- [ ] Implementar configuração avançada com roles múltiplas, modo de rede, discovery, backend, hardware, modelos, segurança e API.
- [ ] Implementar componentes de pipeline, health check e resumo YAML visual para serem reutilizados nos fluxos.
- [ ] Rodar os testes e confirmar que os seis caminhos chegam ao fim.

### Task 5: Traduções e documentação do subsistema

**Files:**
- Modify: `apps/frontend/src/i18n/locales.ts`
- Modify: `apps/frontend/src/routes/AGENTS.md`
- Modify: `apps/frontend/src/components/AGENTS.md`
- Modify: `docs/subsystems/web-ui.md`
- Test: `apps/frontend/src/i18n/locales.test.ts`

**Steps:**

- [ ] Acrescentar as chaves do wizard em inglês e pt-BR e rodar o teste de paridade de locales para capturar qualquer chave ausente.
- [ ] Documentar a rota configurada `/setup`, a separação em relação ao bootstrap `Setup.tsx`, o estado mock central e os componentes criados.
- [ ] Atualizar o subsistema web-ui em pt-BR com o fluxo navegável e o fato de que nenhuma operação real é executada.
- [ ] Rodar `npm test -- --run src/i18n/locales.test.ts` e confirmar paridade.

### Task 6: Verificação visual e regressão

**Files:**
- Modify: `apps/frontend/src/routes/SetupWizard.tsx` (somente ajustes encontrados na validação)
- Modify: `apps/frontend/src/components/setup-wizard/*` (somente ajustes encontrados na validação)

**Steps:**

- [ ] Rodar `npm run build` no frontend e confirmar typecheck e bundle.
- [ ] Subir o dev server, abrir `/admin-ui/setup` no Chrome e testar Welcome, todos os intentos, Voltar, resumo, menu, Chat, Logs e Preferências.
- [ ] Capturar e inspecionar desktop largo, mobile estreito, tema claro e tema escuro; corrigir overflow, foco, contraste ou desalinhamento observável.
- [ ] Rodar `npm test` completo do frontend e contar falhas novas, mantendo separadas as dez falhas Rust conhecidas do brief.
- [ ] Conferir `git diff`, `git status` e os arquivos gerados ignorados antes do commit.
- [ ] Atualizar o relatório externo `data/rdma-wizard-primeiro-acesso/report.md` com os oito pontos pedidos, arquivos, rotas e screenshots/descrições.
- [ ] Fazer commit e, conforme o gate de publicação do launch-brief, rebasear em `origin/feat/implantar` antes de publicar sem force push.
