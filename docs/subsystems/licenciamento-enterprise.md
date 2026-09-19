# Subsistema: licenciamento de plugins (ex-Enterprise)

Este documento registra o que o cliente entrega nesta rodada e separa as
garantias atuais dos limites restantes. A especificação completa
é o relatório de design em
`/Users/jairpereira/Sites/firstmate/data/rdma-enterprise-licenciamento-design/report.md`.

**Pivô de produto (2026-09-18):** o POM deixou de ter uma única "licença
Enterprise" de produto e passou a ter PLUGINS INDEPENDENTES - qualquer
funcionalidade do menu pode ser um plugin próprio, comprado/ativado/desativado
separadamente. Hoje o catálogo (`crate::plugins::PLUGIN_CATALOG`) só tem uma
entrada, `"enterprise"` (o plugin de Estatísticas/Logs de Requisições), mas o
gerenciador (`PluginManager`/`PluginRuntime` em `crates/node/src/plugins.rs`)
é genérico para N desde essa mudança: cada `plugin_code` tem seu próprio
estado de ativação, lease, heartbeat, host de processo e checagem de
atualização de versão, sem nada compartilhado entre plugins. O nome deste
arquivo e do `plugin_code` `"enterprise"` ficaram como estão por não valer o
custo de renomear - o que mudou é que o CÓDIGO GERAL parou de assumir "um
produto licenciável só". Ver `## Fase 7` abaixo para o que essa mudança
tocou e o que ficou de fora de propósito (a atualização do `pom_core` em si,
que é uma fronteira de produto separada desde antes e continua sendo).

## Fronteiras entregues

- `crates/enterprise-protocol` contém os DTOs versionados do contrato de
  licença (`pom-license/v1`), o estado de entitlement, a impressão digital
  versionada e os envelopes de ativação e heartbeat.
- `crates/node/src/plugins.rs` contém o gerenciador genérico
  (`PluginManager`/`PluginRuntime`, um por `plugin_code` de
  `PLUGIN_CATALOG`) e o cliente HTTPS mockável. O runtime usa o endpoint real
  como configuração padrão, mas os testes usam `MockLicenseService` (uma
  resposta por `plugin_code`, para provar dois plugins independentes); a
  chave de licença só atravessa validação/ativação e não faz parte do
  snapshot local de ativação (`plugin_license:<code>`). O cliente consulta
  `/v1/keys`, verifica grant e manifest Ed25519, além de hash, tamanho, ABI e
  plataforma. Uma ativação por chave primeiro chama `/v1/licenses/validate`
  uma vez para descobrir quais `plugin_code` ela concede (`licensed_plugins`,
  uma lista de códigos puros), depois ativa cada um separadamente via
  `/v1/licenses/activations` com `plugin_code` no payload. As features de um
  plugin vêm do `feature_set` do manifesto do release, não da licença. O
  heartbeat desserializa o payload rico de renovação e persiste o novo token,
  expiração, prazo do próximo heartbeat, grant e release informados pelo servidor,
  sem substituir o prazo por uma estimativa local - tudo isso por
  `PluginRuntime`, nunca compartilhado entre plugins. Cada `PluginRuntime`
  também checa sua PRÓPRIA atualização de versão (`detect_update`/
  `apply_update`), reusando o manifest genérico do seu `plugin_code` -
  independente de qualquer outro plugin e do `pom_core` (ver `## Detecção e
  atualização` mais abaixo, que é sobre o `pom_core`, não sobre isto).
- `crates/plugin-host` é o processo `pom-plugin-host`. Ele carrega um artefato
  somente por caminho absoluto, valida ABI, tamanho e SHA-256, mantém a
  biblioteca carregada dentro do processo auxiliar e conversa com o cliente por
  JSON com tamanho prefixado em stdin/stdout. O `pom` não usa `libloading`.
- `crates/enterprise-plugin-fixture` é uma biblioteca `cdylib` de teste para
  validar a entrada ABI `pom_enterprise_plugin_v1` e o fluxo do host.
- `crates/enterprise-plugin` é a biblioteca `cdylib` de produção. O domínio de
  auditoria vive em `enterprise.db`, com schema, índices, listagem filtrada e
  agregações de estatísticas próprios.
- `crates/node/src/plugin_host.rs` (`PluginHost`, renomeado de
  `EnterpriseHost`) supervisiona o processo auxiliar por IPC assíncrono com
  frames limitados, timeout e encerramento ordenado. Uma instância por
  chamada de `start()` - nada é global - então `PluginManager` mantém uma
  instância concorrente por plugin ativo sem se pisarem. O caminho ativo
  envia eventos neutros ao host; o `pom.db` legado é fallback transitório
  para testes, migração e falha do plugin.
- A imagem Docker copia `pom-plugin-host` para `/usr/local/bin` ao lado de
  `pom`. O entrypoint valida ambos, e o `pom` inicia o host somente após a
  ativação, com o caminho absoluto do plugin baixado e pipes stdin/stdout;
  `docker-compose.yml` só precisa persistir `/data`, sem socket IPC extra.

O ABI C v1 é deliberadamente pequeno e explícito: descriptor imutável com
versão, capacidades e callbacks `create`, `ingest`, `query`, `free_buffer` e
`shutdown`, todos com tipos `repr(C)`. O host valida todos os callbacks antes
de criar o handle. O IPC rejeita frames vazios ou maiores que 1 MiB.

## Entitlement e interface

`GET /api/ui/plugins` publica o catálogo inteiro - inclusive plugins que o
operador nunca ativou - cada entrada com seu próprio estado de entitlement,
`manage_url` e `available_update`. A resposta de cada plugin começa em
`needs_activation`. Ativação, grant, manifest, lease e host pronto precisam
existir antes de `active` ficar verdadeiro. Falha transitória de heartbeat
entra em `degraded` durante a janela de tolerância; licença revogada ou
expirada desativa as features - de UM plugin, nunca dos outros.
`POST /api/ui/plugins/activate` recebe só `{license_key}` (nenhum
`plugin_code`: a resposta `{activated: [...]}` nomeia quais plugins aquela
chave concedeu e instalou). `POST /api/ui/plugins/:code/refresh|deactivate`
e `POST /api/ui/plugins/:code/update/check|apply` agem sobre um plugin só.

As rotas autenticadas `/api/ui/requests` e `/api/ui/requests/stats` falham
fechado com `403` e `code: enterprise_required` quando o plugin `enterprise`
não tem a feature correspondente ativa. `/api/ui/cluster-logs` continua sendo
log operacional Free.

As entradas `Logs de Requisições` e `Estatísticas` continuam no menu para todos
os perfis, com ou sem licença. Um acesso direto ou um clique sem entitlement
mostra o convite Enterprise e o link `https://pieceofmind.cloud/enterprise`
(via `useEnterpriseStatus()`, um seletor sobre `usePlugins()` para o
`plugin_code` `"enterprise"`). O menu é descoberta do produto, não prova de
autorização: o backend continua sendo a fronteira de segurança.

A tela **Plugins** (`/plugins`, `apps/frontend/src/routes/Plugins.tsx`) é a
única que ativa, atualiza e desativa plugins - substituiu a antiga tela
`Enterprise.tsx` de produto único. Ela NÃO trata da atualização do binário
`pom` em si: essa fica inteira na tela **Sobre** (`/config/sobre`), atrás de
`GET /api/ui/pom-update/status` e `POST /api/ui/pom-update/check|apply` - ver
`## Detecção e atualização autorizada do binário` abaixo. As duas telas e as
duas famílias de rotas nunca se misturam, no código nem na UI.

## Fases e limites desta entrega

- **Fase 0 - contrato:** entregue em `crates/enterprise-protocol`, incluindo
  envelope, fingerprint, estados, lease, manifest, grant, download e status.
- **Fase 1 - licença sem plugin:** entregue com `validate`, ativação idempotente,
  heartbeat, status após reinício, desativação e persistência sem chave. A
  supervisão roda fora do caminho quente e mantém inferência Free disponível.
- **Fase 2 - host e ABI:** entregue com `pom-plugin-host`, ABI C v1, IPC
  enquadrado, fixture e biblioteca real. A biblioteca nunca é carregada pelo
  processo `pom`.
- **Fase 3 - extração de auditoria:** entregue no caminho Enterprise ativo.
  O plugin grava e consulta `enterprise.db`, preservando os DTOs atuais.
  `reqlog.rs` e o schema antigo permanecem como fallback de migração e de
  indisponibilidade; sua remoção definitiva exige importação explícita.
- **Fase 4 - P2P Enterprise:** entregue o contrato de prova de fingerprint,
  assinatura por nonce e a rota interna autenticada
  `/api/internal/enterprise/fingerprint`. Falta ligar a coleta ao fluxo admin
  de `activation-batches` e à transferência de grants/artefatos entre peers.
- **Fase 5 - frontend:** entregue `/enterprise`, ativação, refresh e estado
  ativo. `/logs` e `/estatisticas` permanecem visíveis e clicáveis sem licença,
  mostrando o convite Enterprise em vez de dados vazios.
- **Fase 6 - release/hardening:** entregues `build-enterprise-plugin.sh`,
  `pom-release.sh`, instalação atômica e verificação Ed25519/hash/ABI.
  `pom-release.sh` consulta `/internal/releases?plugin=<plugin>&status=all`
  com o token `POM_RELEASE_TOKEN` antes do upload, considera releases de todas
  as plataformas e escolhe o próximo `+build.N`; `--version` continua sendo um
  override manual. O catálogo do binário POM usa exatamente `pom_core`, como
  definido no `license-server/AGENTS.md`. Para `pom_core`, o script compila o
  pacote `node` com os defaults `cuda,vulkan` no Linux, `metal` no macOS e
  nenhuma feature no Windows CPU; `--features` substitui esse conjunto e
  `--feature-set` altera somente o metadado do upload. O artefato é `pom` em
  Linux/macOS e `pom.exe` no Windows. O servidor aceita `plugin_abi=1` como
  campo obrigatório genérico para `pom_core`, sem representar ABI de cdylib,
  e `feature_set=core` continua sendo apenas metadado, sem entitlement.
  O `--dry-run` permanece sem rede e usa `+build.1` como prévia quando não há
  override. Ainda faltam code
  signing/notarização nativos, rollback com ponteiro, circuit breaker completo
  e matriz real de três plataformas.
- **Fase 7 - N plugins independentes (2026-09-18):** entregue o pivô de
  produto. `crates/node/src/enterprise.rs` (um `EnterpriseState` global) virou
  `crates/node/src/plugins.rs` (`PluginManager` com um `PluginRuntime` por
  `plugin_code` de `PLUGIN_CATALOG`, hoje só `"enterprise"`, estrutura genérica
  para N); `enterprise_host.rs` virou `plugin_host.rs`
  (`PluginHost`, já suportava várias instâncias concorrentes - confirmado, não
  precisou mudar). A UI trocou a tela `Enterprise.tsx` por `Plugins.tsx`
  (`/plugins`), que lista o catálogo inteiro e ativa/atualiza/desativa por
  plugin. A detecção/aplicação de atualização do `pom_core` saiu de dentro do
  `EnterpriseState` e ganhou arquivo próprio, `pom_core_update.rs`
  (`PomCoreUpdater`), com sua própria tela inteira em `/config/sobre` - nunca
  mais compartilhando código ou UI com a tela de Plugins. **Suposição de
  contrato**, agora confirmado contra o servidor real: `ActivationPayload`/
  `ActivationResponse`/`GrantBody` carregam `plugin_code`, e a ativação é uma
  chamada por `plugin_code` (o cliente descobre quais chamar validando uma vez
  e lendo `licensed_plugins`).

O servidor de licenças evolui em paralelo, e só o mock não basta para
acompanhá-lo: o mock concordava com a suposição errada deste cliente, então os
testes ficaram verdes enquanto toda ativação quebrava em produção com
`missing field \`entitlements\``. Por isso o contrato é afirmado também sobre
corpos de resposta REAIS, capturados do license-server, em
`crates/enterprise-protocol/tests/wire_contract.rs`; o mesmo contrato é afirmado
do outro lado em `license-server/test/wire-contract.test.ts` (repositório
pom-website), que regrava essas fixtures com `POM_WIRE_FIXTURES=<dir>`.

## Detecção e atualização autorizada do binário

Esta seção é sobre o `pom_core`, NUNCA sobre um plugin - ver `crate::pom_core_update`,
deliberadamente separado de `crate::plugins` desde a Fase 7. Uma instalação só
consulta releases do próprio `pom` enquanto o plugin `enterprise` estiver
`active` (mecanismo transicional: ele empresta o lease desse plugin porque é
assim que a autenticação funciona hoje; a tarefa irmã
`rdma-pom-core-github-releases` vai substituir isto por GitHub Releases
público, sem licença nenhuma). O modo sem esse plugin ativo não chama esse
caminho. A detecção compara a release e publica o resultado em
`GET /api/ui/pom-update/status`; não baixa, troca nem reinicia. O cliente usa
a mesma rota pública e os mesmos DTOs de plugin, com o código de catálogo
`pom_core`:

```text
GET /v1/plugins/pom_core/manifest
    ?lease_id=<lease>&os=<consts::OS>&arch=<consts::ARCH>
    &channel=stable&host_version=<CARGO_PKG_VERSION>&plugin_abi=1
Authorization: Bearer <lease_token>
```

A resposta esperada é o `ManifestResponse` já existente, com `manifest` e
`download`. O lado servidor deve publicar para `pom_core` um manifest com
`product: "pom"`, `channel: "stable"`, a plataforma solicitada, `version` em
SemVer, `min_host_version`, `sha256`, `size`, `key_id` e `signature`; o grant de
download continua apontando para a rota existente
`/v1/plugins/pom_core/:release_id/download`. Não é usado o endpoint interno de
listagem de releases. O `plugin_abi` deve continuar em `1`; quando não houver
changelog, o campo deve ser omitido para preservar a canonicalização assinada.
O `release_id` de uma release do core deve ser o commit
curto carimbado em `POM_BUILD_COMMIT` pelo `crates/node/build.rs`. Assim, o
cliente atualiza quando a versão SemVer é maior ou quando a versão é igual e o
commit é diferente, sem fazer downgrade.

Quando o operador confirma o `release_id` exibido, a UI chama
`POST /api/ui/pom-update/apply` com
`{ "release_id": "<release>" }`. O cliente exige que a release ainda esteja
anunciada, reconsulta o manifest para obter uma concessão fresca, verifica a
assinatura Ed25519 pela mesma lista `/v1/keys` usada pelos plugins,
confere tamanho e SHA-256, grava um arquivo temporário no mesmo diretório e faz
`rename` atômico sobre o executável. Em Unix, ele captura o caminho antes do
rename e usa `respawn_at` para iniciar o binário novo com o mesmo argv. Se a
confirmação ficar obsoleta ou qualquer download/verificação falhar, nenhum
reinício é agendado e o binário em execução permanece intacto.

Windows não permite normalmente substituir o executável em uso. Depois da
confirmação, o cliente deixa o arquivo validado ao lado do executável e inicia
um helper PowerShell sem carregar o `pom`; o helper espera o processo antigo
terminar, faz `Move-Item -Force` e inicia o destino com os argumentos originais.
Se o helper não puder ser iniciado, o processo antigo continua rodando e o
arquivo temporário permanece para diagnóstico. A troca do Windows depende,
portanto, do PowerShell presente no sistema e não tem a mesma atomicidade
imediata do rename Unix.

A tela Sobre chama `POST /api/ui/pom-update/check` para repetir a
detecção sob demanda e lê `GET /api/ui/pom-update/status` para o estado
corrente (`usePomCoreUpdateStatus()`, contrato próprio `PomCoreUpdateStatus` -
nunca `PluginStatus`). Essa rota exige a sessão administrativa, não baixa
artefatos e não reinicia o processo; a própria tela Sobre concentra a
confirmação e a instalação, sem redirecionar para nenhuma outra tela - ver
`## Fase 7` acima.

## Testes

```bash
cargo test -p enterprise-protocol
cargo test -p enterprise-plugin-fixture
cargo test -p pom-plugin-host
cargo test -p node plugins
cargo test -p node pom_core_update
cd apps/frontend && npm test -- src/lib/access.test.ts src/components/nav.test.ts src/App.test.tsx src/routes/Plugins.test.tsx src/routes/About.test.tsx
./scripts/build-enterprise-plugin.sh --platform macos-aarch64 --debug
./scripts/test_pom_release.sh
./pom-release.sh --platform linux-x86_64 --plugin enterprise --dry-run
./pom-release.sh --platform linux-x86_64 --plugin pom_core --dry-run
./pom-release.sh --platform macos-aarch64 --plugin pom_core --dry-run
./pom-release.sh --platform windows-x86_64 --plugin pom_core --dry-run
```

## Maintaining this file

Atualize este documento quando uma fase mudar de estado, quando um plugin
novo entrar no catálogo, ou quando a fronteira entre o processo `pom`, o host
e a(s) biblioteca(s) de plugin mudar. Mantenha explícitos os fallbacks de
migração e os limites que ainda não têm integração - inclusive a dependência
transicional da atualização do `pom_core` no lease do plugin `enterprise`.
