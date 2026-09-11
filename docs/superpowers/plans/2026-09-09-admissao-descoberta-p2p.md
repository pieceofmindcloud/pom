# Plano: admissão autenticada na descoberta P2P

## Objetivo

Impedir que um anúncio sem prova de pertencimento seja registrado em
`Network.peers`. A tabela é a fronteira usada pela UI, pelo planejador e pelo
pipeline, então o gate precisa ocorrer antes de qualquer inserção ou efeito
derivado do peer.

## Decisão de desenho

- Rede privada é reconhecida por uma `connection_key` não vazia e prova a
  chave com o endpoint existente `/handshake`; a chave não viaja no
  `Announce`.
- Rede pública exige âncora local válida, `cluster_id` igual ao da âncora e a
  tupla assinada `{node_id, peer_id, pubkey}` de um principal ou concessão
  administrativa não demovida. `cluster_id` isolado nunca é credencial.
  - Seguimento (2026-09-10): a regra acima passou a valer só para
    `access_policy = authorized` (padrão). Ela recusava todo nó worker comum,
    inclusive os co-locados legítimos, que nunca recebem concessão. Com
    `access_policy = anyone_with_anchor` a admissão pública exige apenas âncora
    local válida para o `cluster_id` anunciado (`AdminTrust::anchors_cluster`)
    mais o vínculo de identidade; o `cluster_id` continua não sendo credencial
    sozinho porque a identidade é amarrada e o `Announce` do gossip é assinado.
    Ver `docs/subsystems/rede-p2p.md`.
- `Announce` passa a carregar `cluster_id` opcional para selecionar a âncora;
  UDP e gossipsub usam o mesmo gate. `PeerList` também é revalidado e não
  herda `connection_verified` de outro nó.
- mDNS, Kademlia e pkarr continuam mecanismos de transporte/endereço. Um
  desconhecido pode ser visto no swarm, mas não entra em `peers`, UI, rotas ou
  pipeline.

## Sequência

1. Adicionar testes para chave incorreta, ausência de trust e principal válido.
2. Mover a cadeia de trust para a montagem do stack e ligar o gate à Network.
3. Validar Announce UDP, Announce gossipsub, PeerList e inserção direta.
4. Corrigir o handshake para que chave vazia nunca seja uma credencial válida.
5. Atualizar documentação do subsistema e executar fmt, testes direcionados e
   suíte da crate.
6. Rebase final em `origin/feat/implantar3`, commit e push da branch.

## Não coberto

Não há alteração no llama.cpp vendorizado. A descoberta de transporte não é
transformada em PSK de libp2p nesta mudança; a admissão da aplicação acontece
antes da tabela de peers e o custo extra fica restrito ao primeiro Announce de
um peer privado, com timeout curto.
