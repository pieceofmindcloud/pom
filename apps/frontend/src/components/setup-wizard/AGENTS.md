# components/setup-wizard/

Componentes reutilizáveis do protótipo do wizard de primeiro acesso. `WizardLayout.tsx` mantém o header, progresso, conteúdo e ações; `WizardCards.tsx` concentra cartões de objetivo, hardware, máquinas, modelos, redes e capabilities. As telas e o estado ficam em `../routes/SetupWizard.tsx`; os dados simulados ficam em `../routes/setupWizardMocks.ts`.

O conjunto é somente apresentação: não chama backend, não cria permissões e não implementa descoberta ou download reais. Essa pendência de produto está em `TODO.md`, item 22. Use os tokens de `../../index.css` e os primitives de `../ui.tsx`; qualquer texto novo deve passar pelo i18n existente.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this folder. Prefer pointers to the source files over duplicating implementation detail.
