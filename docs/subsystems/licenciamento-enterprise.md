# Subsistema: licenciamento de plugins (ex-Enterprise)

Este documento registra o que o cliente entrega nesta rodada e separa as
garantias atuais dos limites restantes. A especificação completa
é o relatório de design em
`/Users/jairpereira/Sites/firstmate/data/rdma-enterprise-licenciamento-design/report.md`.

**Pivô de produto (2026-09-18):** o POM deixou de ter uma única "licença
Enterprise" de produto e passou a ter PLUGINS INDEPENDENTES - qualquer
funcionalidade do menu pode ser um plugin próprio, comprado/ativado/desativado
separadamente. O binário não mantém um catálogo estático de plugins
instaláveis: `PluginManager`/`PluginRuntime` em
`crates/node/src/plugins.rs` cria runtimes para todo `plugin_code` concedido
pelo servidor e restaura os códigos persistidos localmente. Cada plugin tem seu próprio
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
  (`PluginManager`/`PluginRuntime`, criado dinamicamente por `plugin_code`)
  e o cliente HTTPS mockável. O runtime usa o endpoint real
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
  expiração, prazo do próximo heartbeat e grant informados pelo servidor,
  sem substituir o prazo por uma estimativa local - tudo isso por
  `PluginRuntime`, nunca compartilhado entre plugins. `PersistedPluginState`
  separa duas releases que o heartbeat não pode confundir:
  `installed_release_id` é a release efetivamente baixada e instalada neste
  peer, e só `activate`/`activate_and_install`/`apply_update` concluídos podem
  avançá-la; o heartbeat sozinho nunca baixa nem verifica nada, então nunca
  escreve nela. `plugin_release_id`, `release_changed` e `available_release_id`
  do payload do heartbeat continuam consumidos, mas só para alimentar
  `status.available_update` - disparando `detect_update` (que reconsulta o
  manifest para trazer versão e changelog reais) quando `release_changed` é
  verdadeiro, inclusive no supervisor de heartbeat periódico, que antes nunca
  chamava `detect_update` por conta própria. Cada `PluginRuntime`
  também checa sua PRÓPRIA atualização de versão (`detect_update`/
  `apply_update`), reusando o manifest genérico do seu `plugin_code` -
  independente de qualquer outro plugin e do `pom_core` (ver `## Detecção e
  atualização` mais abaixo, que é sobre o `pom_core`, não sobre isto).

  `PersistedPluginState.installed_at` marca quando o host ficou pronto após a
  instalação inicial ou a aplicação de uma nova release. `update_checked_at`
  marca a última consulta bem-sucedida do manifest, mesmo quando não há update;
  falhas preservam a consulta boa anterior. Ambos aparecem em `PluginStatus` e
  são campos opcionais com default ao ler estados já persistidos. `last_success_at`
  segue reservado ao heartbeat e nunca serve como data de instalação.

  `PluginStatus.license_expires_at` é a validade da LICENÇA, distinta do
  `lease_expires_at` técnico acima: o lease é um prazo curto de renovação que
  o heartbeat empurra a cada poucos dias, a licença é o que a compra realmente
  concede (meses ou anos). Vem de `LicenseStatusResponse::license_expires_at`
  (`GET /v1/licenses/status`, o mesmo endpoint que `restore()` já consultava
  na inicialização), reconsultado também no clique manual em "Atualizar"
  (`PluginRuntime::refresh_license_status`, dentro de `PluginManager::refresh`)
  - nunca no heartbeat periódico, que não traz esse campo. Falha nessa consulta
  extra é só logada, sem derrubar o `refresh`. A tela mostrava antes o
  `lease_expires_at` sob o rótulo "Licença válida até", o que confundia um
  vencimento técnico de dias com a validade real da licença - corrigido
  reaproveitando o mesmo campo que `restore()` já buscava e descartava.

  **Defeito corrigido (2026-09-21):** até esta correção, o heartbeat escrevia o
  `plugin_release_id` recebido do servidor diretamente em
  `installed_release_id` - o mesmo campo que `detect_update` usa como base de
  comparação contra o `release_id` do manifest. Um heartbeat cujo servidor já
  reportasse a release seguinte (por exemplo, por tratar `0.1.0+build.1` e
  `0.1.0+build.2` como a mesma versão para fins de entitlement) adotava essa
  release como "instalada" sem nenhum download real; o clique em "Atualizar"
  chama heartbeat e depois `detect_update` na mesma ação
  (`PluginManager::refresh`), então a comparação seguinte via a base já
  adulterada e sempre concluía "sem atualização" - sem relação com o formato
  semver do release_id, que a comparação por igualdade de string já tratava
  corretamente. A regressão está em
  `refresh_button_does_not_lose_an_update_signalled_by_heartbeat` e
  `a_background_heartbeat_alone_also_surfaces_the_update`
  (`crates/node/src/plugins.rs`).
- `crates/plugin-host` é o processo `pom-plugin-host`. Ele carrega um artefato
  somente por caminho absoluto, valida ABI, tamanho e SHA-256, mantém a
  biblioteca carregada dentro do processo auxiliar e conversa com o cliente por
  JSON com tamanho prefixado em stdin/stdout. O `pom` não usa `libloading`.
- `crates/enterprise-plugin-fixture` é uma biblioteca `cdylib` de teste para
  validar a entrada ABI `pom_enterprise_plugin_v1` e o fluxo do host.
- A biblioteca `cdylib` de produção do plugin `enterprise` (o domínio de
  auditoria em `enterprise.db`, com schema, índices, listagem filtrada e
  agregações de estatísticas próprios) **não mora mais neste repositório**:
  foi extraída para `https://github.com/pereirajair/pom-plugin-logs` (ver
  `## Extração para pom-plugin-logs` abaixo). Este repositório mantém somente
  a fronteira genérica de licenciamento/host, reusável por qualquer
  `plugin_code` futuro.
- `crates/node/src/plugin_host.rs` (`PluginHost`, renomeado de
  `EnterpriseHost`) supervisiona o processo auxiliar por IPC assíncrono com
  frames limitados, timeout e encerramento ordenado. Um leitor persistente
  correlaciona cada resposta pelo ID, inclusive quando pedidos concorrentes
  retornam fora de ordem. Depois do timeout, o registro pendente é removido,
  mas o leitor consome uma resposta tardia pelo ID sem a entregar à chamada
  seguinte. Uma instância por chamada de `start()` - nada é global - então
  `PluginManager` mantém uma instância concorrente por plugin ativo sem se
  pisarem. O caminho ativo envia eventos neutros ao host; o `pom.db` legado é
  fallback transitório para testes, migração e falha do plugin.
- A imagem Docker copia `pom-plugin-host` para `/usr/local/bin` ao lado de
  `pom`. O entrypoint valida ambos, e o `pom` inicia o host somente após a
  ativação, com o caminho absoluto do plugin baixado e pipes stdin/stdout;
  `docker-compose.yml` só precisa persistir `/data`, sem socket IPC extra.

O ABI C v1 é deliberadamente pequeno e explícito: descriptor imutável com
versão, capacidades e callbacks `create`, `ingest`, `query`, `free_buffer` e
`shutdown`, todos com tipos `repr(C)`. O host valida todos os callbacks antes
de criar o handle. O IPC local por stdin/stdout rejeita frames vazios ou
maiores que 16 MiB, usando `enterprise_protocol::MAX_IPC_FRAME_BYTES` nos dois
processos. Esse limite cobre consultas e eventos do plugin, inclusive a
telemetria serializada, sem alterar os limites do transporte de inferência
entre peers.

## Extração para pom-plugin-logs (2026-09-21)

O código de produto do plugin `enterprise` (a lib `cdylib` que grava e
consulta `enterprise.db`) saiu
deste repositório para `https://github.com/pereirajair/pom-plugin-logs`, um
repositório de licenciamento próprio com seu build, empacotamento e pipeline
de release separados. O que **fica** aqui é só a fronteira genérica,
reusável por qualquer `plugin_code` futuro:

- `crates/enterprise-protocol` (DTOs, ABI C v1, `PLUGIN_ABI_VERSION`,
  `MAX_IPC_FRAME_BYTES`) continua sendo a fonte única do contrato. O novo
  repositório depende dele via git (`enterprise-protocol = { git =
  "https://github.com/pereirajair/rdma-p2p", rev = "<commit>" }`), fixado num
  commit, nunca num branch flutuante - Cargo resolve o pacote dentro do clone
  completo do workspace sem tentar compilar os outros membros (`node`,
  `llama-engine` etc.), porque nenhuma dependência de `enterprise-protocol`
  usa herança `.workspace = true` além de `version`/`edition`.
- `crates/enterprise-plugin-fixture` e `crates/plugin-host` continuam aqui
  porque testam/carregam o mecanismo genérico de ABI/IPC, não o domínio de
  auditoria de um plugin específico.
- `crates/node/src/plugins.rs` e `plugin_host.rs` não mudaram: o `pom` sempre
  tratou o artefato do plugin como um `cdylib` opaco baixado por manifest -
  de onde vem o código-fonte dele é irrelevante para o runtime.
- `plugin_code` continua sendo `"enterprise"` no `feature_set`
  (`enterprise.request_logs`,
  `enterprise.statistics`): só o repositório do código mudou, não a identidade
  do plugin nem o contrato com o servidor de licenças.
- `pom-release.sh` não libera mais nada com esse plugin_code: agora libera
  somente `pom_core`. O release do plugin `enterprise` (build, empacotamento
  reproduzível para macOS/Linux/Windows, GitHub Actions e a publicação da
  versão já gerada no GitHub no servidor de licenças) vive inteiramente em
  `pom-plugin-logs` - ver o `AGENTS.md` desse repositório.
- No frontend, o POM **não** contém mais as telas de Logs de Requisições e
  Estatísticas: elas vivem em `pom-plugin-logs` (`ui/`) e chegam pelo
  mecanismo genérico de UI de plugin (manifesto, telas e i18n instalados junto
  do backend) - ver `plugins-ui.md`. O POM mantém só o fallback de propaganda
  (`EnterpriseFeatureShowcase`, com link para
  `https://pieceofmind.cloud/enterprise/index.html`) para `/logs` e
  `/estatisticas` quando o plugin não está ativo. Uma versão anterior
  (97bb0e91) exibia só a vitrine mesmo com o plugin ativo ("Este plugin já está
  ativo neste cluster") - defeito corrigido. As rotas `/api/ui/requests` e
  `/api/ui/requests/stats` continuam no nó, AdminOnly, e recusam os dados sem
  entitlement.

## Entitlement e interface

`GET /api/ui/plugins` publica os estados locais dos plugins restaurados ou
vistos pelo runtime. O catálogo público `GET /v1/plugins` é lido diretamente
pelo browser e inclui plugins ainda sem estado local. Cada estado local tem
seu próprio entitlement, `manage_url` e `available_update`. A resposta começa
em `needs_activation`. Ativação, grant, manifest, lease e host pronto precisam
existir antes de `active` ficar verdadeiro. Falha transitória de heartbeat
entra em `degraded` durante a janela de tolerância; licença revogada ou
expirada desativa as features - de UM plugin, nunca dos outros.
`POST /api/ui/plugins/activate` recebe só `{license_key}` (nenhum
`plugin_code`: a resposta `{activated: [...]}` nomeia quais plugins aquela
chave concedeu e instalou). `POST /api/ui/plugins/:code/refresh|deactivate`
e `POST /api/ui/plugins/:code/update/check|apply` agem sobre um plugin só.

`GET /api/ui/plugins` é AdminOnly. Seu `PluginStatus` carrega `licensed_plugins`,
`activation_candidates` com release elegível, `license_expires_at` e
`plugin_usage` por `plugin_code`; os contadores no status escalar correspondem
ao plugin daquela entrada, não aos totais da licença. A tela usa esses dados
autenticados para distinguir licença ativa, ausente, expirada ou revogada e
para mostrar uso de ativações. O catálogo público não publica entitlement nem
uso de licença. Comprar leva à loja pública com o código do plugin na query.
O formulário de ativação aparece se qualquer plugin público que exige licença
não estiver ativo no estado local, mesmo quando outro já está ativo ou existe
contexto de licença. Falta de status local, expiração ou revogação mantém o
formulário visível; ele some quando todos os plugins licenciáveis conhecidos
estão ativos.

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
  `plugin_code`; a lista compilada da primeira implementação foi depois
  removida para aceitar todo código concedido pelo servidor);
  `enterprise_host.rs` virou `plugin_host.rs`
  (`PluginHost`, já suportava várias instâncias concorrentes - confirmado, não
  precisou mudar). A UI trocou a tela `Enterprise.tsx` por `Plugins.tsx`
  (`/plugins`), que lê a vitrine pública e ativa/atualiza/desativa por
  `plugin_code`. A detecção/aplicação de atualização do `pom_core` saiu de dentro do
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
./pom-release.sh --platform linux-x86_64 --dry-run
./pom-release.sh --platform macos-aarch64 --dry-run
./pom-release.sh --platform windows-x86_64 --dry-run
```

## Maintaining this file

Atualize este documento quando uma fase mudar de estado, quando um plugin
novo entrar no catálogo, ou quando a fronteira entre o processo `pom`, o host
e a(s) biblioteca(s) de plugin mudar. Mantenha explícitos os fallbacks de
migração e os limites que ainda não têm integração - inclusive a dependência
transicional da atualização do `pom_core` no lease do plugin `enterprise`.
