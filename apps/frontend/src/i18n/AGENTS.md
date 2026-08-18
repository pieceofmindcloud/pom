# src/i18n

Two-language UI localization (English and Brazilian Portuguese) for the pom
panel. All visible strings live here as flat message keys; components consume
them through `useI18n().t`. The same file also holds a module-level active
locale used by non-React code (`api.ts`, `chatStore.ts`) and a shim that keeps
the UI language contract while old node binaries still send Portuguese text.

Two files carry all of it: a static message table and a small provider. Tests:
`locales.test.ts` (resolution + formatting) and `provider.test.tsx` (jsdom).

## Per-file

- `locales.ts` - the `messages` dictionary (en block lines ~20-671, pt-BR block lines ~672-1363, 628 keys each) plus locale resolution and formatting functions.
- `provider.tsx` - `LocaleProvider`, the `useI18n()` hook and the `LanguageSelect` component.

## How locales are structured

- `SUPPORTED_LOCALES = ["en", "pt-BR"]`, `Locale` is that literal union.
- A `Message` is `(values, locale) => string`, not a template string: pluralization is computed with `Intl.PluralRules` (e.g. `cluster.peerCount` -> "1 node" / "2 nodes" / "1 nó" / "2 nós"). Values come in via `t("key", { count })`.
- `messages` is `Record<Locale, Record<string, Message>>` with both locales side by side; keys are namespaced by screen (`topbar.*`, `nav.*`, `chat.*`, `deploy.knob.*`, ...). The destructive-action group `undeploy.*` (button, confirmation, per-machine report) and the per-machine exit `dashboard.leaveNode*` arrived together with the `LeaveNode`/`UndeployPipeline` components, plus the shared `common.close` used by the `Modal` footer. The panel rework in 5423852 added `dashboard.groupDeployed*` / `dashboard.groupOutside*` (the two details groups), `dashboard.noSlice*` and `dashboard.machineDetails` (the card and its window), `canvas.openDetails` (the double-click hint on the canvas card), and changed `dashboard.noMachines` from "Nenhuma máquina para detalhar" to "Nenhuma máquina na rede" because the tab now lists the whole network.

## Key functions

### locales.ts
- `formatMessage(locale, key, values)` - looks up `messages[locale]`, falls back to `messages.en`, then returns the raw key if absent. The fallback chain is what makes adding a key to one locale safe.
- `resolveLocale(savedLocale, browserLocales)` - the saved value wins if valid; otherwise any `pt*` browser locale maps to `pt-BR`, any `en*` to `en`, default `en`. Saves nothing; the provider does.
- `getActiveLocale()` / `setActiveLocale(locale)` - module-level locale for code outside React (error strings in `api.ts`, default chat titles in `chatStore.ts`); the provider syncs it on mount.
- `isLocale(value)` - type guard.
- `localizeServerText(locale, text)` - maps known Portuguese strings a rolling-upgrade old binary might send ("pronto", "erro", "falhou", ...) to the UI language; heuristic regex detects untranslated Portuguese and falls back to a generic message rather than showing mixed language.

The chat telemetry additions live in both locale blocks: the execution
diagnosis, TP collective table, AI-analysis action and initial-model mark must
remain available in English and Brazilian Portuguese.

### provider.tsx
- `LocaleProvider({ children, initialLocale })` - initial state from `initialLocale` or from `localStorage` `codai.locale` + `navigator.languages`; writes `<html lang>` on change; persists the choice.
- `useI18n()` - `{ locale, setLocale, t }`.
- `LanguageSelect({ className })` - the en/pt-BR `<select>`.

## Links to the rest of the app

- Imported by nearly every route and component (28 files use `useI18n`).
- `api.ts` and `chatStore.ts` import `getActiveLocale` from `locales` for non-React strings.
- `index.html` has a pre-paint script that stamps `document.documentElement.lang` from the same `codai.locale` + browser-locale logic, so the first render is already in the right language before `LocaleProvider` mounts.
