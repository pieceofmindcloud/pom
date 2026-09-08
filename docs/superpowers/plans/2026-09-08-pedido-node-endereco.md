# Pedido de promoção infer -> node - Plano de implementação

> **Para trabalhadores agentivos:** SUB-SKILL OBRIGATÓRIO: use `superpowers:test-driven-development` e `superpowers:verification-before-completion` durante a execução. Cada passo deve ser acompanhado por um teste observável.

**Objetivo:** Fazer o pedido autorizado de `infer` para `node` atravessar ida e volta quando o primeiro endereço anunciado do peer estiver morto, mantendo recusas explícitas.

**Evidência do captain:** no Mac a ida chega ao admin e a volta falha no
`accept-node`; no Salem, dentro do Docker, a ida nem chega ao principal. As
duas pernas precisam, portanto, tentar o conjunto de endereços anunciado. A
redetecção do endereço em cada novo anúncio é uma tarefa paralela
(`rdma-redetectar-endereco-anunciado`); este trabalho trata o cache recebido
como descartável e não absorve a mudança no produtor do anúncio.

**Arquitetura:** O fluxo continua sendo iniciado no `infer`, autenticado pela `connection_key` e autorizado pela cadeia Ed25519 do admin. O transporte HTTP da promoção passa a tentar os candidatos de gateway do peer, na ordem já publicada por `PeerInfo::gateway_candidates`, em ambas as direções. O testkit levanta dois routers locais e verifica pedido, autorização, aplicação do perfil e recusa.

**Tech Stack:** Rust 2021, Axum, Tokio, reqwest, `Network::testkit`, testes unitários assíncronos.

**Spec:** `docs/subsystems/api-http.md` e `docs/subsystems/rede-p2p.md`, contratos atuais das rotas internas e dos candidatos de endereço.

## Restrições globais

- O pedido interno usa a `connection_key` compartilhada e `x-pom-internal-node`.
- O perfil persistido continua sendo propriedade do nó; a interface não escolhe perfil.
- Nenhum fallback para protocolo antigo será introduzido.
- Endereços anunciados são dados do peer; a tentativa deve nomear a falha e não cair em silêncio.
- O teste não usa UDP real nem altera `llama.cpp`.

---

### Tarefa 1: Reproduzir a falha e provar as duas metades

**Arquivos:**
- Teste temporário e depois permanente: `crates/node/src/backend/admin.rs`, módulo `tests`.
- Apoio existente: `crates/node/src/backend/gateway/tests.rs` (`testkit`), `crates/node/src/network/network.rs` (`PeerInfo::gateway_candidates`).

**Interfaces:**
- Consome as rotas `/api/ui/network/request-node`, `/api/internal/profile/authorize-node` e `/api/internal/profile/accept-node`.
- Produz evidência separada de que o admin recebe a ida e de que o endereço primário morto bloqueia a volta antes da correção.

- [x] **Passo 1: Montar dois estados de nó e dois listeners loopback**, com o admin ancorado e o solicitante em `infer`.
- [x] **Passo 2: Anunciar no solicitante um endereço morto em `addr` e o listener vivo em `addr_candidates`**, mantendo a porta de gateway correta.
- [x] **Passo 3: Enviar o pedido pelo endpoint do solicitante e observar o admin**, confirmando que a ida chega e que a falha ocorre ao tentar `accept-node`.
- [x] **Passo 4: Executar o teste antes do conserto**, esperando falha de conexão na volta.

### Tarefa 2: Fixar o fallback e os contratos de recusa

**Arquivos:**
- Modificar: `crates/node/src/backend/admin.rs`.
- Testar: `crates/node/src/backend/admin.rs`, módulo `tests`.
- Documentar: `crates/node/src/backend/AGENTS.md`, `docs/subsystems/api-http.md`, `docs/subsystems/rede-p2p.md`.

**Interfaces:**
- Produz a tentativa de todas as URLs em `gateway_urls(peer)` para as duas pernas da promoção.
- Mantém respostas HTTP do peer como recusas distintas de erro de conexão.

- [x] **Passo 1: Escrever o teste vermelho ponta a ponta**, incluindo sucesso com endereço primário morto e recusa informada ao solicitante.
- [x] **Passo 2: Rodar o teste e confirmar que falha somente por usar o primeiro endereço morto.**
- [x] **Passo 3: Implementar o menor ajuste**, substituindo o uso único de `gateway_url()` por tentativa ordenada de `gateway_urls(peer)` nas duas pernas.
- [x] **Passo 4: Rodar o mesmo teste e confirmar que ida, autorização, aplicação e recusa passam.**
- [x] **Passo 5: Corrigir o orçamento de timeout da ida**, para uma tentativa morta na volta não fazer o solicitante reenviar a mesma mutação assinada e gerar falso replay.
- [ ] **Passo 5: Atualizar as instruções e os contratos documentados**, incluindo que `/api/internal/*` é servido em todo perfil quando a chamada é autenticada entre nós e que promoção tenta candidatos.

### Tarefa 3: Validação e entrega

**Arquivos:**
- Nenhum arquivo novo além dos testes, implementação e documentação acima.

- [x] **Passo 1: Rodar os testes focados e o conjunto proporcional do crate.** Os dois testes da promoção passam; os nove vermelhos completos são o baseline preexistente declarado pelo captain.
- [x] **Passo 2: Rodar `cargo check -p node` e `cargo clippy -p node --all-targets -- -D warnings`**, separando avisos e falhas preexistentes claramente. `cargo check` passa; o clippy estrito para em diagnósticos preexistentes de `llama-engine` e do crate `node`, fora dos arquivos tocados.
- [ ] **Passo 3: Fazer `git pull --rebase` sobre `origin/feat/implantar3` antes da publicação**, parando se houver conflito na tabela de escopo.
- [ ] **Passo 4: Commitar somente estado coerente e reportar o commit e a linha.**
