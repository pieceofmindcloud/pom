# Wizard de primeiro acesso do POM - design

## Contexto

O contrato é `/Users/jairpereira/Sites/firstmate/data/rdma-wizard-primeiro-acesso/especificacao-do-captain.md`. Este trabalho cria somente um protótipo navegável dentro da SPA existente. O bootstrap real de `Setup.tsx`, usado quando o backend ainda não tem `pom.json`, permanece intacto.

## Direção escolhida

O wizard será uma rota `/setup` no shell já configurado do painel. `App.tsx` continuará escolhendo a tela de bootstrap antes de montar o shell; quando `/api/ui/node` indicar um nó disponível, `/setup` renderá o novo wizard. A entrada também será adicionada a Configurações para tornar a revisão posterior descobrível.

A experiência usará um estado único `PomSetup` em React state, sem biblioteca nova. A navegação mantém uma pilha de telas, de forma que cada avanço tenha um retorno simétrico e que mudar de fluxo limpe o histórico. Os dados simulados vivem em um módulo próprio, com nomes e formatos próximos das futuras respostas de API.

## Estrutura de componentes

- `routes/SetupWizard.tsx`: coordena o estado, o histórico, a escolha do fluxo e a composição da tela atual.
- `routes/setupWizardTypes.ts`: tipos de intenção, roles, modos, etapas e contrato de estado.
- `routes/setupWizardMocks.ts`: hardware, computadores, modelos, redes públicas, capabilities, diagnóstico e resultado de inferência.
- `components/setup-wizard/WizardLayout.tsx`: cabeçalho, progresso, área de conteúdo e ações fixas.
- `components/setup-wizard/WizardCards.tsx`: cartões de objetivo, hardware, máquina, modelo, rede e capability.
- `components/setup-wizard/WizardScreens.tsx`: telas de hardware, rede, descoberta, modelo, pipeline, convite, segurança, compartilhamento, redes públicas, diagnóstico, recursos e resumo.

As telas recebem dados e callbacks, não conhecem a implementação de navegação. A cópia passa pelo `useI18n` existente em inglês e pt-BR. A aparência usa os tokens do `index.css`, `Panel`, `Button`, `Field`, `Input`, `Select`, `Badge`, `Progress` e `Footnote` já existentes.

## Fluxos

- Quick Start: hardware -> rede local -> modelo -> pipeline -> diagnóstico -> resumo.
- Minhas máquinas: hardware -> rede -> descoberta LAN -> modelo -> pipeline -> diagnóstico -> resumo.
- Empresa: organização -> segurança -> convite/equipe -> hardware -> modelo -> API -> diagnóstico -> resumo.
- Contribuir: hardware e recursos -> escolha de rede -> redes públicas quando necessário -> resumo.
- Usar uma rede: entrada por convite/rede -> conexão -> recursos por capability -> resumo.
- Avançado: roles múltiplas -> rede/discovery/backend/modelos/segurança/API -> resumo técnico.

As transições de operação são locais e curtas, com estados `searching`, `connecting`, `downloading` e diagnóstico revelados por ação do usuário ou por um atraso pequeno. Não há chamada a backend, mDNS, QR real, download ou autenticação nova.

## Direção visual

A composição é uma faixa de trabalho centrada, com o passo atual em primeiro plano e um rail de progresso discreto no topo. O elemento memorável é o mapa de inferência: cartões de máquinas conectados por uma linha iris/aqua, usado para explicar a preparação da rede sem expor layers ou RPC. O restante é deliberadamente quieto: superfícies POM, bordas finas, cantos já usados pelo produto, texto alinhado à esquerda e grids que viram uma coluna no mobile.

O tema claro/escuro vem integralmente dos tokens globais. Foco visível, `aria-current`, `aria-pressed`, labels explícitos e respeito a `prefers-reduced-motion` seguem os padrões existentes.

## Validação

O teste de rota verifica que `/setup` carrega o wizard no shell configurado. O teste da tela percorre o caminho rápido e os seis intentos, cobrindo que cada um chega a resumo, que Voltar remove a última tela, que o resumo expõe roles e dados escolhidos e que capabilities alteram os recursos mostrados. A validação manual no Chrome cobre desktop/mobile e os dois temas, além de Logs, Chat e Preferências.

## Fora de escopo

Não haverá alteração de protocolo P2P, inferência, APIs existentes, llama.cpp, autenticação definitiva, descoberta real, download real, reputação, pagamento, créditos ou state management externo.
