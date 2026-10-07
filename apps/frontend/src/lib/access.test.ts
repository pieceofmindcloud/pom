import { describe, expect, it } from "vitest";
import { canAccessRoute, canAccessEnterprise, canSeePeerDetails, homeRoute, routeProfiles } from "./access";

describe("canAccessRoute", () => {
  it("deixa o administrador entrar nas telas de operação do cluster", () => {
    for (const path of ["/implantar", "/implantacoes", "/logs"]) {
      expect(canAccessRoute(path, "admin")).toBe(true);
    }
  });

  it("fecha implantação para o perfil do nó, mas mantém as telas Enterprise navegáveis", () => {
    for (const path of ["/implantar", "/implantacoes", "/modelos", "/modelos/remotos", "/modelos/fatiar", "/fatias"]) {
      expect(canAccessRoute(path, "node")).toBe(false);
      expect(canAccessRoute(path, "admin")).toBe(true);
    }
    expect(canAccessRoute("/logs", "node")).toBe(true);
    expect(canAccessRoute("/estatisticas", "node")).toBe(true);
  });

  it("deixa passar toda rota que ninguém restringiu", () => {
    for (const path of ["/", "/chat", "/transferencias", "/config/rede", "/inexistente"]) {
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
    expect(canAccessRoute("/logs/qualquer-filha", "node")).toBe(true);
  });

  it("diz que uma rota livre não tem lista de perfis", () => {
    expect(routeProfiles("/transferencias")).toBeNull();
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
    expect(canAccessRoute("/network-chat", "infer")).toBe(true);
    expect(canAccessRoute("/config/keys/api", "infer")).toBe(true);
    expect(canAccessRoute("/config/rede", "infer")).toBe(true);
    expect(canAccessRoute("/documentacao", "infer")).toBe(true);
    expect(canAccessRoute("/config/confianca", "infer")).toBe(true);
    for (const path of ["/", "/modelos", "/implantar", "/config/keys/conexao", "/config/rede/participantes", "/config/preferencias/admin"]) {
      expect(canAccessRoute(path, "infer")).toBe(false);
    }
    // Logs (requests and deployment) and statistics are the network's
    // operation: never offered to an inference node.
    for (const path of ["/logs", "/logs/qualquer-filha", "/estatisticas", "/implantacoes/logs"]) {
      expect(canAccessRoute(path, "infer"), path).toBe(false);
    }
  });

  it("mostra a chave do cluster só ao admin", () => {
    expect(canAccessRoute("/config/keys/conexao", "admin")).toBe(true);
    expect(canAccessRoute("/config/keys/conexao", "node")).toBe(false);
    expect(canAccessRoute("/config/keys/conexao", "infer")).toBe(false);
  });

  it("mantém participantes e preferências administrativas nos perfis atuais", () => {
    expect(canAccessRoute("/config/rede/participantes", "admin")).toBe(true);
    expect(canAccessRoute("/config/rede/participantes", "node")).toBe(true);
    expect(canAccessRoute("/config/rede/participantes", "infer")).toBe(false);
    expect(canAccessRoute("/config/preferencias/admin", "admin")).toBe(true);
    expect(canAccessRoute("/config/preferencias/admin", "node")).toBe(true);
  });

  /**
   * Os Detalhes do Painel são operação do cluster: estado interno de cada
   * máquina, liberar fatia, remover do pipeline, tirar da rede. O `node` abre
   * o Painel e fica com o Fluxo - e onde há uma vista só, não há o que
   * escolher.
   */
  it("reserva o detalhe máquina a máquina do Painel ao administrador", () => {
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
    expect(canAccessEnterprise("/estatisticas", false)).toBe(true);
  });

  it("keeps Enterprise routes available when the lease is active", () => {
    expect(canAccessEnterprise("/logs", true)).toBe(true);
    expect(canAccessEnterprise("/estatisticas", true)).toBe(true);
  });
});
