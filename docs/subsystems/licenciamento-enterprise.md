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
  `pom-release.sh`, instalação atômica e verificação Ed25519/hash/ABI. Ainda
  faltam code signing/notarização nativos, rollback com ponteiro, circuit
  breaker completo e matriz real de três plataformas.

O servidor de licenças pode evoluir em paralelo: o cliente não depende dele
para os testes locais porque o contrato é exercitado pelo mock.

## Testes

```bash
cargo test -p enterprise-protocol
cargo test -p enterprise-plugin-fixture
cargo test -p pom-plugin-host
cargo test -p node enterprise
cd apps/frontend && npm test -- src/lib/access.test.ts src/components/nav.test.ts src/App.test.tsx
./scripts/build-enterprise-plugin.sh --platform macos-aarch64 --debug
./pom-release.sh --platform linux-x86_64 --plugin enterprise --dry-run
```

## Maintaining this file

Atualize este documento quando uma fase mudar de estado ou quando a fronteira
entre o processo `pom`, o host e a biblioteca Enterprise mudar. Mantenha
explícitos os fallbacks de migração e os limites que ainda não têm integração.
