# Modelo de confiança do administrador

Registro da implementação da cadeia de autoridade administrativa do rdma-p2p,
conforme o relatório `data/rdma-admin-chave-publica/report.md` e as decisões do
captain. O código entregue está em `crates/node/src/admin_trust.rs`.

## Objetivo e limites

- A autoridade é a chave Ed25519 do próprio peer libp2p do administrador.
- A raiz é uma impressão digital conferida manualmente e fica fora de
  `pom.json` e de `profile`.
- A concessão fica no arquivo dedicado `config::admin_trust_path()`, com a
  âncora, a cadeia completa, despromoções, sucessão e replay persistido.
- O principal é único. Secundários administram, mas não promovem e não
  delegam. A sucessão usa oferta assinada do principal e aceitação assinada do
  último secundário promovido que esteja ativo.
- `profile: admin` sem uma concessão válida não produz capacidade administrativa.
- O controle é assinado; frames de inferência não recebem assinatura Ed25519
  por frame. O transporte libp2p autentica o peer e as rotas de inferência
  continuam usando a autenticação de transporte existente.
- O `role` de configuração/topologia foi removido. Os campos `role` de TP,
  mensagens e acessibilidade permanecem porque são contratos diferentes.

## Implementação

### Núcleo da cadeia

`AdminTrust` valida JSON canônico e assinaturas para `AdminGrant`,
`AdminDemotion`, `AdminTransferOffer`, `AdminTransferAcceptance`,
`NodePromotionRequest` e `NodePromotionCommand`. A gravação usa arquivo
temporário, renomeação atômica e modo 0600 em Unix.

`nonce`, `command_id`, `offer_id` e época entram no registro de replay e são
consumidos antes da mutação. `reconcile` valida o documento inteiro desde a
âncora, inclusive a cadeia de concessões, em vez de confiar em um checkpoint.
Oferta pendente e aceitação isolada não mudam o principal.

O bootstrap de produção exige uma `TrustAnchor` manual. Se o arquivo não existe
ou é inválido, o processo mantém o perfil efetivo sem capacidade administrativa
e registra o diagnóstico. Não há bootstrap por gossip, UDP, pkarr, HTTP ou pelo
próprio `profile`.

### Gateway e rede

- `GatewayState` carrega a cadeia durável e a chave do peer libp2p local.
- `require_admin_for_surface` consulta autoridade efetiva, não apenas o texto
  de `profile`.
- Promoção de secundário, despromoção, resignação e sucessão usam concessões e
  comandos assinados, com entrega da cadeia completa ao alvo.
- Infer para node é pedido assinado autorizado por qualquer admin. Node para
  infer é decisão local com aviso ao administrador.
- `Announce` publica `profile`, `authority_pubkey` e `libp2p_peer_id`. O
  gossipsub é assinado e rejeita anúncio cujo `PeerId` declarado não coincide
  com a origem autenticada do transporte.
- A exceção de chamadas autenticadas nó-a-nó atravessa o perfil local do
  receptor somente depois de validar `InternalNodeAuth`; o navegador continua
  sujeito ao perfil local.

### Contrato das telas

As telas usam:

- `GET /api/ui/network/admin-authority` para a vista do principal, secundários,
  cadeia e permissões;
- `POST /admin/nodes/{id}/promote-admin`, `POST /admin/authority/resign` e
  `POST /admin/authority/secondaries/{id}/demote` para mutações do principal;
- `GET /api/transfer/offers` e `POST /api/transfer/offers/{id}/accept` para a
  oferta de sucessão no sino. `reject` é recusado para essa oferta, pois ela só
  termina com aceitação assinada ou avanço de época;
- `POST /api/ui/network/request-node` e
  `POST /api/ui/network/relinquish-node` para as mudanças de perfil de infer e
  node.

## Testes de aceitação

Os testes unitários de `admin_trust.rs` cobrem perfil administrativo sem
concessão, concessão forjada por secundário, replay depois de reload, cadeia
reconstruída em nó offline e as duas metades da transferência. Os testes do
gateway cobrem o bloqueio de `profile=admin` sem cadeia e o fan-out autenticado
entre nós.

O caminho de inferência não foi alterado para assinar frames. A assinatura fica
no plano de controle; o transporte continua sendo a autenticação dos frames.

## Verificação e entrega

- [x] Código, rotas e cadeia publicados em `origin/feat/implantar3`.
- [x] Documentação dos subsistemas atualizada após o rebase da linha viva.
- [ ] `cargo check` e os testes de compilação finais devem ser registrados no
  relatório de entrega junto dos avisos e falhas preexistentes da linha.
