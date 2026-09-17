# Subsistema: licenciamento Enterprise

Este documento registra o que o cliente entrega nesta rodada e separa as
garantias atuais dos limites restantes. A especificação completa
é o relatório de design em
`/Users/jairpereira/Sites/firstmate/data/rdma-enterprise-licenciamento-design/report.md`.

## Fronteiras entregues

- `crates/enterprise-protocol` contém os DTOs versionados do contrato de
  licença (`pom-license/v1`), o estado de entitlement, a impressão digital
  versionada e os envelopes de ativação e heartbeat.
- `crates/node/src/enterprise.rs` contém o cliente HTTPS mockável. O runtime
  usa o endpoint real como configuração padrão, mas os testes usam
  `MockLicenseService`; a chave de licença só atravessa validação/ativação e
  não faz parte do snapshot local de ativação. O cliente consulta `/v1/keys`,
  verifica grant e manifest Ed25519, além de hash, tamanho, ABI e plataforma.
  O heartbeat desserializa o payload rico de renovação e persiste o novo token,
  expiração, prazo do próximo heartbeat, entitlements, grant e release
  informados pelo servidor, sem substituir o prazo por uma estimativa local.
- `crates/plugin-host` é o processo `pom-plugin-host`. Ele carrega um artefato
  somente por caminho absoluto, valida ABI, tamanho e SHA-256, mantém a
  biblioteca carregada dentro do processo auxiliar e conversa com o cliente por
  JSON com tamanho prefixado em stdin/stdout. O `pom` não usa `libloading`.
- `crates/enterprise-plugin-fixture` é uma biblioteca `cdylib` de teste para
  validar a entrada ABI `pom_enterprise_plugin_v1` e o fluxo do host.
- `crates/enterprise-plugin` é a biblioteca `cdylib` de produção. O domínio de
  auditoria vive em `enterprise.db`, com schema, índices, listagem filtrada e
  agregações de estatísticas próprios.
- `crates/node/src/enterprise_host.rs` supervisiona o processo auxiliar por
  IPC assíncrono com frames limitados, timeout e encerramento ordenado. O
  caminho ativo envia eventos neutros ao host; o `pom.db` legado é fallback
  transitório para testes, migração e falha do plugin.
- A imagem Docker copia `pom-plugin-host` para `/usr/local/bin` ao lado de
  `pom`. O entrypoint valida ambos, e o `pom` inicia o host somente após a
  ativação, com o caminho absoluto do plugin baixado e pipes stdin/stdout;
  `docker-compose.yml` só precisa persistir `/data`, sem socket IPC extra.

O ABI C v1 é deliberadamente pequeno e explícito: descriptor imutável com
versão, capacidades e callbacks `create`, `ingest`, `query`, `free_buffer` e
`shutdown`, todos com tipos `repr(C)`. O host valida todos os callbacks antes
de criar o handle. O IPC rejeita frames vazios ou maiores que 1 MiB.

## Entitlement e interface

`GET /api/ui/enterprise/status` publica somente o estado do entitlement e o
link de compra. A resposta começa em `needs_activation`. Ativação, grant,
manifest, lease e host pronto precisam existir antes de `active` ficar
verdadeiro. Falha transitória de heartbeat entra em `degraded` durante a
janela de tolerância; licença revogada ou expirada desativa as features.

As rotas autenticadas `/api/ui/requests` e `/api/ui/requests/stats` falham
fechado com `403` e `code: enterprise_required` quando a feature correspondente
não está ativa. `/api/ui/cluster-logs` continua sendo log operacional Free.

As entradas `Logs de Requisições` e `Estatísticas` continuam no menu para todos
os perfis, com ou sem licença. Um acesso direto ou um clique sem entitlement
mostra a tela Enterprise e o link
  `https://pieceofmind.cloud/enterprise`. O menu é descoberta do produto, não
  prova de autorização: o backend continua sendo a fronteira de segurança.
  A tela Sobre também reutiliza `GET /api/ui/enterprise/status`, mostra `active`
  e `state`, e oferece `POST /api/ui/enterprise/pom-update/check` para uma
  detecção manual. Se `core_update` for encontrado, Sobre leva o operador para
  `/enterprise`, que mantém a única confirmação e aplicação do binário.

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

O servidor de licenças pode evoluir em paralelo: o cliente não depende dele
para os testes locais porque o contrato é exercitado pelo mock.

## Detecção e atualização autorizada do binário

Uma instalação só consulta releases do próprio `pom` depois de um heartbeat
Enterprise aceito e enquanto `EnterpriseStatus.active` for verdadeiro. O modo
Free não chama esse caminho. A detecção diária apenas compara a release e
publica `EnterpriseStatus.core_update`; não baixa, troca nem reinicia. O cliente
usa a mesma rota pública e os mesmos DTOs do plugin, com o código de catálogo
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
`POST /api/ui/enterprise/pom-update/apply` com
`{ "release_id": "<release>" }`. O cliente exige que a release ainda esteja
anunciada, reconsulta o manifest para obter uma concessão fresca, verifica a
assinatura Ed25519 pela mesma lista `/v1/keys` usada pelo plugin Enterprise,
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

A tela Sobre chama `POST /api/ui/enterprise/pom-update/check` para repetir a
detecção sob demanda e recebe o mesmo `EnterpriseStatus` atualizado do polling
de status. Essa rota exige a sessão administrativa, não baixa artefatos e não
reinicia o processo; a ação de aplicar continua concentrada em `/enterprise`
para evitar duas implementações da confirmação e da instalação.

## Testes

```bash
cargo test -p enterprise-protocol
cargo test -p enterprise-plugin-fixture
cargo test -p pom-plugin-host
cargo test -p node enterprise
cd apps/frontend && npm test -- src/lib/access.test.ts src/components/nav.test.ts src/App.test.tsx
./scripts/build-enterprise-plugin.sh --platform macos-aarch64 --debug
./scripts/test_pom_release.sh
./pom-release.sh --platform linux-x86_64 --plugin enterprise --dry-run
./pom-release.sh --platform linux-x86_64 --plugin pom_core --dry-run
./pom-release.sh --platform macos-aarch64 --plugin pom_core --dry-run
./pom-release.sh --platform windows-x86_64 --plugin pom_core --dry-run
```

## Maintaining this file

Atualize este documento quando uma fase mudar de estado ou quando a fronteira
entre o processo `pom`, o host e a biblioteca Enterprise mudar. Mantenha
explícitos os fallbacks de migração e os limites que ainda não têm integração.
