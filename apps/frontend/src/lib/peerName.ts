/**
 * Como uma máquina se chama na tela.
 *
 * O nome é o que o nó **anuncia** — `node_name` do `pom.json`, ou o hostname
 * do sistema quando ninguém batizou a máquina. O painel, o pipeline e a tela
 * de nós leem esse nome de `/api/ui/cluster`; a tela de fatias o lê de
 * `/api/ui/cluster/models`, e as duas rotas agora respondem a mesma coisa.
 *
 * O `node_id` encurtado é o último recurso, não o nome: é um prefixo
 * hexadecimal, e ver `83bee8670abc` no lugar de `ksia` num seletor de destino
 * não diz para qual máquina o arquivo vai.
 */
export interface NamedPeer {
  node_id: string;
  hostname?: string | null;
}

export function peerDisplayName(peer: NamedPeer): string {
  return peer.hostname?.trim() || peer.node_id.slice(0, 12);
}

/**
 * O mesmo nome, desempatado quando dois peers da lista o repetem.
 *
 * Dois nós na mesma máquina compartilham hostname de propósito (é o modo de
 * dar janela grande sem arriscar a RAM da menor), e aí só o `node_id`
 * distingue um do outro. O desempate é dentro da lista mostrada: acrescentar o
 * prefixo a um nome que já é único só o deixaria mais difícil de ler.
 */
export function uniquePeerName(peer: NamedPeer, all: NamedPeer[]): string {
  const name = peerDisplayName(peer);
  const repeated = all.filter((p) => peerDisplayName(p) === name).length > 1;
  return repeated ? `${name} · ${peer.node_id.slice(0, 6)}` : name;
}
