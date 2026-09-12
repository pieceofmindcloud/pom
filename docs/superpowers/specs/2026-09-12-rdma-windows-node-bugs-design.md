# Bugs de lançamento Windows - desenho de investigação

## Objetivo

Investigar e corrigir separadamente três relatos de primeiro acesso no POM:
travamento ao carregar modelo em CPU-only, recusa do perfil `node` ou `infer`
no último passo do setup e ausência de uma rede pública já publicada via
pkarr. Cada item tem uma reprodução automatizada ou uma limitação de
ambiente explicitamente registrada antes da alteração correspondente.

## Decisões de escopo

- O workflow Windows do GitHub Actions não será usado.
- A reprodução deve passar pela fronteira mais próxima do usuário disponível
  nesta máquina: fit e carga do motor para o item 1, UI e handler `/api/setup`
  para o item 2, e consulta/parsing da rede para o item 3.
- Não foi assumida uma causa comum entre os relatos. A investigação confirmou
  uma causa comum apenas para o travamento percebido e o ruído da listagem: o
  polling de 50 ms do socket UDP do Mainline DHT em uma thread de ator própria.
- Cada causa confirmada terá um teste de regressão mínimo que falha antes do
  código de produção ser alterado.
- Se a causa depender obrigatoriamente de Windows real, o código receberá o
  teste portátil possível e a entrega registrará a validação Windows ainda
  necessária, sem disparar CI remoto.

## Fronteiras técnicas

1. `crates/llama-engine/src/llm/config.rs` continua responsável pelo fit e
   carregamento. A reprodução Windows mostrou que o modelo CPU chega ao
   contexto pronto com `n_gpu_layers=0`; portanto não há correção confirmada
   nessa fronteira.
2. `apps/frontend/src/routes/Setup.tsx` decide quando o botão final pode ser
   usado e apresenta erros. `crates/node/src/backend/webui/setup.rs` valida e
   persiste o perfil, trust document, rede e recursos.
3. `crates/node/src/network/pkarr_node.rs` e seus consumidores resolvem o
   registro público; a tela `apps/frontend/src/routes/PublicNetworks.tsx`
   exibe o resultado. A implementação atual consulta pkarr por chave somente
   para candidatos entregues por UDP/libp2p ou cache, não enumera o DHT.
   Parsing, transporte e firewall não serão misturados na mesma hipótese.

## Proposta para o diretório global de redes públicas

O item 3 escolhido pelo captain exige uma fronteira nova, independente da
descoberta P2P direta. A proposta para decisão é um protocolo de registros
assinados, com serviço de diretório replicado:

- O nó publica um anúncio assinado pela chave de autoridade já usada no
  registro pkarr. O anúncio contém identidade da rede, nome público, endpoint(s),
  metadados mínimos, versão do protocolo, `issued_at` e expiração. O diretório
  valida formato, assinatura e limites, mas não vira autoridade de confiança.
- O diretório POM opera uma API HTTPS de escrita e múltiplas réplicas de
  leitura. O cliente recebe uma lista de URLs de bootstrap, consulta as réplicas
  em paralelo, faz união por identidade do registro e rejeita assinatura,
  cluster ou expiração inválidos localmente. Assim, uma réplica fora do ar não
  impede a listagem e uma réplica comprometida não pode forjar uma rede.
- O cliente mantém cache local de anúncios assinados com expiração. A consulta
  inicial não depende de UDP, libp2p, relay, DHT ou conectividade IPv6 do nó
  publicado. Depois da listagem, a conexão de dados ainda exige que algum
  endpoint anunciado seja roteável, por IPv4, Tailscale ou outro transporte
  suportado.
- A publicação usa idempotência por `(authority_pubkey, sequence)` e pode ser
  repetida em todas as réplicas. O armazenamento é um log de anúncios com
  tombstone/expiração, permitindo replicação e auditoria sem guardar segredo de
  usuário. As réplicas podem ser distribuídas por operadores diferentes se o
  protocolo de sincronização for aberto.

Há uma decisão arquitetural real antes de codar: começar com um serviço
replicado operado pela POM, com protocolo extensível para federação futura, ou
definir desde já um diretório descentralizado/federado com regras de descoberta,
replicação e prevenção de spam. A primeira opção reduz a superfície inicial e
evita ponto único de falha por múltiplas réplicas, mas mantém dependência
operacional da POM. A segunda reduz dependência de uma operadora, porém exige
definir consenso, bootstrap, moderação, retenção e identidade de instâncias.
Não será escolhida silenciosamente.

## Critérios de saída

- Cada item tem passos de reprodução e causa raiz anotados no plano de
  implementação.
- Correções confirmadas passam em regressões unitárias ou de integração nesta
  máquina.
- A documentação permanente das pastas alteradas e dos subsistemas afetados
  descreve o comportamento entregue em pt-BR.
- A necessidade de validação Windows real permanece explícita quando não for
  possível simular o ambiente nesta máquina.
- O diretório global permanece sem implementação até a decisão sobre operação
  replicada pela POM versus federação/descentralização.
