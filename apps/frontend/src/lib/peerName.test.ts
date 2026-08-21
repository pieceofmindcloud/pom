import { describe, expect, it } from "vitest";
import { peerDisplayName, uniquePeerName } from "./peerName";

/**
 * O seletor "Enviar para..." da tela de fatias mostrava o rótulo errado: ele
 * lia `/api/ui/cluster/models`, que respondia com o hostname cru do sistema
 * operacional em vez do nome que o nó anuncia (`node_name` do pom.json). Numa
 * máquina com dois nós era pior — os dois relatavam o mesmo hostname do SO, a
 * lista via colisão e desempatava com o prefixo do `node_id`.
 */
describe("peerDisplayName", () => {
  it("usa o nome que a máquina anuncia", () => {
    expect(peerDisplayName({ node_id: "83bee8670abc2155", hostname: "ksia" })).toBe("ksia");
  });

  /**
   * O `node_id` encurtado é o último recurso, não o nome: é um prefixo
   * hexadecimal, e não diz para qual máquina o arquivo vai.
   */
  it("cai no id curto só quando não há nome nenhum", () => {
    expect(peerDisplayName({ node_id: "83bee8670abc2155" })).toBe("83bee8670abc");
    expect(peerDisplayName({ node_id: "83bee8670abc2155", hostname: null })).toBe("83bee8670abc");
    expect(peerDisplayName({ node_id: "83bee8670abc2155", hostname: "  " })).toBe("83bee8670abc");
  });
});

describe("uniquePeerName", () => {
  it("não polui um nome que já é único", () => {
    const peers = [
      { node_id: "aaaa1111bbbb2222", hostname: "atena" },
      { node_id: "cccc3333dddd4444", hostname: "ksia" },
    ];
    expect(uniquePeerName(peers[0], peers)).toBe("atena");
    expect(uniquePeerName(peers[1], peers)).toBe("ksia");
  });

  /**
   * Dois nós na mesma máquina compartilham hostname de propósito — é o modo
   * de dar janela grande sem arriscar a RAM da menor. Aí só o `node_id`
   * distingue um do outro.
   */
  it("desempata dois nós da mesma máquina pelo id", () => {
    const peers = [
      { node_id: "aaaa1111bbbb2222", hostname: "ksia" },
      { node_id: "cccc3333dddd4444", hostname: "ksia" },
    ];
    expect(uniquePeerName(peers[0], peers)).toBe("ksia · aaaa11");
    expect(uniquePeerName(peers[1], peers)).toBe("ksia · cccc33");
  });
});
