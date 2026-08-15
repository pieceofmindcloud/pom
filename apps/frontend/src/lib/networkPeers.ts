/**
 * Como o Painel lê a lista de peers do nó.
 *
 * Duas perguntas diferentes, e a tela precisa das duas: "quem serve o modelo
 * ativo, na ordem em que a requisição passa" (`pipelinePeers`) e "quem está na
 * rede" (`networkPeers`). A segunda inclui máquina conectada e sem fatia
 * implantada, que é justamente a que não aparece em lugar nenhum quando só se
 * olha o pipeline - e é onde se vai procurar espaço para a próxima fatia.
 */
import type { ClusterView, PeerView } from "./types";

/** Peers serving the model this node is on, in execution order. */
export function pipelinePeers(cluster: ClusterView): PeerView[] {
  const onModel = cluster.peers.filter((p) => p.model === cluster.active_model);
  return (onModel.length ? onModel : cluster.peers)
    .slice()
    .sort((a, b) => a.start_layer - b.start_layer || a.short_id.localeCompare(b.short_id));
}

/**
 * A máquina anuncia uma fatia? `model` vazio com `total_layers` zerado é o que
 * o nó manda para quem está na rede sem nada implantado (`webui::cluster`), e
 * não é uma faixa "0-0": é a ausência de faixa.
 */
export function hasSlice(peer: PeerView): boolean {
  return Boolean(peer.model) && peer.total_layers > 0;
}

/**
 * A rede inteira, separada em quem faz parte da implantação ativa e quem não.
 *
 * A primeira lista mantém a ordem do pipeline; a segunda vai por nome, que é
 * como o operador procura uma máquina que ainda não tem papel.
 */
export function networkPeers(cluster: ClusterView): {
  pipeline: PeerView[];
  others: PeerView[];
} {
  const pipeline = pipelinePeers(cluster);
  const inPipeline = new Set(pipeline.map((p) => p.node_id));
  const label = (p: PeerView) => p.node?.hostname || p.short_id;
  const others = cluster.peers
    .filter((p) => !inPipeline.has(p.node_id))
    .slice()
    .sort((a, b) => label(a).localeCompare(label(b)) || a.short_id.localeCompare(b.short_id));
  return { pipeline, others };
}
