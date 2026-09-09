# Chat da rede - design

## Contexto

O POM já tem um enxame libp2p assinado, identidade Ed25519 estável por nó,
relay/DCuTR e comportamentos request/response separados para inferência e
logs. O Chat da rede precisa usar esse enxame sem transformar o canal de
conversa em um caminho alternativo de autoridade ou em uma fila que atrase o
pipeline de inferência.

## Decisões

### Topologia e autoridade

O canal usa estrela no principal efetivo da cadeia `AdminTrust`. O nó que
recebe a ação da UI coloca a ação em uma fila limitada para o swarm. O swarm
envia a ação ao principal pelo protocolo `/pom/network-chat/1.0.0`. No
principal, a ação é recusada se:

- o interruptor do Chat da rede estiver desligado;
- a API key não existir no SQLite local, não estiver ativa ou pertencer a um
  `node_id` em `blocked_nodes`;
- o `sender_node_id` não for o `node_id` da API key;
- o `PeerId` libp2p do transporte não corresponder ao `node_id` anunciado;
- a sala, mensagem, arquivo ou taxa exceder os limites do protocolo.

Somente depois dessas verificações o principal persiste o evento local e o
retransmite aos peers vivos não bloqueados. O retransmissor usa o mesmo
`room_id`, `sender_node_id` e `recipient_node_id` da ação. A API key não é
persistida nem enviada na retransmissão, pois já foi validada no ponto de
entrada; o frame original sempre a contém.

Um peer só aceita uma retransmissão cujo transporte seja o principal efetivo.
Uma tentativa direta entre peers comuns nunca vira evento local. Relay e
DCuTR são transparência de transporte do mesmo stream libp2p, logo NAT não
introduz um servidor de mensagens externo.

### Protocolo e limites

`network_chat.rs` terá codec length-prefixed JSON próprio, separado dos codecs
`infer_tunnel` e `cluster_logs`. O frame total fica limitado a 12 MiB; texto
fica limitado a 8 KiB; arquivo a 8 MiB; nome a 255 bytes e sala a 128 bytes.
Arquivo é transportado como bytes no request/response dedicado e guardado no
SQLite, sem escrever em diretórios do projeto ou competir com o catálogo de
modelos.

As filas de entrada da UI e de transporte usam `try_send` e capacidade
limitada. O principal mantém uma janela de taxa por `node_id` e aceita no
máximo 20 ações de texto/arquivo por 10 segundos e 8 eventos de digitação por
segundo. O worker de persistência usa `SyncSender::try_send`; pressão descarta
o evento e incrementa um contador exposto no estado do canal.

O loop do swarm processa eventos de inferência antes das mensagens de chat.
Validação/persistência não espera SQLite dentro de `handle_event`; o chat é
encaminhado para uma task de baixa prioridade com limite de concorrência. A
telemetria de carga mede RTT de uma sonda de inferência durante uma rajada de
chat e falha se o p95 crescer além da margem definida no teste.

### Persistência e API

O banco `pom.db` recebe `network_chat_messages`, com `message_id`, `room_id`,
`sender_node_id`, `recipient_node_id`, tipo, texto, nome/MIME/tamanho/bytes do
arquivo e timestamp. Há índice por `(room_id, received_seq)` e por
`sender_node_id`. O histórico local retorna eventos de texto e arquivo, sem
eventos transitórios de digitação; o conteúdo do arquivo volta em base64 para
permitir reabrir a conversa e baixar o anexo pela mesma tela.

`GET /api/ui/network-chat` retorna `enabled`, histórico recente, participantes
online indexados por `node_id` e indicadores de digitação ainda válidos.
`POST /api/ui/network-chat` recebe uma ação e exige no corpo a `api_key` em
toda mensagem, arquivo ou evento de digitação. A superfície HTTP continua
protegida pela sessão administrativa da UI, enquanto a chave do corpo é a
credencial que o principal valida para a rede. O endpoint de política
`GET /api/cluster/network-chat` é autenticado por `connection_key` e só o
principal pode alterar a política por `POST /api/ui/network-chat/policy`.

### Política propagada

`network_chat_enabled` é salvo em `Config` e anunciado como gatilho no
`Announce`. Como no TLS e nos logs, um peer não adota o booleano recebido. Se
o valor anunciado pelo principal divergir, o peer faz uma leitura autenticada
no principal e só então grava a própria configuração e liga/desliga o serviço.
Falha na leitura preserva o valor local. Um chat desligado recusa tanto
entrada quanto retransmissão em todos os nós.

### Interface

`Chat.tsx` ganha uma barra de duas abas, `Chat` e `Rede`, sem misturar o
histórico da IA com o histórico por `node_id`. A aba Rede mostra participantes
online, mensagens alinhadas pelo remetente, anexos com nome/tamanho, campo de
texto, seletor de arquivo e o indicador de digitação. O histórico é buscado
novamente ao abrir a aba e enquanto ela fica visível. Todos os textos novos
entram em `messages.csv` em inglês e pt-BR.

Preferências recebe o painel `Ligar Chat da rede`, no mesmo padrão do TLS e
dos logs. O controle só aparece ativo quando o nó é o principal efetivo; nos
demais nós mostra o estado e a explicação de que a decisão vem do principal.

## Testes e critérios de aceitação

- testes unitários provam limites, fila sem bloqueio, taxa, validação da chave,
  vínculo token/node/PeerId e a política fail-closed;
- teste de gateway prova que ação com chave inválida ou node bloqueado não é
  persistida nem retransmitida;
- teste de dois peers no harness libp2p prova texto, arquivo, typing e
  histórico após reabrir;
- teste de política prova desligamento e religamento propagados;
- teste de prioridade injeta uma rajada maior que a fila do chat enquanto
  mede sondas do túnel de inferência e compara p95 com o baseline;
- `cargo test` da crate do node, `npm run build` e testes Vitest do frontend
  devem passar.

## Fora de escopo

Não haverá XMPP, broker externo, conta humana nova, mensagens offline para
peers mortos, edição/remoção de mensagens ou salas criadas pelo usuário. A
primeira sala é a sala global fixa `network`; os campos de sala e destinatário
continuam presentes para que a identidade nunca dependa de hostname ou
endereço.
