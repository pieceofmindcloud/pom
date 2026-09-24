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
| `routes[]` | `path`, `screen`, `roles` opcional (`admin`/`node`/`infer`) ou `profiles` legado (`admin`/`node`/`infer`, não vazio), `full_bleed`; sem os dois, todos os perfis são elegíveis |
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
visibilidade do link e a rota direta correspondente; `routes[].roles` tem
precedência sobre `routes[].profiles`, e a lista efetiva é a interseção com os
menus que cobrem o destino. Para manifests existentes,
`menu[].profiles` continua aceito como equivalente quando `roles` não foi
declarado; `roles` prevalece quando os dois campos aparecem. Sem nenhum dos
campos do menu ou da rota, todos os perfis são elegíveis. Perfil fora da lista
efetiva recebe a recusa normal.

## Telas

`components/PluginScreen.tsx` importa o módulo do asset (mesma origem, a sessão
vale) e monta o export declarado como componente React da própria árvore, com
`ErrorBoundary`; falha vira um cartão de erro. O host remonta a tela quando a
rota normalizada muda, inclusive entre destinos do mesmo `plugin_code`, para
que uma navegação interna não retenha o componente da rota anterior. O plugin não empacota outro
React: `lib/pluginHost.ts` expõe em `globalThis.__POM_HOST__` React, ReactDOM,
o runtime JSX, React Query, o kit de design (`components/ui`), `format`,
`useI18n`, `useTheme`, `useCluster`, `api.get`, `Modal`, `ExecutionTelemetry`,
`Markdown` e `storageKey`. O plugin troca esses imports por leituras desse
objeto na hora de empacotar; hooks e contextos são os da aplicação. A
superfície é versionada (`PLUGIN_HOST_VERSION`) e só cresce de forma
compatível. O CSS do plugin usa os mesmos tokens de cor (variáveis CSS) do POM.

## Preferências de release e contexto do host

As preferências locais não fazem parte do contrato de tela `pom-plugin-ui/v1`
acima. O schema tipado é opcional em `PluginManifest.preferences` no contrato
de release/licença (`crates/enterprise-protocol`): toggles booleanos e um
campo opcional de diretório de armazenamento. O POM valida esse schema, aplica
os defaults, persiste os valores por `plugin_code` no `pom.db` e nunca envia os
valores ao servidor de licenças nem os trata como entitlement. A tela Plugins
edita os valores apenas enquanto o plugin está ativo.

`host.configure` inclui os valores locais sob `preferences`, junto do contexto
do gateway, para o próprio plugin ativo. Isso é IPC entre o nó e o host local,
não tráfego para o license-server. A atualização pública por GitHub é uma
origem separada: seus manifestos por plataforma são assets
`pom-plugin-<os>-<arch>.json`, e um binário baixado fica em staging até o
operador confirmar a instalação. Ver `licenciamento-enterprise.md` para campos
exigidos, hash e ciclo de checagem.

## Contexto do nó (`host.configure`)

Um plugin que chama os modelos do nó não embute endereço nem credencial: o
nó os entrega pelo `query` genérico, na operação `host.configure`:

```json
{"operation": "host.configure",
 "gateway": {"openai_base_url": "http://127.0.0.1:8080/v1", "api_key": "sk-..."},
 "preferences": {"features": {"capture_headers": false}, "storage_directory": null}}
```

`openai_base_url` é o gateway do próprio nó em loopback (texto claro de
loopback é sempre aceito, mesmo com TLS). `api_key` é a chave de chat do nó
(`pom-<node_id>`), a mesma que o Chat da interface usa, para que também
funcione num nó sem autoridade administrativa. O nó entrega o contexto a todo
host que sobe e de novo quando ele muda (chave revogada e recriada); plugins
que não conhecem a operação respondem erro, que é ignorado.

## Interface web servida pelo plugin (`/api/ui/plugins/:code/proxy/*`)

Uma tela de plugin pode depender de um servidor HTTP que o próprio plugin roda
em loopback - por exemplo uma aplicação web inteira. O plugin anuncia onde ele
está pela operação `ui.upstream`:

```json
{"status": "ready", "port": 43127, "token": "<segredo de 16 a 512 bytes ASCII>"}
```

(`{"status": "starting"}` e `{"status": "error", "error": "..."}` viram 503.)
`crates/node/src/plugin_proxy.rs` monta esse servidor em
`/api/ui/plugins/<code>/proxy/`, na mesma origem da interface, e encaminha:

- HTTP com corpo em streaming nos dois sentidos, e upgrades (WebSocket);
- o caminho depois do prefixo, byte a byte, e a query intacta;
- `x-pom-plugin-token: <token>`, para que só o nó alcance o servidor do plugin;
- `x-forwarded-prefix`, `x-forwarded-host` e `x-forwarded-proto`, para que o
  plugin gere URLs sob o prefixo.

Nunca seguem para o plugin o cookie de sessão, `Authorization` nem
`x-pom-access-token`, e o nó descarta `Set-Cookie` das respostas. A rota é
AdminOnly e usa a autorização da superfície administrativa, porque scripts,
WebSockets e `EventSource` do navegador não carregam o `Authorization` da
interface; em troca, recusa (403) `Origin` diferente do próprio nó,
`Sec-Fetch-Site` fora de `same-origin`/`none` e chamadas entre nós.

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
  catálogos, o manifesto real do plugin `enterprise`); `plugin_proxy`
  (`ui.upstream`, barreira de origem, cabeçalhos filtrados, HTTP e WebSocket
  contra um servidor real); e
  `plugins::tests::host_context_and_ui_proxy_reach_a_real_plugin_host`
  (`host.configure` na subida, na troca e no reinício de um host real, e o
  proxy pelo router da interface).
- Frontend: `lib/pluginUi.test.ts` (registro, namespaces, colisões, caminhos,
  perfis e menu), `lib/pluginI18n.test.ts` (instalação, idiomas selecionados,
  ausência/invalidez), `App.test.tsx` (fallback, tela por perfil, duas rotas
  relativas iguais e URL direta sob `/admin-ui`).
- Ponta a ponta manual feita na entrega: o `cdylib` real carregado por
  `pom-plugin-host` respondendo `ui.manifest`/`ui.asset`, e o bundle real
  renderizado no `App` do POM com o catálogo pt-BR do plugin.
