# src/i18n

Two-language UI localization (English and Brazilian Portuguese) for the pom
panel. All visible strings live here as flat message keys; components consume
them through `useI18n().t`. The same file also holds a module-level active
locale used by non-React code (`api.ts`, `chatStore.ts`) and a shim that keeps
the UI language contract while old node binaries still send Portuguese text.

As traduções ficam em `messages.csv`, com as colunas `key,en,pt-BR`, uma linha
por chave e as mensagens em ICU MessageFormat. `locales.ts` importa o CSV como
texto bruto em build time e interpreta as mensagens sincronicamente com
`intl-messageformat` 11.2.14. O provider continua separado. Testes:
`locales.test.ts` (resolução + formatação), `locales.equivalence.test.ts`
(dump de referência byte a byte) e `provider.test.tsx` (jsdom).

## Per-file

- `messages.csv` - fonte única das mensagens, ordenada por chave para diffs
  estáveis. Textos simples permanecem literais; interpolação usa `{name}` e
  lógica usa `plural`, `select` e seletores exatos ICU. Valores internos de
  `select` são preparados apenas pelo carregador para preservar condicionais
  de truthiness que existiam no código antigo.
- O grupo `adminAuthority.*` cobre a tela do principal e as vistas somente
  leitura de admins secundários e nós: roster, cadeia de promoções, sucessor,
  recusas concretas, confirmações e resultado das ações. O grupo
  `notifications.*` também nomeia ofertas de sucessão e de promoção
  infer-to-node, além do aviso sem botões de descida para infer.
- `locales.ts` - parser CSV pequeno, carregamento síncrono do `?raw`, cache de
  `IntlMessageFormat` e a API pública de locale. Não coloque traduções novas
  aqui. O fallback continua sendo locale solicitado, depois inglês, depois a
  própria chave. Erros de formatação ICU são avisados no console e devolvem o
  padrão bruto, para que uma variável omitida não derrube a tela. Todas as
  chaves e textos existentes, incluindo os grupos de logs de deployment,
  preferências, telemetria e modelos remotos, ficam no CSV.
- O grupo `trustAnchor.*` cobre a tela da raiz de confiança: estado ancorado e
  por ancorar, a impressão digital e sua cópia, o formulário de instalação e as
  cinco recusas do servidor por código (`trustAnchor.refusal.*`). A tela mostra
  a recusa traduzida e só cai no texto do servidor para um código que ela não
  conhece.
- O grupo `remoteModels.*` cobre status do CLI e da autenticação, busca,
  detalhes, licença desconhecida, confirmação, fila, progresso, cancelamento,
  retry e erros do provider nas duas línguas. Não coloque texto do CLI ou
  caminho local nesses valores.
- O grupo `huggingFace.*` cobre o campo de token da tela de Preferências. O valor
  persistido nunca entra numa mensagem; a UI mostra somente a máscara e a
  fonte efetiva.
- O grupo `tokenLogs.*` cobre a auditoria de requisições: barra de filtros,
  abas de requisições e estatísticas, métricas agregadas, abas ENVIADO,
  REQUEST, Resposta e TELEMETRIA, visualizadores Markdown, toggle persistente
  de JSON cru ou Markdown renderizado, caminho da requisição e cópia segura da
  aba ativa. Também nomeia os botões de expansão e a janela grande de leitura.
  O aviso
  sobre JSON selecionável foi removido; nenhuma mensagem dessa tela recebe o
  segredo da chave.
- O grupo `deploymentLogs.*` cobre o console de processo: estado do auto-scroll,
  cópia das últimas 500 linhas, retorno da cópia, filtro e estados de coleta.
- `provider.tsx` - `LocaleProvider`, the `useI18n()` hook and the `LanguageSelect` component.

Os identificadores oficiais de perfil são `admin`, `node` e `infer`. O rótulo
visível do primeiro passa por `profile.admin`; os dois últimos permanecem
minúsculos por decisão de produto. A toolbar, a tela de rede e o bootstrap
usam essas chaves, nunca literais cravados nos componentes.

## Como editar mensagens

- `SUPPORTED_LOCALES = ["en", "pt-BR"]`, e `Locale` continua sendo a união literal.
- Cada chave deve existir nas duas colunas. Ao adicionar ou editar uma mensagem, altere diretamente a mesma linha no CSV e mantenha a chave ordenada.
- Use a sintaxe ICU de `intl-messageformat`: `{count, plural, one {# node} other {# nodes}}` para plural, `{value}` para interpolação e `select` para escolhas textuais. Para mensagens condicionais, preserve os nomes dos valores que os componentes já passam.
- O CSV entra no bundle por importação `?raw`; não use fetch, estado assíncrono ou carregamento por rota.
- Rode `npm exec -- vitest run src/i18n/locales.test.ts src/i18n/locales.equivalence.test.ts` após qualquer alteração. O teste de equivalência compara todas as chaves, os dois idiomas e os casos do dump gerado antes da migração.

## Key functions

### locales.ts
- `formatMessage(locale, key, values)` - looks up `messages[locale]`, falls back to `messages.en`, then returns the raw key if absent. The fallback chain is what makes adding a key to one locale safe.
- `resolveLocale(savedLocale, browserLocales)` - the saved value wins if valid; otherwise any `pt*` browser locale maps to `pt-BR`, any `en*` to `en`, default `en`. Saves nothing; the provider does.
- `getActiveLocale()` / `setActiveLocale(locale)` - module-level locale for code outside React (error strings in `api.ts`, default chat titles in `chatStore.ts`); the provider syncs it on mount.
- `isLocale(value)` - type guard.
- `localizeServerText(locale, text)` - maps known Portuguese strings a rolling-upgrade old binary might send ("pronto", "erro", "falhou", ...) to the UI language; heuristic regex detects untranslated Portuguese and falls back to a generic message rather than showing mixed language. As recusas de autoridade de implantação também usam as chaves `deploy.adminOnlyAdd` e `undeploy.adminOnly`, mantendo a mensagem do servidor clara em cada idioma. Antes do mapa fixo ele casa as frases que **carregam um nome de máquina** ("recusado por ksia", "cancelado por atena"): o nome não se traduz, o verbo sim, e o desfecho de uma transferência aparecia em português na tela em inglês.

The chat telemetry additions live in both locale blocks: execution RTT, the
source-to-destination edge scope, machine capacity/configuration, effective
placement, per-peer `POM_PROFILE_SYNC`, backend-reported execution engine,
  execution diagnosis, TP collective table, prefix reuse and transmitted-token
  counts, overlap status, per-traversal transport identity/bytes, AI-analysis
  action, initial-model mark and the explicit "time outside the node" diagnosis
  must remain available in
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

The `chat.history` message names the bounded conversation list used by the
`infer` Sidebar. It remains in both CSV columns even though it is primarily an
accessible label, so the menu has no language-specific copy in the component.

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
