import { describe, expect, it } from "vitest";
import { canAccessRoute, homeRoute, routeProfiles } from "./access";

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

  it("manda cada perfil para uma tela inicial que ele abre", () => {
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
  it("abre a raiz de confiança para os três perfis", () => {
    for (const profile of ["admin", "node", "infer"] as const) {
      expect(canAccessRoute("/config/confianca", profile)).toBe(true);
    }
    expect(routeProfiles("/config/confianca")).toBeNull();
  });

  it("mantém o infer na rede local, conversa, documentação e sua chave", () => {
    expect(canAccessRoute("/chat", "infer")).toBe(true);
    expect(canAccessRoute("/config/keys/api", "infer")).toBe(true);
    expect(canAccessRoute("/config/rede", "infer")).toBe(true);
    expect(canAccessRoute("/documentacao", "infer")).toBe(true);
    expect(canAccessRoute("/config/confianca", "infer")).toBe(true);
    for (const path of ["/", "/modelos", "/implantar", "/logs", "/config/keys/conexao"]) {
      expect(canAccessRoute(path, "infer")).toBe(false);
    }
  });
});
