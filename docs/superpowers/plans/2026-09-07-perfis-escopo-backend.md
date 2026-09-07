# Perfis e escopo no backend - Plano de Implementação

> **Para trabalhadores agenticos:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans para implementar este plano task-by-task. Os passos usam checkbox (`- [ ]`) para acompanhamento.

**Objetivo:** Aplicar no gateway o escopo de `infer`, `node` e `admin` derivado da decisão do captain, com recusas HTTP explícitas e uma organização do `webui` por perfil.

**Arquitetura:** O router do `webui` será dividido em `routes/common.rs`, `routes/infer.rs`, `routes/node.rs` e `routes/admin.rs`. `routes/mod.rs` manterá a tabela única de regras de acesso e montará os quatro grupos, enquanto o middleware do gateway aplicará a regra usando o `profile` local do `pom.json`. Chamadas nó-a-nó autenticadas pelo marcador interno e pela `connection_key` serão uma exceção explícita de identidade interna, sem aceitar perfil em cabeçalho.

**Stack:** Rust, Axum, Tokio, testes `tower::ServiceExt` e `cargo test`.

**Spec:** `/Users/jairpereira/Sites/firstmate/data/rdma-perfis-escopo-backend/decisao-escopo-rotas.md`

## Restrições globais

- O perfil usado na decisão vem de `Config.profile`, nunca de um cabeçalho do cliente.
- A recusa de perfil será JSON HTTP 403 com erro nomeado, sem resposta vazia.
- Setup (`/api/setup/key` e `/api/setup/save`) fica fora do escopo e será marcado explicitamente.
- O servidor de inferência na porta 8181 fica fora do escopo e será marcado explicitamente.
- A tabela de rotas será dado centralizado e legível; a divisão dos arquivos não será a única barreira.
- Não alterar comportamento de handlers durante a movimentação.
- Atualizar `crates/node/src/backend/AGENTS.md` e `docs/subsystems/api-http.md` no mesmo commit da mudança correspondente; não editar arquivos gerados.
- Fazer commits separados para reorganização e autorização.

---

### Tarefa 1: Reorganizar o registro do `webui` por perfil

**Arquivos:**
- Criar: `crates/node/src/backend/webui/routes/mod.rs`
- Criar: `crates/node/src/backend/webui/routes/common.rs`
- Criar: `crates/node/src/backend/webui/routes/infer.rs`
- Criar: `crates/node/src/backend/webui/routes/node.rs`
- Criar: `crates/node/src/backend/webui/routes/admin.rs`
- Remover: `crates/node/src/backend/webui/routes.rs`
- Modificar: `crates/node/src/backend/webui/mod.rs`
- Testar: testes existentes em `crates/node/src/backend/webui/tests/`

**Grupos de registro:**
- `common.rs`: `/api/ui/node`, `/api/ui/cluster`, `/api/ui/cluster/models`, `/api/ui/peer/:node_id/runtime`, `/api/ui/network/rtt`, `/api/ui/chat/telemetry`, `/api/ui/chat/history` e `/:id`.
- `infer.rs`: `/api/ui/network/promote`.
- `node.rs`: catálogo local, caminhos, temperatura, cache types, endereço anunciado, planejamento/fatiamento, provider remoto e jobs.
- `admin.rs`: acesso administrativo, auditoria, logs de cluster, TLS/logs do cluster, implantação, ciclo de vida, desenvolvimento e `/api/ui/chat-key` quando a decisão de autorização for aplicada.

- [ ] Criar os quatro routers sem duplicar handlers.
- [ ] Fazer `webui::routes()` apenas mesclar `common`, `infer`, `node` e `admin`.
- [ ] Rodar `cargo test -p node webui` e comparar o diff para garantir somente movimentação.
- [ ] Atualizar a documentação do backend descrevendo a nova árvore.
- [ ] Commitar somente a reorganização e sua documentação com `refactor(webui): organizar rotas por perfil`.

### Tarefa 2: Definir o dado único de escopo

**Arquivos:**
- Modificar: `crates/node/src/backend/webui/routes/mod.rs`
- Testar: testes unitários no mesmo módulo

**Contrato:** `profile_scope(path)` retornará uma regra explícita para os quatro grupos: `AdminOnly`, `NodeAdmin`, `InferNodeAdmin` e `CommonOrInternal`. A tabela deve usar o casamento por segmento/prefixo mais específico e manter os dois grupos fora do escopo como comentários de código.

- [ ] Escrever testes que classifiquem uma rota de cada grupo e uma rota desconhecida.
- [ ] Rodar os testes para observar a falha antes de implementar a tabela.
- [ ] Implementar a tabela com todos os caminhos do inventário, incluindo aliases e OPTIONS pelo mesmo caminho.
- [ ] Rodar os testes unitários novamente e garantir que a decisão é independente de headers.

### Tarefa 3: Aplicar a regra no caminho HTTP único

**Arquivos:**
- Modificar: `crates/node/src/backend/gateway/state.rs`
- Modificar: `crates/node/src/backend/gateway/http.rs` somente se a ordem do middleware exigir
- Modificar: `crates/node/src/backend/auth.rs` somente para reutilizar a prova interna existente sem duplicar autenticação
- Testar: `crates/node/src/backend/gateway/tests.rs`

- [ ] Escrever testes E2E de router montado com `GatewayState`: para cada perfil, uma rota permitida e uma rota recusada; incluir infer recusando `/admin/nodes` com a `connection_key` e recebendo 403 JSON nomeado.
- [ ] Rodar os testes para confirmar a falha atual, antes do middleware de perfil.
- [ ] Implementar o middleware central depois da autenticação de transporte, lendo somente `state.auth.profile`.
- [ ] Permitir identidade interna somente após validar `connection_key` e marcador interno, mantendo os handlers responsáveis por sua autorização operacional.
- [ ] Garantir que uma recusa não executa o handler e sempre retorna JSON 403.
- [ ] Rodar os testes E2E e os testes existentes de autenticação.

### Tarefa 4: Reconciliar handlers e contratos

**Arquivos:**
- Modificar apenas se necessário: `crates/node/src/backend/webui/*.rs`, `crates/node/src/backend/admin.rs`, `crates/node/src/backend/transfer.rs`
- Testar: testes de `webui`, `deploy`, `gateway`, `admin` e `transfer`

- [ ] Verificar que callbacks e fan-out internos continuam usando a exceção de identidade interna.
- [ ] Verificar que `/api/ui/network/promote` mantém a política operacional existente e que o mapa não a transforma em promoção de outro nó.
- [ ] Verificar que setup e inferência 8181 continuam sem a nova barreira, com comentários explícitos no código.
- [ ] Registrar qualquer defeito preexistente no `done:` sem correção lateral.

### Tarefa 5: Validar, documentar e publicar

**Arquivos:**
- Modificar: `crates/node/src/backend/AGENTS.md`
- Modificar: `docs/subsystems/api-http.md`

- [ ] Rodar `cargo check`.
- [ ] Rodar `cargo clippy --all-targets --all-features -- -D warnings` ou registrar falhas preexistentes.
- [ ] Rodar a suíte Rust relevante e `cargo test --workspace` se o custo permitir.
- [ ] Rodar `git diff --check` e verificar que a branch contém apenas mudanças da tarefa.
- [ ] Fazer `git pull --rebase` sobre `origin/feat/implantar3` antes de publicar.
- [ ] Publicar os commits coerentes em `origin/feat/implantar3`, sem `--force`.
- [ ] Registrar `done:` com os hashes dos commits e a linha publicada.
