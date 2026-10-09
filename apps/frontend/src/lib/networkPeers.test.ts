/**
 * A aba Detalhes mostra a rede inteira, e não só o pipeline.
 *
 * O que se afirma aqui é a separação: quem serve o modelo ativo continua na
 * ordem de execução e vem primeiro; quem está na rede sem fatia aparece depois,
 * por nome, em vez de simplesmente sumir da tela.
 */
import { describe, expect, it } from "vitest";
import { hasSlice, networkPeers, pipelinePeers } from "./networkPeers";
import { fakeCluster, fakeNode, fakePeer, fakeSlicePeer } from "../test/fixtures";

const head = fakeSlicePeer("a", 0, 15);
const tail = fakeSlicePeer("b", 16, 31);
const empty = fakePeer({ node_id: "z" });

describe("networkPeers", () => {
  it("puts the pipeline first, in layer order, and the rest of the network after", () => {
    const { pipeline, others } = networkPeers(fakeCluster([tail, empty, head]));
    expect(pipeline.map((p) => p.node_id)).toEqual(["a", "b"]);
    expect(others.map((p) => p.node_id)).toEqual(["z"]);
  });

  it("loses no machine of the network between the two groups", () => {
    const c = fakeCluster([tail, empty, head]);
    const { pipeline, others } = networkPeers(c);
    expect([...pipeline, ...others]).toHaveLength(c.peers.length);
    expect(new Set([...pipeline, ...others].map((p) => p.node_id)).size).toBe(3);
  });

  it("orders the outside machines by the hostname they report", () => {
    const named = (id: string, hostname: string) =>
      fakePeer({ node_id: id, node: fakeNode({ node_id: id, short_id: id, hostname }) });
    const { others } = networkPeers(
      fakeCluster([head, named("n1", "salem"), named("n2", "ksia"), named("n3", "atena")]),
    );
    expect(others.map((p) => p.node?.hostname)).toEqual(["atena", "ksia", "salem"]);
  });

  it("leaves `others` empty when the whole network serves the active model", () => {
    expect(networkPeers(fakeCluster([head, tail])).others).toEqual([]);
  });

  // `pipelinePeers` cai para todos os peers quando nenhum casa com o modelo
  // ativo; sem esta guarda a mesma máquina apareceria nos dois grupos.
  it("does not repeat a machine when no peer matches the active model", () => {
    const { pipeline, others } = networkPeers(fakeCluster([head, empty], "outro-modelo"));
    expect(pipeline).toHaveLength(2);
    expect(others).toEqual([]);
  });
});

describe("hasSlice", () => {
  it("reads an empty `model` with zero layers as no slice, not as 0-0", () => {
    expect(hasSlice(empty)).toBe(false);
    expect(hasSlice(head)).toBe(true);
  });
});

describe("pipelinePeers", () => {
  it("keeps ordering by starting layer", () => {
    expect(pipelinePeers(fakeCluster([tail, head])).map((p) => p.start_layer)).toEqual([0, 16]);
  });
});
