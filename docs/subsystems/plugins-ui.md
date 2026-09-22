# Subsistema: interface fornecida por plugins

Um plugin pode entregar, junto do backend, o próprio menu, telas, rotas,
perfis e textos. O POM só implementa o mecanismo genérico (contrato
`pom-plugin-ui/v1`); nenhum plugin específico é conhecido pelo frontend. As
rotas nativas `/logs` e `/estatisticas` continuam como **fallbacks de
propaganda** do `EnterpriseFeatureShowcase`, mas um manifesto pode declarar
uma delas para substituir a tela nessa URL. As demais telas de plugin ficam sob
o namespace de cada `plugin_code`. O código real de Logs de Requisições e
Estatísticas mora em `pom-plugin-logs`, não aqui.

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
   (`components/nav.ts`), o guarda de perfil (`lib/access.ts`), o carregamento
   de textos (`lib/pluginI18n.ts`) e o roteador (`App.tsx`) consultam o mesmo
   registro validado.

## Manifesto (`pom-plugin-ui/v1`)

| Campo | Regra |
|---|---|
| `schema`, `plugin_code` | `pom-plugin-ui/v1`; o código tem de ser o do plugin instalado |
| `menu[]` | `id`, `label` por idioma (`en` obrigatório), `icon` (nome conhecido, senão genérico), `to`, `roles` opcional (`admin`/`node`/`infer`); sem `roles` nem o campo legado `profiles`, todos os perfis são elegíveis. `group` é aceito, mas não organiza itens |
| `routes[]` | `path`, `screen`, `profiles` (`admin`/`node`/`infer`, não vazio), `full_bleed` |
| `screens{}` | `module` (asset ES), `export` (padrão `default`), `styles[]` |
| `i18n{}` | idioma -> asset JSON; se presente, `en` é obrigatório |
| `assets[]` | lista fechada; caminhos relativos, sem `..`; tudo o que telas e i18n citam tem de estar aqui |

O host normaliza cada `menu[].to` e `routes[].path` sob `/<plugin_code>/...` e
guarda os dois no mesmo registro. Assim, `plugin_code: "base"` com `to: "/example"`
e `path: "/example"` usa `/base/example` tanto no link quanto no matcher; no
navegador, o basename acrescenta `/admin-ui`. Barras repetidas e finais são
normalizadas. Um prefixo já igual ao `plugin_code` não é duplicado, mantendo
manifestos que já carregam esse segmento. Caminhos com `.`/`..`, separadores
codificados, schema inválido, código divergente ou estruturas inconsistentes
fazem o host ignorar o manifesto inteiro. O destino do menu também precisa ser
coberto por uma rota declarada.

Plugins diferentes podem usar o mesmo caminho relativo: seus destinos ficam,
por exemplo, em `/base/example` e `/analytics/example`. `/logs` e
`/estatisticas` são pontos de extensão nativos: se exatamente um plugin ativo
declarar um deles, sua rota e seu menu usam a URL nativa e substituem o fallback
correspondente. Se dois plugins reivindicarem o mesmo ponto, nenhum assume a
URL nativa e ambos ficam sob seus namespaces, evitando colisão. Sem uma
declaração única, o fallback do POM permanece disponível.

Menus novos são exibidos como itens de primeiro nível antes de Documentação,
na ordem em que aparecem nos manifestos e na lista de plugins. O campo `group`
não cria submenus; a hierarquia de menus será definida por uma evolução futura
do contrato, sem mudar o identificador `pom-plugin-ui/v1`. O registro normaliza
uma vez cada destino e a navegação e o roteador consomem esse mesmo caminho.
O basename `/admin-ui` continua sendo aplicado pelo `BrowserRouter`, inclusive
em recarga direta e navegação pelo histórico do navegador.

Os perfis da rota decidem quem entra na tela do plugin - inclusive `infer`, cuja
lista reduzida do POM não se aplica a essas rotas. `menu[].roles` restringe a
visibilidade do link e a rota direta correspondente; a lista efetiva é a
interseção com `routes[].profiles`. Para manifests existentes,
`menu[].profiles` continua aceito como equivalente quando `roles` não foi
declarado; `roles` prevalece quando os dois campos aparecem. Sem nenhum dos
campos, todos os perfis são elegíveis, ainda sujeitos aos perfis da rota. Perfil
fora da lista efetiva recebe a recusa normal.

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
- Frontend: `lib/pluginUi.test.ts` (registro, namespaces, colisões, caminhos,
  perfis e menu), `lib/pluginI18n.test.ts` (instalação, idiomas selecionados,
  ausência/invalidez), `App.test.tsx` (fallback, tela por perfil, duas rotas
  relativas iguais e URL direta sob `/admin-ui`).
- Ponta a ponta manual feita na entrega: o `cdylib` real carregado por
  `pom-plugin-host` respondendo `ui.manifest`/`ui.asset`, e o bundle real
  renderizado no `App` do POM com o catálogo pt-BR do plugin.
