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

const cabeca = fakeSlicePeer("a", 0, 15);
const cauda = fakeSlicePeer("b", 16, 31);
const vazia = fakePeer({ node_id: "z" });

describe("networkPeers", () => {
  it("põe o pipeline primeiro, na ordem das camadas, e o resto da rede depois", () => {
    const { pipeline, others } = networkPeers(fakeCluster([cauda, vazia, cabeca]));
    expect(pipeline.map((p) => p.node_id)).toEqual(["a", "b"]);
    expect(others.map((p) => p.node_id)).toEqual(["z"]);
  });

  it("não perde nenhuma máquina da rede entre os dois grupos", () => {
    const c = fakeCluster([cauda, vazia, cabeca]);
    const { pipeline, others } = networkPeers(c);
    expect([...pipeline, ...others]).toHaveLength(c.peers.length);
    expect(new Set([...pipeline, ...others].map((p) => p.node_id)).size).toBe(3);
  });

  it("ordena as máquinas de fora pelo hostname que elas reportam", () => {
    const named = (id: string, hostname: string) =>
      fakePeer({ node_id: id, node: fakeNode({ node_id: id, short_id: id, hostname }) });
    const { others } = networkPeers(
      fakeCluster([cabeca, named("n1", "salem"), named("n2", "ksia"), named("n3", "atena")]),
    );
    expect(others.map((p) => p.node?.hostname)).toEqual(["atena", "ksia", "salem"]);
  });

  it("deixa `others` vazio quando toda a rede serve o modelo ativo", () => {
    expect(networkPeers(fakeCluster([cabeca, cauda])).others).toEqual([]);
  });

  // `pipelinePeers` cai para todos os peers quando nenhum casa com o modelo
  // ativo; sem esta guarda a mesma máquina apareceria nos dois grupos.
  it("não repete uma máquina quando nenhum peer casa com o modelo ativo", () => {
    const { pipeline, others } = networkPeers(fakeCluster([cabeca, vazia], "outro-modelo"));
    expect(pipeline).toHaveLength(2);
    expect(others).toEqual([]);
  });
});

describe("hasSlice", () => {
  it("lê `model` vazio com zero camadas como ausência de fatia, não como 0-0", () => {
    expect(hasSlice(vazia)).toBe(false);
    expect(hasSlice(cabeca)).toBe(true);
  });
});

describe("pipelinePeers", () => {
  it("continua ordenando por camada inicial", () => {
    expect(pipelinePeers(fakeCluster([cauda, cabeca])).map((p) => p.start_layer)).toEqual([0, 16]);
  });
});
