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
   `crates/node/src/network/public_directory.rs` implementa o novo protocolo
   de registros federados. Parsing, transporte e firewall não serão misturados
   na mesma hipótese.

## Protocolo federado para o diretório global de redes públicas

O captain escolheu federação desde a primeira versão. Nenhuma réplica é uma
autoridade POM e nenhuma delas é obrigatória para a segurança ou para a
conectividade P2P. As réplicas são ingressos de transporte que podem ser
operados por organizações diferentes.

### Registro e consenso

- Cada publicação é um registro completo assinado por Ed25519 pela autoridade
  do nó publicador. A chave, `cluster_id`, fingerprint da âncora, documento de
  confiança, endpoint, localização e ingressos conhecidos ficam dentro da
  carga assinada. O `discovered_via` é anotação local e não entra na assinatura.
- A identidade lógica de uma versão é `(publisher_pubkey, sequence)`. Para o
  mesmo publicador, a maior sequência vence. Um conflito artificial na mesma
  sequência é resolvido pelo menor hash da carga assinada, de forma
  determinística. Réplicas e clientes fazem merge idempotente e podam versões
  vencidas.
- Esse é o consenso escolhido para o diretório v1: convergência eventual por
  merge determinístico, sem líder, quorum ou bloqueio de publicação. A
  assinatura impede falsificação e a regra de sequência impede que uma cópia
  atrasada regrida o anúncio. Não há consenso BFT nem eleição de réplicas nesta
  versão, porque uma réplica nunca autoriza a rede nem participa da admissão.
- Um nó publica em todos os ingressos configurados e consulta todos em
  paralelo. O cache local também é mesclado. Assim, réplicas que não se veem
  diretamente convergem quando publicadores ou leitores carregam registros
  entre elas, sem ponto único de falha.

### Bootstrap e disponibilidade

- O operador informa uma lista de URLs HTTP(S) de ingressos no wizard, separada
  por vírgulas, ou pela variável `POM_PUBLIC_DIRECTORY` em instalação sem UI.
  A lista é deduplicada e limitada a 16 URLs. Os registros carregam a lista de
  ingressos que o publicador conhece para que um join possa conservar o
  bootstrap federado.
- Um nó privado pode consultar o diretório por conexões de saída HTTP(S), mesmo
  continuando privado. Isso exige apenas que a política de rede local permita
  saída até pelo menos uma réplica. Não há exigência de IPv6 público, porta de
  entrada, relay ou descoberta P2P para listar redes públicas.
- O nó privado não publica registros e, para preservar isolamento, sua tela de
  redes públicas não usa candidatos UDP/libp2p/pkarr da rede privada. Ela usa
  apenas o cache assinado e os ingressos configurados.
- A conexão de dados depois da listagem é outra etapa: o endpoint anunciado
  ainda precisa ser alcançável por IPv4, Tailscale ou outro transporte
  suportado. O diretório não promete que um endereço IPv6 anunciado tenha rota.

### Respostas explícitas às perguntas de segurança

- Um nó privado pode listar redes públicas. O mínimo é saída HTTP(S) para uma
  réplica configurada, ou um cache local ainda vigente. Nenhuma conexão de
  entrada é necessária.
- O v1 não mantém rede privada e rede pública simultaneamente no mesmo
  processo. O usuário pode consultar a lista enquanto está privado, mas o join
  chama o fluxo de saída da rede atual e substitui a configuração antes de
  entrar na pública. Essa é uma escolha deliberada de isolamento, não uma
  limitação escondida do wizard.
- A rede pública nunca recebe a chave, os bootstrap addresses ou anúncios da
  rede privada. O diretório carrega apenas registros que o publicador escolheu
  assinar. O caminho de consulta usa HTTP(S) de metadados e não injeta os
  endereços retornados em um swarm privado. Como só existe uma rede P2P ativa,
  não há uma interface pública paralela para encaminhar até a privada. O
  firewall continua sendo defesa adicional, não a única barreira.

### Anti-spam, retenção e segurança

- Toda carga exige assinatura Ed25519 válida e uma prova de trabalho de 16 bits
  sobre a carga completa. O custo é pago pelo publicador, não por quem lista.
  O servidor também limita tamanho dos campos e aceita no máximo uma versão
  vigente por publicador.
- Ao aceitar ou servir um registro, a réplica confere a cadeia de confiança,
  `cluster_id` e fingerprint. Uma assinatura válida nunca transforma uma raiz
  de outro cluster em raiz local.
- O TTL máximo é de 7 dias. Registros expirados ou mais antigos que 7 dias são
  removidos do cache, e cada cache tem limite de 1024 registros. Isso define a
  política de retenção e limita crescimento e replay sem criar tombstones
  permanentes.

## Critérios de saída

- Cada item tem passos de reprodução e causa raiz anotados no plano de
  implementação.
- Correções confirmadas passam em regressões unitárias ou de integração nesta
  máquina.
- A documentação permanente das pastas alteradas e dos subsistemas afetados
  descreve o comportamento entregue em pt-BR.
- A necessidade de validação Windows real permanece explícita quando não for
  possível simular o ambiente nesta máquina.
- O diretório global implementado segue as escolhas federadas acima. Não há
  dependência de um serviço central da POM.
