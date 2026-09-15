# Descoberta local segura no primeiro acesso

## Objetivo

Permitir que um nó novo encontre um administrador na LAN durante o primeiro
acesso e receba automaticamente o documento de confiança e a chave de conexão,
sem copiar e colar segredos. O mesmo mecanismo poderá ser ligado depois pelo
administrador em Preferências, mas o estado persistido fora do wizard começa
desligado.

## Decisões de arquitetura

### Reutilizar o UDP nativo

O projeto já usa o socket UDP da `Network` para broadcast em
`network::DISCOVERY_PORT` e já possui `P2PMessage::LocalDiscover`. A nova
mensagem será uma variante desse mesmo envelope. Isso evita um segundo daemon,
uma segunda política de portas e uma divergência entre discovery do setup e do
runtime. O modo setup, que ainda não cria `Network`, usará uma instância
equivalente com um socket próprio no `p2p_port` configurado.

### Handshake em três mensagens

1. O nó envia um `Probe` broadcast com um request id, nonce e chave pública
   efêmera X25519. O probe não contém trust, chave de conexão ou credencial.
2. Um administrador só responde quando o modo está ativamente habilitado. A
   resposta `Offer` leva o documento de confiança, seu fingerprint, um nonce e
   a chave pública efêmera do administrador. O documento é público e pode ser
   lido por qualquer observador; a chave de conexão não aparece na oferta.
3. O nó verifica o documento, envia `Proof` com os dois nonces e as duas
   chaves públicas, e o administrador só então cifra o payload final com
   X25519 + ChaCha20-Poly1305. O payload cifrado contém o trust e a chave de
   conexão. O nonce da sessão, o request id e o transcript completo entram no
   AAD, evitando aceitar uma resposta de outra sessão.

O documento assinado é a autenticação da identidade da rede. Antes de receber
esse documento, um nó ainda não tem uma identidade confiável própria para
assinar um desafio. Por isso a propriedade de segurança exigida nesta etapa é
que nenhum observador passivo receba a chave: só uma parte que gerou a chave
efêmera, recebeu a oferta e enviou a prova consegue decifrar a resposta.
Sessions expiram rapidamente e request ids são consumidos uma vez.

### Papéis e ciclo de vida

- Admin: o estágio novo fica depois do convite, inicia o listener ao abrir e
  espera nós locais. O serviço transitório do wizard desaparece ao salvar; o
  runtime começa desligado.
- Node/infer: o estágio novo aparece antes da tela de trust, procura um admin
  local e preenche trust e chave quando encontra. A tela manual continua como
  fallback explícito quando não houver admin na LAN.
- Runtime: `POST /api/ui/network/auto-discovery` liga ou desliga o listener do
  administrador. A rota é `AdminOnly`, grava `Config.local_auto_discovery` e
  atualiza a instância UDP em memória. O GET expõe estado e porta, sem expor a
  chave de conexão.

## Contratos

O protocolo serializado usa `P2PMessage::LocalAutoDiscovery` com uma carga
tipada. Campos de segredo ficam ausentes de `Probe`, `Offer` e `Proof`; apenas
`Secret.ciphertext` pode transportar a chave, e somente após a prova aceita.
O serviço expõe:

- `LocalDiscoveryRuntime::set_admin_material(enabled, trust, key)`;
- `LocalDiscoveryRuntime::begin_search()`;
- `LocalDiscoveryRuntime::status()`;
- `LocalDiscoveryRuntime::handle(message, source)`.

O setup terá GET `/api/setup/auto-discovery/status`, POST
`/api/setup/auto-discovery/start` para o admin e POST
`/api/setup/auto-discovery/search` para node/infer. O start recebe o trust e a
chave apenas em memória antes do save, e nunca grava a chave em log ou resposta
de status.

## Falhas e limites

Probes sem versão, nonces inválidos, transcript alterado, request desconhecido
ou sessão expirada são ignorados. Documento acima do limite de transporte é
recusado antes de entrar no anúncio. A busca expira em 20 segundos e não impede
o caminho manual. Desligar o toggle apaga material administrativo e sessions
pendentes.

## Cobertura de testes

O módulo de protocolo terá testes de serialização sem vazamento, handshake
completo, prova ausente ou alterada, replay e expiração. A API terá testes de
default desligado e de troca de estado. O wizard terá testes do novo estágio,
preenchimento automático e ausência da chave pública separada. A tela de
Preferências terá teste da rota de toggle.
