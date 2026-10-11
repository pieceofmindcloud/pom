# components/setup-wizard/

Reusable components of the first access wizard. `WizardLayout.tsx` holds the header, progress, content and actions; `WizardCards.tsx` gathers the goal, hardware, machines, models, networks and capabilities cards. The screens and state are simulated in `../routes/SetupWizard.tsx`; the real flow lives in `../routes/Setup.tsx` and uses the same layout with `/api/setup/*`.

`SetupWizard.tsx` remains presentation only: it calls no backend, creates no permissions and implements no real discovery or download. `Setup.tsx` is the exception and must stay one screen per question, without reusing mocks as state. Use the tokens from `../../index.css` and the primitives from `../ui.tsx`; any new text must go through the existing i18n.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this folder. Prefer pointers to the source files over duplicating implementation detail.