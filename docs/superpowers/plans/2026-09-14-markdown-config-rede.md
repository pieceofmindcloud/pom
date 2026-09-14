# Markdown na configuração e no diretório público - Plano de implementação

> **Para agentes:** use TDD e verificação antes da conclusão ao executar este plano.

**Objetivo:** permitir que os metadados textuais da rede sejam escritos em Markdown, exibidos formatados e consultados em um diálogo completo no diretório público.

**Arquitetura:** manter `NetworkSettings` com os nomes atuais (`description`, `rules` e `channels`), adicionar um campo compartilhado de edição com preview baseado no componente `Markdown` existente e transportar `rules`/`channels` junto dos registros públicos assinados. A lista pública abre o `Modal` compartilhado e conserva a confirmação de entrada como ação separada.

**Tecnologias:** React 18, TypeScript, Vitest + Testing Library, `react-markdown` por meio de `Markdown.tsx`, Rust/Serde para o contrato do diretório federado.

**Restrições globais:** não adicionar dependências; não editar arquivos gerados; manter textos no i18n; preservar o contrato de segurança do documento assinado; usar hífen em documentação e mensagens de commit.

---

### Tarefa 1: Cobrir o comportamento desejado

**Arquivos:**
- Modificar: `apps/frontend/src/routes/NetworkSettings.test.tsx`
- Modificar: `apps/frontend/src/routes/PublicNetworks.test.tsx`
- Modificar: `crates/node/src/backend/gateway/tests.rs`

- [x] Escrever testes que encontrem textareas Markdown amplas, exibam preview formatado, renderizem leitura somente leitura e abram o diálogo público com descrição, regras e canais.
- [x] Rodar os testes direcionados e confirmar falhas causadas pela ausência do comportamento.

### Tarefa 2: Implementar editor e leitura Markdown

**Arquivos:**
- Criar: `apps/frontend/src/components/MarkdownField.tsx`
- Modificar: `apps/frontend/src/routes/NetworkSettings.tsx`

- [x] Usar textarea com área confortável, prévia ao vivo e o componente `Markdown` no preview.
- [x] Aplicar o campo a `description`, `rules` e `channels` sem renomear o contrato.
- [x] Usar `Markdown` em todos os valores somente leitura e manter fallback de valor ausente.

### Tarefa 3: Ampliar o contrato público e abrir detalhes

**Arquivos:**
- Modificar: `crates/node/src/network/public_directory.rs`
- Modificar: `crates/node/src/backend/gateway/http.rs`
- Modificar: `crates/node/src/backend/gateway/tests.rs`
- Modificar: `apps/frontend/src/routes/PublicNetworks.tsx`
- Modificar: `apps/frontend/src/i18n/messages.csv`

- [x] Incluir `rules` e `channels` no registro público assinado e em suas conversões local/diretório.
- [x] Renderizar uma linha clicável e um `Modal` com todas as informações públicas, reutilizando `Markdown`.
- [x] Manter o botão de entrada e a confirmação sem que o clique no botão abra o detalhe acidentalmente.

### Tarefa 4: Documentação e verificação

**Arquivos:**
- Modificar: `apps/frontend/src/routes/AGENTS.md`
- Modificar: `apps/frontend/src/components/AGENTS.md`
- Modificar: `crates/node/src/network/AGENTS.md`
- Modificar: `crates/node/src/backend/AGENTS.md`

- [x] Documentar o comportamento atual nos AGENTS das pastas tocadas.
- [x] Rodar testes da UI, build da UI e testes Rust relevantes.
- [x] Subir o frontend local e conferir a tela `/admin-ui/config/rede` e o diálogo em Chrome DevTools.

### Tarefa 5: Entrega

- [ ] Fazer `git fetch` e `git pull --rebase origin feat/implantar3` antes do commit final.
- [ ] Comitar a branch `fm/rdma-markdown-config-rede`.
- [ ] Publicar conforme a linha de entrega fornecida e confirmar o SHA remoto antes de relatar `done`.
