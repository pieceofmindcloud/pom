# components/setup-wizard/

Componentes reutilizáveis do wizard de primeiro acesso. `WizardLayout.tsx` mantém o header, progresso, conteúdo e ações; `WizardCards.tsx` concentra cartões de objetivo, hardware, máquinas, modelos, redes e capabilities. As telas e o estado simulados ficam em `../routes/SetupWizard.tsx`; o fluxo real fica em `../routes/Setup.tsx` e usa o mesmo layout com `/api/setup/*`.

`SetupWizard.tsx` continua sendo somente apresentação: não chama backend, não cria permissões e não implementa descoberta ou download reais. `Setup.tsx` é a exceção e deve continuar com uma tela por pergunta, sem reaproveitar mocks como estado. Use os tokens de `../../index.css` e os primitives de `../ui.tsx`; qualquer texto novo deve passar pelo i18n existente.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this folder. Prefer pointers to the source files over duplicating implementation detail.
