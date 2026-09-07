# Consolidar AGENTS.md e TODO.md - Plano de implementação

> **Para agentes:** esta tarefa é documental. Preserve o comportamento do código e use a verificação descrita antes de declarar conclusão.

**Objetivo:** alinhar as fontes `AGENTS.md` ao código atual, concentrar os defeitos ainda existentes em `TODO.md` e regenerar a documentação embutida da interface.

**Abordagem:** reconfirmar pontualmente os itens da auditoria na revisão atual, editar somente as fontes documentais e o gerador se a investigação provar que ele causa a cópia órfã. Em seguida, regenerar `apps/frontend/src/docs/`, comparar cada espelho com sua fonte, validar que nenhum defeito corrigido/obsoleto continua descrito e registrar tudo no relatório externo solicitado.

**Tecnologias:** Markdown, Node.js (`apps/frontend/scripts/build-docs.mjs`), Git, testes Cargo e Vitest.

## Restrições globais

- Não alterar código de runtime, `FIT_MAX_UBATCH` ou o fork vendorizado `llama.cpp`.
- Não editar manualmente cópias em `apps/frontend/src/docs/**`; elas devem sair do gerador.
- Preservar a numeração da auditoria: 18 itens ainda existentes e o item 17 não verificável.
- Manter as decisões dos itens 11 e 12 explícitas como decisões do captain.
- Atualizar o relatório externo em português sem alterar o relatório da auditoria.

## Tarefas

### Tarefa 1: Confirmar fontes e conjunto gerado

**Arquivos:** somente leitura em `AGENTS.md`, fontes dos 25 `AGENTS.md` e `apps/frontend/scripts/build-docs.mjs`.

- [ ] Confirmar na revisão `origin/feat/implantar3` os trechos dos itens 1, 2, 3, 4, 7, 8, 9, 21, 28, 29, 30 e 31.
- [ ] Confirmar a contagem e o mapeamento das fontes e cópias. Se `crates/node/build/AGENTS.md` for órfão produzido por regra genérica, não alterar o gerador e deixar a regeneração removê-lo.

### Tarefa 2: Consolidar fontes e backlog

**Arquivos:** fontes `AGENTS.md` que contêm os itens auditados e `TODO.md`.

- [ ] Criar `TODO.md` com cabeçalho de origem, data e SHA auditado, agrupando Alta, Média, Baixa e Não verificável.
- [ ] Incluir os itens 5, 6, 10, 11, 12, 13, 14, 15, 16, 18, 19, 20, 22, 23, 24, 25, 26 e 27, além do 17, cada um com checkbox, fonte:linha, efeito, afetados e esforço.
- [ ] Reescrever as fontes removendo os 9 itens corrigidos e 3 obsoletos; preservar invariantes úteis e apontar pendências para `TODO.md` sem duplicar listas.
- [ ] Corrigir o teto documental do microlote para 512 e registrar que ele governa os buffers de cálculo do contexto principal e MTP, com custo de VRAM maior que 128.
- [ ] Remover referências a bitswap e consolidar a explicação atual de `split_simple`/`kv_unified`.
- [ ] Corrigir `scripts/AGENTS.md` para refletir que o Dockerfile, e não `build-docker.sh`, executa o build da UI.

### Tarefa 3: Regenerar e validar espelhos

**Arquivos:** `apps/frontend/src/docs/**` gerados por `apps/frontend/scripts/build-docs.mjs`.

- [ ] Executar o gerador a partir de `apps/frontend`.
- [ ] Confirmar que as 24 cópias geradas correspondem às 24 fontes e que não existe a cópia órfã de `crates/node/build/AGENTS.md`.

### Tarefa 4: Relatar, testar e publicar

**Arquivos:** `/Users/jairpereira/Sites/firstmate/data/rdma-consolidar-agents-md/report.md` e o commit da branch.

- [ ] Escrever relatório item a item das remoções e alterações, incluindo a decisão sobre a cópia órfã e a ausência de alterações de comportamento.
- [ ] Rodar validações documentais e a suíte relevante sem usar testes UDP perigosos na LAN.
- [ ] Revisar `git diff`, integrar commits novos de `origin/feat/implantar3` com `git pull --rebase`, criar commit sem coautor automático e publicar na linha determinada pelo captain.
