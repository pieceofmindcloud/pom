import { describe, expect, it } from "vitest";
import { canAccessRoute, canAccessEnterprise, canSeePeerDetails, homeRoute, routeProfiles } from "./access";

describe("canAccessRoute", () => {
  it("lets the administrator into the cluster operation screens", () => {
    for (const path of ["/deploy", "/deployments", "/logs"]) {
      expect(canAccessRoute(path, "admin")).toBe(true);
    }
  });

  it("closes deployment to the node profile, but keeps the Enterprise screens navigable", () => {
    for (const path of ["/deploy", "/deployments"]) {
      expect(canAccessRoute(path, "node")).toBe(false);
    }
    expect(canAccessRoute("/logs", "node")).toBe(true);
    expect(canAccessRoute("/statistics", "node")).toBe(true);
  });

  it("lets through every route nobody restricted", () => {
    for (const path of ["/", "/chat", "/models", "/shards", "/config/network", "/inexistente"]) {
      expect(canAccessRoute(path, "node")).toBe(true);
      expect(canAccessRoute(path, "admin")).toBe(true);
    }
  });

  /**
   * A restrição casa segmento inteiro. Se casasse por `startsWith` cru, uma
   * rota futura chamada `/deploy-draft` nasceria fechada sem ninguém ter
   * decidido isso - e o contrário, uma filha de `/logs`, ficaria aberta.
   */
  it("matches by segment, not by text prefix", () => {
    expect(canAccessRoute("/deployx", "node")).toBe(true);
    expect(canAccessRoute("/logs-antigos", "node")).toBe(true);
    expect(canAccessRoute("/logs/qualquer-filha", "node")).toBe(true);
  });

  it("says a free route has no profile list", () => {
    expect(routeProfiles("/models")).toBeNull();
    expect(routeProfiles("/deploy")).toEqual(["admin"]);
  });

  it("sends each profile to a home screen it can open", () => {
    // O `/` é o Painel: o infer cairia na recusa, que é um beco.
    expect(homeRoute("infer")).toBe("/chat");
    expect(canAccessRoute(homeRoute("infer"), "infer")).toBe(true);
    expect(homeRoute("admin")).toBe("/");
    expect(homeRoute("node")).toBe("/");
  });

  /**
   * A tela da raiz de confiança é o trabalho anterior à autoridade: um nó
   * `admin` por ancorar ainda não tem capacidade de admin, e quem recebe a
   * âncora costuma ser `node` ou `infer`. Fechá-la a algum perfil deixaria o
   * operador sem caminho justamente onde ele precisa dela.
   */
  it("opens the trust root for the three profiles", () => {
    for (const profile of ["admin", "node", "infer"] as const) {
      expect(canAccessRoute("/config/trust", profile)).toBe(true);
    }
    expect(routeProfiles("/config/trust")).toBeNull();
  });

  it("keeps infer on the local network, conversation, documentation and its key", () => {
    expect(canAccessRoute("/chat", "infer")).toBe(true);
    expect(canAccessRoute("/network-chat", "infer")).toBe(true);
    expect(canAccessRoute("/config/keys/api", "infer")).toBe(true);
    expect(canAccessRoute("/config/network", "infer")).toBe(true);
    expect(canAccessRoute("/documentation", "infer")).toBe(true);
    expect(canAccessRoute("/config/trust", "infer")).toBe(true);
    for (const path of ["/", "/models", "/deploy", "/config/keys/connection", "/config/network/participants", "/config/preferences/admin"]) {
      expect(canAccessRoute(path, "infer")).toBe(false);
    }
    // Logs (requests and deployment) and statistics are the network's
    // operation: never offered to an inference node.
    for (const path of ["/logs", "/logs/qualquer-filha", "/statistics", "/deployments/logs"]) {
      expect(canAccessRoute(path, "infer"), path).toBe(false);
    }
  });

  it("shows the cluster key only to the admin", () => {
    expect(canAccessRoute("/config/keys/connection", "admin")).toBe(true);
    expect(canAccessRoute("/config/keys/connection", "node")).toBe(false);
    expect(canAccessRoute("/config/keys/connection", "infer")).toBe(false);
  });

  it("keeps participants and administrative preferences in the current profiles", () => {
    expect(canAccessRoute("/config/network/participants", "admin")).toBe(true);
    expect(canAccessRoute("/config/network/participants", "node")).toBe(true);
    expect(canAccessRoute("/config/network/participants", "infer")).toBe(false);
    expect(canAccessRoute("/config/preferences/admin", "admin")).toBe(true);
    expect(canAccessRoute("/config/preferences/admin", "node")).toBe(true);
  });

  /**
   * Os Detalhes do Painel são operação do cluster: estado interno de cada
   * máquina, liberar fatia, remover do pipeline, tirar da rede. O `node` abre
   * o Painel e fica com o Fluxo - e onde há uma vista só, não há o que
   * escolher.
   */
  it("reserves the machine-by-machine detail of the Dashboard for the administrator", () => {
    expect(canSeePeerDetails("admin")).toBe(true);
    expect(canSeePeerDetails("node")).toBe(false);
    expect(canSeePeerDetails("infer")).toBe(false);
    // O Painel em si continua aberto ao `node`: o que muda é a vista.
    expect(canAccessRoute("/", "node")).toBe(true);
  });
});

describe("canAccessEnterprise", () => {
  it("keeps request logs and statistics navigable when the lease is inactive", () => {
    expect(canAccessEnterprise("/logs", false)).toBe(true);
    expect(canAccessEnterprise("/statistics", false)).toBe(true);
  });

  it("keeps Enterprise routes available when the lease is active", () => {
    expect(canAccessEnterprise("/logs", true)).toBe(true);
    expect(canAccessEnterprise("/statistics", true)).toBe(true);
  });
});
