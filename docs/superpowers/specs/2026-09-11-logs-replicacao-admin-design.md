# Logs para admins secundários - desenho

## Contexto

O banco `pom.db` é local a cada processo. O log de processo chega hoje a um
único destino escolhido por `profile=admin`, e a rota de leitura de processo
ainda exige que o leitor seja o principal. A auditoria de requisições é criada
somente no gateway que recebeu a chamada. Assim, um admin secundário pode ter
autoridade administrativa válida e ainda assim não possuir dados para as duas
telas.

## Decisão de autoridade

`Config.profile` continua sendo a permissão ampla de uma máquina. A identidade
que distingue principal e secundário é `AdminTrust::effective_authority`,
comparando a autoridade Ed25519 do nó com a cadeia local: o resultado é
`EffectiveAuthority::Primary` ou `EffectiveAuthority::Secondary`. Um peer
anunciado com `profile=admin` mas sem concessão efetiva não recebe dados.

O anúncio continua sendo descoberta e não fonte de autorização. O remetente
seleciona peers anunciados como admin para não depender de o nó de origem
conhecer a cadeia completa de promoções; o receptor confirma a própria
autoridade efetiva antes de persistir. Para localizar o principal da catraca,
`authority_pubkey` anunciado é comparado à chave do principal na cadeia local,
em vez de tratar qualquer admin como principal.

## Transporte e fluxo

- Logs de processo continuam no protocolo libp2p dedicado existente. Cada lote
  é enviado em modo best-effort a todos os peers admin anunciados, e não apenas
  ao primeiro. Um admin com autoridade efetiva ingere o lote no próprio
  `pom.db`; o principal segue recebendo e gravando como antes.
- A auditoria de requisições ganha um protocolo libp2p request/response
  dedicado, com eventos `Begin` e `Complete`. O gateway que cria a entrada
  envia os mesmos eventos a todos os peers admin anunciados. O receptor grava
  somente localmente, sem ecoar a mensagem, e enfileira uma conclusão que tenha
  chegado antes do início para tolerar a ordem de entrega independente do
  libp2p.
- A seleção de destinos exclui o próprio nó por construção da tabela de peers.
  Falhas de um destino não impedem os demais e incrementam o contador local de
  descarte da mesma forma que a fila atual.
- A leitura de ambas as telas exige a autenticação administrativa existente,
  mas não exige `Primary`. Lista vazia continua produzindo a mensagem normal de
  ausência de dados; indisponibilidade do principal não participa da leitura.

## Retenção e custo

Não existe hoje uma política configurável de retenção para `cluster_logs` ou
`audit_requests`. A primeira versão replica o mesmo conteúdo completo para
cada admin efetivo, sem truncamento especial no secundário, porque reduzir
silenciosamente o histórico produziria duas verdades diferentes e impediria
diagnóstico durante a indisponibilidade do principal. O custo aceito e
documentado é aproximadamente N escritas e N vezes o armazenamento para N
admins, além do tráfego de replicação. Uma política de retenção por destino é
uma decisão futura explícita, não um limite escondido nesta mudança.

## Testes e fora de escopo

Os testes verificam fan-out para mais de um admin, rejeição de ingestão por um
admin sem autoridade, leitura por `Secondary` e entrega independente do
principal. Um teste de integração entrega um lote de processo e uma auditoria
de requisição diretamente ao admin secundário, lista os dois no banco local e
não cria nenhum destino principal. Ficam fora de escopo sincronização de
histórico anterior à promoção, retries duráveis e retenção diferenciada.
