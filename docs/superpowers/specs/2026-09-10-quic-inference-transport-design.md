# Transporte QUIC opcional para hops de inferência

## Contexto e diagnóstico

O hop de inferência usa a porta de inferência anunciada pelo peer. Quando a
política TLS está desligada, `peer_client::peer_base_url` já constrói
`http://host:porta`, portanto a camada HTTP fica em HTTP/1.1 puro sobre TCP.
Quando TLS está ligado, o mesmo caminho usa `https://` e o cliente pinado
confere o certificado contra o `node_id`.

Há uma divergência de observabilidade: o caminho direto sempre classificava o
resultado como `https`, e o canal persistente como `https-link`, mesmo quando o
URL era `http`. A mudança corrige a evidência para `http` ou `https` conforme a
travessia real. O túnel libp2p continua sendo uma alternativa separada, pois
seu Noise não é TLS de aplicação.

## Decisão de arquitetura

Adicionar `quic_enabled` à configuração do nó, com `false` como default
serde, e expor a decisão no mesmo padrão de `tls_required`:

- o principal grava a política e a aplica imediatamente;
- `quic_policy` usa o booleano do Announce somente como gatilho e confirma o
  valor por consulta autenticada ao principal;
- peers exibem o valor e a autoridade, mas não podem alterá-lo;
- mudança de política não fecha clientes ou canais em andamento. Novas
  travessias leem o snapshot atual; referências já adquiridas continuam vivas.

O servidor de inferência e o gateway abrem endpoints QUIC UDP na mesma porta
numérica dos respectivos listeners TCP. A camada HTTP/3 será implementada com
`quinn`, `h3` e `h3-quinn`, usando TLS 1.3 embutido no QUIC. O certificado é o certificado de
identidade do nó já usado pelo TLS entre peers, e a verificação do cliente
continua pinada por `node_id`.

O cliente mantém um endpoint QUIC por `PeerClients` e uma conexão HTTP/3
reutilizável por peer. Lotes de inferência e entregas de retorno usam streams
HTTP/3 independentes na mesma conexão; quando QUIC está ligado, o link TCP
persistente deixa de ser escolhido e o multiplexador do HTTP/3 ocupa esse
papel. Falhas de
negociação, ausência do endpoint ou incompatibilidade com um peer antigo são
falhas de transporte e caem para o caminho TCP atual, mantendo HTTP ou HTTPS
de acordo com a política TLS. A recusa HTTP do peer continua final e não é
repetida em outro transporte.

O servidor aceita QUIC independentemente de `quic_enabled`, mas o cliente só
o tenta quando a política está ligada. Assim, ativar a preferência não exige
reinício nem coordenação simultânea e um peer antigo continua atendendo o
fallback TCP.

## Contrato de transporte e telemetria

`InferenceTransport` passa a publicar `http`, `https` e `quic` para hops HTTP,
além dos transportes locais, túnel e stream que já existem. O canal persistente
não inventa um nome novo: sua evidência é `http` ou `https` conforme a conexão
que carregou o frame. `transport_fallback` fica verdadeiro quando uma tentativa
de transporte falha e o mesmo hop conclui por outra via, incluindo QUIC para
HTTP/HTTPS, mantendo o contador e as trocas da telemetria existentes.

O campo `tls` continua sendo verdadeiro apenas para HTTPS sobre TCP. QUIC é
sempre cifrado por TLS 1.3 próprio, mas não transforma a contagem de TLS de
aplicação TCP em verdadeira. O cliente não atribui `length` nem altera o
contrato de inferência por causa da troca de transporte.

## UI e API

`NodeInfo` terá `quic_enabled` e `quic_authority`. A tela de Preferências
ganha um painel de QUIC ao lado do painel TLS, com o mesmo interruptor,
autoridade somente no principal, estado read-only nos peers e cópia em inglês e
pt-BR. A rota administrativa será `POST /api/ui/network/quic` com
`{ "quic_enabled": boolean }`. A política autenticada para peers será
`GET /api/cluster/quic`.

A telemetria do Chat continuará mostrando a distribuição por hop, agora com
`quic` quando esse foi o caminho efetivo e com o fallback explícito quando
voltou para TCP.

## Invariantes

1. QUIC desligado não muda o comportamento de rede existente: TLS desligado é
   HTTP puro sobre TCP e TLS ligado é HTTPS sobre TCP.
2. QUIC usa HTTP/3 sobre QUIC e, portanto, sempre tem TLS 1.3 embutido.
3. Nenhum valor vindo diretamente do gossip muda a política persistida.
4. Falha de QUIC nunca interrompe uma inferência se o caminho TCP ou túnel
   disponível concluir a mesma chamada.
5. Trocar a política não cancela travessias ou canais já em andamento.
6. Uma recusa HTTP recebida do peer não é mascarada por fallback.
7. A telemetria publica o transporte observado, não a preferência configurada.

## Testes e validação

- Testes unitários de default serde, URL HTTP/HTTPS, política QUIC, gatilho do
  Announce, adoção autenticada e preservação do valor local em erro.
- Teste de integração do cliente e servidor HTTP/3 em loopback, além do canal
  de retorno direto.
- Teste de negociação que força a falha QUIC e prova o fallback TCP, incluindo
  a distinção HTTP versus HTTPS.
- Teste de troca de política sem interromper conexão já adquirida.
- Testes de UI do toggle principal/read-only e do indicador de telemetria QUIC.
- `cargo fmt`, testes direcionados do node, `cargo test -p node`, paridade de
  locales e `npm run build` da frontend.

## Fora de escopo

- Não alterar o motor de inferência, MTP, wire de estado oculto ou planner.
- Não substituir gossip, Kademlia, libp2p Noise ou Tailscale.
- Não exigir QUIC para a operação normal. O default permanece desligado.
