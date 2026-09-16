# Contrato rico de heartbeat Enterprise - Plano de implementação

> **Para agentes de implementação:** execute este plano por tarefa, usando TDD e verificações recentes antes de afirmar conclusão.

**Objetivo:** Fazer o cliente Enterprise desserializar e persistir a resposta rica de heartbeat emitida pelo servidor de licenças.

**Arquitetura:** O servidor permanece a autoridade do contrato. O cliente adicionará um DTO local para o payload de renovação, com os campos que o servidor já publica, e atualizará o estado persistido somente com esses valores autoritativos. O teste HTTP usará um stub local com o mesmo envelope e payload do servidor para proteger a fronteira de desserialização sem depender da rede.

**Stack:** Rust, Tokio, Axum, Reqwest, Serde e `enterprise-protocol`.

**Especificação:** `docs/superpowers/specs/2026-09-15-enterprise-complete-design.md`, complementada pelo incidente de produção descrito na tarefa.

## Restrições globais

- Não alterar `pom-website` nem qualquer contrato do servidor.
- O cliente deve consumir `lease_token`, `heartbeat_due_at`, `entitlements`, `plugin_release_id`, `release_changed` e `available_release_id` do heartbeat.
- Não substituir o prazo do servidor por `agora + 24h`.
- A resposta deve continuar dentro de `ResponseEnvelope<T>`.
- Atualizar a documentação da pasta tocada no mesmo commit.

### Tarefa 1: DTO e consumo do heartbeat Enterprise

**Arquivos:**
- Modificar: `crates/node/src/enterprise.rs`, trait e implementação HTTP `LicenseService`, `EnterpriseState::heartbeat`, mock e testes.
- Modificar: `crates/node/AGENTS.md`, somente se o mapa precisar registrar conhecimento durável novo.
- Modificar: `docs/AGENTS.md`, índice deste plano.

**Interfaces:**
- Produz: `HeartbeatResponse` local serializável com `license_id`, `activation_id`, `lease_id`, `lease_token`, `entitlements`, `lease_expires_at`, `heartbeat_due_at`, `grace_seconds`, `plugin_release_id`, `release_changed`, `available_release_id` opcional e `grant`.
- Produz: `LicenseService::heartbeat(...) -> Result<ResponseEnvelope<HeartbeatResponse>, LicenseClientError>`.

- [x] Escrever o teste HTTP que retorna um envelope com payload objeto e exige que `HttpLicenseService::heartbeat` aceite a resposta.
- [x] Executar o teste antes da implementação e registrar a falha `invalid type: map, expected unit`.
- [x] Implementar o DTO local, trocar a assinatura do trait e do cliente HTTP, e atualizar o mock com os campos reais.
- [x] Atualizar `EnterpriseState::heartbeat` para copiar os dados retornados para o estado persistido, derivar `features` de `entitlements` e manter o estado do plugin coerente com o release indicado, sem inventar prazo local.
- [x] Executar o teste de regressão e os testes Enterprise do crate `node`.
- [x] Executar `cargo fmt --all -- --check` e revisar diff, documentação e ausência de mudanças no servidor.
- [ ] Fazer commit, rebase sobre `origin/feat/enterprise`, publicar com `git push origin HEAD:feat/enterprise`, confirmar o SHA em `git ls-remote` e registrar o estado `done`.
