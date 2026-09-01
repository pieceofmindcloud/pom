# src/i18n

Two-language UI localization (English and Brazilian Portuguese) for the pom
panel. All visible strings live here as flat message keys; components consume
them through `useI18n().t`. The same file also holds a module-level active
locale used by non-React code (`api.ts`, `chatStore.ts`) and a shim that keeps
the UI language contract while old node binaries still send Portuguese text.

Two files carry all of it: a static message table and a small provider. Tests:
`locales.test.ts` (resolution + formatting) and `provider.test.tsx` (jsdom).

## Per-file

- `locales.ts` - the `messages` dictionary (the exact key count is checked in
  `locales.test.ts`, en block first, pt-BR block second) plus locale resolution
  and formatting functions. The slice screen has messages for pipeline and
  tensor-parallel modes, including grouped `1ofN` rank labels. Deploy metadata
  refusals are rendered from the backend's selection-specific `blocked` reason;
  the whole-model single-machine warning and cache update failure are paired
  in both locales. Dashboard GPU temperature badges distinguish an unavailable
  sensor from a machine with no GPU, and the Network thermal policy panel has
  keys in both locale blocks. Deployment-wide hidden-state dtype controls and
  the Chat telemetry label for bytes/dtype are also present in both locale blocks.
  Slicing phase, byte-progress, disk-space, required intermediate-head cost and
  single-range controls also have keys in both locale blocks.
  Dashboard machine details also has paired labels for requested MTP status,
  its measured VRAM/RAM location and its runtime unavailability reason. The
  deployment form labels the opt-out for the RAM fallback in both locales.
  The Deployments section has separate `nav.history` and `deployments.created`
  labels in both locale blocks for the nested history menu and date column.
- O grupo `remoteModels.*` cobre status do CLI e da autenticação, busca,
  detalhes, licença desconhecida, confirmação, fila, progresso, cancelamento,
  retry e erros do provider nas duas línguas. Não coloque texto do CLI ou
  caminho local nesses valores.
- O grupo `huggingFace.*` cobre o campo de token da tela de Rede. O valor
  persistido nunca entra numa mensagem; a UI mostra somente a máscara e a
  fonte efetiva.
- O grupo `tokenLogs.*` cobre a auditoria de requisições: barra de filtros,
  abas de requisições e estatísticas, métricas agregadas, inspetor JSON e
  cópia segura. Nenhuma mensagem dessa tela recebe o segredo da chave.
- `provider.tsx` - `LocaleProvider`, the `useI18n()` hook and the `LanguageSelect` component.

## How locales are structured

- `SUPPORTED_LOCALES = ["en", "pt-BR"]`, `Locale` is that literal union.
- A `Message` is `(values, locale) => string`, not a template string: pluralization is computed with `Intl.PluralRules` (e.g. `cluster.peerCount` -> "1 node" / "2 nodes" / "1 nó" / "2 nós"). Values come in via `t("key", { count })`.
- `messages` is `Record<Locale, Record<string, Message>>` with both locales side by side; keys are namespaced by screen (`topbar.*`, `nav.*`, `chat.*`, `deploy.knob.*`, ...). The destructive-action group `undeploy.*` (button, confirmation, per-machine report) and the per-machine exit `dashboard.leaveNode*` arrived together with the `LeaveNode`/`UndeployPipeline` components, plus the shared `common.close` used by the `Modal` footer. The panel rework in 5423852 added `dashboard.groupDeployed*` / `dashboard.groupOutside*` (the two details groups), `dashboard.noSlice*` and `dashboard.machineDetails` (the card and its window), `canvas.openDetails` (the double-click hint on the canvas card), and changed `dashboard.noMachines` from "Nenhuma máquina para detalhar" to "Nenhuma máquina na rede" because the tab now lists the whole network. The navigation keeps the model entries together, and `sidebar.collapse` / `sidebar.expand` label the persistent desktop menu toggle; `jobs.*` labels the embedded slicing progress panel. `profile.*` nomeia os perfis (`profile.admin`, `profile.node`) e o seletor de desenvolvimento da barra superior, e `forbidden.*` é a tela que o perfil ativo não abre - `forbidden.detail` recebe o perfil já traduzido em `{profile}`. `deploy.knob.fit_memory_margin_mb` existe nos dois blocos de idioma porque é um ajuste numérico visível no formulário compartilhado. Os controles de detecção de ciclo e seus limites também existem nos dois blocos.

## Key functions

### locales.ts
- `formatMessage(locale, key, values)` - looks up `messages[locale]`, falls back to `messages.en`, then returns the raw key if absent. The fallback chain is what makes adding a key to one locale safe.
- `resolveLocale(savedLocale, browserLocales)` - the saved value wins if valid; otherwise any `pt*` browser locale maps to `pt-BR`, any `en*` to `en`, default `en`. Saves nothing; the provider does.
- `getActiveLocale()` / `setActiveLocale(locale)` - module-level locale for code outside React (error strings in `api.ts`, default chat titles in `chatStore.ts`); the provider syncs it on mount.
- `isLocale(value)` - type guard.
- `localizeServerText(locale, text)` - maps known Portuguese strings a rolling-upgrade old binary might send ("pronto", "erro", "falhou", ...) to the UI language; heuristic regex detects untranslated Portuguese and falls back to a generic message rather than showing mixed language. Antes do mapa fixo ele casa as frases que **carregam um nome de máquina** ("recusado por ksia", "cancelado por atena"): o nome não se traduz, o verbo sim, e o desfecho de uma transferência aparecia em português na tela em inglês.

The chat telemetry additions live in both locale blocks: execution RTT, the
source-to-destination edge scope, machine capacity/configuration, effective
placement, per-peer `POM_PROFILE_SYNC`, backend-reported execution engine,
execution diagnosis, TP collective table, prefix reuse and transmitted-token
counts, overlap status, per-traversal transport identity/bytes, AI-analysis
action and initial-model mark must remain available in
English and Brazilian Portuguese. Generic configuration state labels for
fit-memory and KV offload are separate from the per-peer `POM_PROFILE_SYNC`
state labels. The footnote explains the measurements but does not claim a fixed
environment value.

Os rótulos de tempo por requisição também existem nos dois blocos: TTFT,
velocidade de pré-processamento do prompt, velocidade de geração da saída,
tempo total, tokens/s e o rótulo explícito de taxa não mensurável usado para
prefill de zero token ou resposta de um token.

O Chat também traduz o rótulo de erro vindo dentro do stream, o controle para
copiar a mensagem e o estado de cópia. O texto do servidor é preservado sem
passar pelo fallback de idioma, para que o diagnóstico real não seja perdido.

The slice locale group also covers multipart part lists, summed-size loading
state and explicit incomplete-set errors in both languages.

### provider.tsx
- `LocaleProvider({ children, initialLocale })` - initial state from `initialLocale` or from `localStorage` `codai.locale` + `navigator.languages`; writes `<html lang>` on change; persists the choice.
- `useI18n()` - `{ locale, setLocale, t }`.
- `LanguageSelect({ className })` - the en/pt-BR `<select>`.

## Links to the rest of the app

- Imported by nearly every route and component (28 files use `useI18n`).
- `api.ts` and `chatStore.ts` import `getActiveLocale` from `locales` for non-React strings.
- `index.html` has a pre-paint script that stamps `document.documentElement.lang` from the same `codai.locale` + browser-locale logic, so the first render is already in the right language before `LocaleProvider` mounts.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
