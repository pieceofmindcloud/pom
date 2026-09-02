import { describe, expect, it } from "vitest";
import { canAccessRoute, routeProfiles } from "./access";

describe("canAccessRoute", () => {
  it("deixa o administrador entrar nas telas de operação do cluster", () => {
    for (const path of ["/implantar", "/implantacoes", "/logs"]) {
      expect(canAccessRoute(path, "admin")).toBe(true);
    }
  });

  it("fecha implantar, implantações e logs para o perfil do nó", () => {
    for (const path of ["/implantar", "/implantacoes", "/logs"]) {
      expect(canAccessRoute(path, "node")).toBe(false);
    }
  });

  it("deixa passar toda rota que ninguém restringiu", () => {
    for (const path of ["/", "/chat", "/modelos", "/fatias", "/config/rede", "/inexistente"]) {
      expect(canAccessRoute(path, "node")).toBe(true);
      expect(canAccessRoute(path, "admin")).toBe(true);
    }
  });

  /**
   * A restrição casa segmento inteiro. Se casasse por `startsWith` cru, uma
   * rota futura chamada `/implantar-rascunho` nasceria fechada sem ninguém ter
   * decidido isso - e o contrário, uma filha de `/logs`, ficaria aberta.
   */
  it("casa por segmento, não por prefixo de texto", () => {
    expect(canAccessRoute("/implantarx", "node")).toBe(true);
    expect(canAccessRoute("/logs-antigos", "node")).toBe(true);
    expect(canAccessRoute("/logs/qualquer-filha", "node")).toBe(false);
  });

  it("diz que uma rota livre não tem lista de perfis", () => {
    expect(routeProfiles("/modelos")).toBeNull();
    expect(routeProfiles("/implantar")).toEqual(["admin"]);
  });

  it("limita o inference à conversa, documentação e à sua tela de chave", () => {
    expect(canAccessRoute("/chat", "inference")).toBe(true);
    expect(canAccessRoute("/config/keys/api", "inference")).toBe(true);
    expect(canAccessRoute("/documentacao", "inference")).toBe(true);
    for (const path of ["/", "/modelos", "/implantar", "/logs", "/config/rede", "/config/keys/conexao"]) {
      expect(canAccessRoute(path, "inference")).toBe(false);
    }
  });
});
