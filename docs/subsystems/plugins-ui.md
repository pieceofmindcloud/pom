# Subsistema: interface fornecida por plugins

Um plugin pode entregar, junto do backend, o próprio menu, telas, rotas,
perfis e textos. O POM só implementa o mecanismo genérico (contrato
`pom-plugin-ui/v1`); nenhum plugin específico é conhecido pelo frontend. O que
o POM mantém de cada plugin são **fallbacks de propaganda** para quando ele não
está instalado/ativo (hoje `EnterpriseFeatureShowcase` em `/logs` e
`/estatisticas`). O código real de Logs de Requisições e Estatísticas mora em
`pom-plugin-logs`, não aqui.

## Como chega ao navegador

1. O artefato do plugin (o mesmo binário verificado por SHA-256 na instalação)
   carrega o manifesto, o bundle JS/CSS e os catálogos i18n. O host do plugin
   responde as operações `ui.manifest` e `ui.asset` do `query` genérico. Nada
   muda no servidor de licenças, e manifesto e backend nunca divergem de versão.
2. `crates/node/src/plugin_ui.rs` valida o manifesto (`PluginUiManifest::parse`)
   e serve:
   - `GET /api/ui/plugins/ui` - `{plugins: [{code, version, manifest}]}` só dos
     plugins **ativos** com host rodando e manifesto válido. Inativo, ausente
     ou inválido = não aparece = só os fallbacks.
   - `GET /api/ui/plugins/:code/assets/*path` - só o que o manifesto lista em
     `assets`; tipo de conteúdo numa lista fechada, `nosniff`, `no-cache`.
     Catálogos i18n passam por `validate_catalog` antes de sair.
   Ambas são GET comuns de `/api/ui/plugins` (`RouteScope::CommonOrInternal`);
   a autorização dos dados que a tela lê continua nas rotas do plugin (por
   exemplo `/api/ui/requests`, AdminOnly).
3. O frontend (`usePluginUi`) guarda o resultado em `lib/pluginUi.ts`. O menu
   (`components/nav.ts`), o guarda de perfil (`lib/access.ts`) e o roteador
   (`App.tsx`) consultam o mesmo registro.

## Manifesto (`pom-plugin-ui/v1`)

| Campo | Regra |
|---|---|
| `schema`, `plugin_code` | `pom-plugin-ui/v1`; o código tem de ser o do plugin instalado |
| `menu[]` | `id`, `label` por idioma (`en` obrigatório), `icon` (nome conhecido, senão genérico), `to`, `group` opcional (`nav.<group>`), `profiles` |
| `routes[]` | `path`, `screen`, `profiles` (`admin`/`node`/`infer`, não vazio), `full_bleed` |
| `screens{}` | `module` (asset ES), `export` (padrão `default`), `styles[]` |
| `i18n{}` | idioma -> asset JSON; se presente, `en` é obrigatório |
| `assets[]` | lista fechada; caminhos relativos, sem `..`; tudo o que telas e i18n citam tem de estar aqui |

Menu: um item cujo `to` já existe no menu do POM (o fallback) o **substitui**
(rótulo e ícone); um novo entra no grupo pedido ou na raiz, antes da
documentação. Rota de plugin **substitui** o fallback naquele caminho, e os
perfis do manifesto decidem quem entra - inclusive `infer`, cuja lista reduzida
do POM não se aplica a rotas de plugin. Perfil fora da lista recebe a recusa
normal, nunca a propaganda.

## Telas

`components/PluginScreen.tsx` importa o módulo do asset (mesma origem, a sessão
vale) e monta o export declarado como componente React da própria árvore, com
`ErrorBoundary`; falha vira um cartão de erro. O plugin não empacota outro
React: `lib/pluginHost.ts` expõe em `globalThis.__POM_HOST__` React, ReactDOM,
o runtime JSX, React Query, o kit de design (`components/ui`), `format`,
`useI18n`, `useTheme`, `useCluster`, `api.get`, `Modal`, `ExecutionTelemetry`,
`Markdown` e `storageKey`. O plugin troca esses imports por leituras desse
objeto na hora de empacotar; hooks e contextos são os da aplicação. A
superfície é versionada (`PLUGIN_HOST_VERSION`) e só cresce de forma
compatível. O CSS do plugin usa os mesmos tokens de cor (variáveis CSS) do POM.

## i18n do plugin

Cada plugin leva os próprios textos: `{"<plugin_code>.chave": "texto ICU"}` por
idioma. `lib/pluginI18n.ts` carrega os idiomas **selecionados** (o ativo e `en`
como piso, só os que o manifesto declara e o POM suporta), valida de novo no
navegador (chaves só no espaço `<plugin_code>.`, valores string, tetos) e
entrega a `setPluginMessages` (`i18n/locales.ts`), que os coloca no mesmo mapa
do `formatMessage` sem nunca sobrescrever chave do POM e os remove quando o
plugin sai. Catálogo ausente, ilegível ou inválido é descartado com aviso: cai
no `en` ou na chave. **O POM não é dono de nenhum texto de plugin.**

## Testes

- Rust: `cargo test -p node --bin pom plugin_ui` (manifesto, caminhos, perfis,
  catálogos, o manifesto real do plugin `enterprise`).
- Frontend: `lib/pluginUi.test.ts` (registro, perfis, menu), `lib/pluginI18n.test.ts`
  (instalação, idiomas selecionados, ausência/invalidez), `App.test.tsx`
  (fallback vs tela do plugin por perfil).
- Ponta a ponta manual feita na entrega: o `cdylib` real carregado por
  `pom-plugin-host` respondendo `ui.manifest`/`ui.asset`, e o bundle real
  renderizado no `App` do POM com o catálogo pt-BR do plugin.
