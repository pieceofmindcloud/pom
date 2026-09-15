# Subsistema: licenciamento Enterprise

Este documento registra o que o cliente entrega nesta primeira rodada e separa
as garantias atuais das fases ainda não implementadas. A especificação completa
é o relatório de design em
`/Users/jairpereira/Sites/firstmate/data/rdma-enterprise-licenciamento-design/report.md`.

## Fronteiras entregues

- `crates/enterprise-protocol` contém os DTOs versionados do contrato de
  licença (`pom-license/v1`), o estado de entitlement, a impressão digital
  versionada e os envelopes de ativação e heartbeat.
- `crates/node/src/enterprise.rs` contém o cliente HTTPS mockável. O runtime
  usa o endpoint real como configuração padrão, mas os testes usam
  `MockLicenseService`; a chave de licença só atravessa o pedido de ativação e
  não faz parte do snapshot local de ativação.
- `crates/plugin-host` é o processo `pom-plugin-host`. Ele carrega um artefato
  somente por caminho absoluto, valida ABI, tamanho e SHA-256, mantém a
  biblioteca carregada dentro do processo auxiliar e conversa com o cliente por
  JSON com tamanho prefixado em stdin/stdout. O `pom` não usa `libloading`.
- `crates/enterprise-plugin-fixture` é uma biblioteca `cdylib` de teste para
  validar a entrada ABI `pom_enterprise_plugin_v1` e o fluxo do host.

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

## Fases ainda pendentes

- **Fase 3 - extração de auditoria:** `crates/node/src/backend/reqlog.rs`
  ainda mantém a persistência SQLite, o schema, a replicação e os handlers de
  auditoria. O gate já está no handler, mas o banco Enterprise separado e o
  processamento pelo plugin ainda não existem.
- **Fase 4 - P2P Enterprise:** grants, estado de lease e pacotes Enterprise
  ainda não são propagados entre peers. Nenhuma chave de licença é propagada.
- **Fase 5 - frontend:** o contrato, a consulta, o menu descobrível e o
  callout com link foram entregues. Ainda falta uma tela de entrada da chave e
  a ativação ponta a ponta; os dados continuam protegidos pelo backend.
- **Fase 6 - release/hardening:** o `pom` ainda não supervisiona o processo
  host no fluxo de produção, nem baixa e verifica criptograficamente um plugin
  assinado. Também faltam persistência integrada da instalação, agendamento de
  heartbeat e do lease, rotação/remoção de artefatos, atualização e
  observabilidade.

O servidor de licenças pode evoluir em paralelo: o cliente não depende dele
para os testes locais porque o contrato é exercitado pelo mock.

## Testes

```bash
cargo test -p enterprise-protocol
cargo test -p enterprise-plugin-fixture
cargo test -p pom-plugin-host
cargo test -p node enterprise
cd apps/frontend && npm test -- src/lib/access.test.ts src/components/nav.test.ts src/App.test.tsx
```

## Maintaining this file

Atualize este documento quando uma fase mudar de estado ou quando a fronteira
entre o processo `pom`, o host e a biblioteca Enterprise mudar. Não descreva
como entregue uma fase que ainda aparece em "Fases ainda pendentes".
