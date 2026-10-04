// @vitest-environment jsdom
/**
 * O serviço de um plugin aparece junto da máquina em que roda: no card de
 * detalhes e, no Fluxo, pendurado sob o card dela.
 */
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { PeerDetailCard } from "./PeerDetailCard";
import { PluginServiceBadge, PluginServiceTether } from "./PluginServices";
import { LocaleProvider } from "../i18n/provider";
import { ToastProvider } from "../lib/toast";
import { fakeNode, fakePeer } from "../test/fixtures";
import type { PluginService } from "../lib/types";

const qdrant: PluginService = {
  plugin_code: "qdrant",
  id: "rag",
  kind: "rag",
  name: "Qdrant",
  status: "ready",
  version: "v1.19.1",
};

function wrap(children: React.ReactNode) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <LocaleProvider initialLocale="pt-BR">
        <ToastProvider>{children}</ToastProvider>
      </LocaleProvider>
    </QueryClientProvider>,
  );
}

describe("serviços de plugin", () => {
  afterEach(cleanup);

  it("diz o que o serviço é e em que estado está", () => {
    wrap(<PluginServiceBadge service={qdrant} />);
    const badge = screen.getByTestId("plugin-service");
    expect(badge.textContent).toContain("Qdrant");
    expect(badge.textContent).toContain("Servidor de RAG");
    expect(badge.getAttribute("title")).toContain("v1.19.1");
    expect(badge.getAttribute("data-status")).toBe("ready");
  });

  it("escreve o estado quando o serviço não está no ar", () => {
    wrap(<PluginServiceBadge service={{ ...qdrant, status: "installing" }} />);
    expect(screen.getByTestId("plugin-service").textContent).toContain("instalando");
  });

  it("mostra um tipo desconhecido como o plugin o chamou", () => {
    wrap(<PluginServiceBadge service={{ ...qdrant, kind: "search" }} />);
    expect(screen.getByTestId("plugin-service").textContent).toContain("search");
  });

  it("pendura os serviços sob o card da máquina no Fluxo", () => {
    wrap(
      <PluginServiceTether
        services={[qdrant]}
        position={{ left: 100, top: 200 }}
        size={{ width: 264, height: 204 }}
      />,
    );
    const tether = screen.getByTestId("plugin-service-tether");
    expect(tether.style.top).toBe("404px");
    expect(tether.style.left).toBe("116px");
  });

  it("dentro do card expandido fica sob a borda dele, qualquer que seja a altura", () => {
    wrap(<PluginServiceTether services={[qdrant]} />);
    const tether = screen.getByTestId("plugin-service-tether");
    expect(tether.style.top).toBe("100%");
    expect(tether.style.left).toBe("16px");
  });

  it("não desenha nada para uma máquina sem serviços", () => {
    wrap(<PluginServiceTether services={[]} position={{ left: 0, top: 0 }} size={{ width: 1, height: 1 }} />);
    expect(screen.queryByTestId("plugin-service-tether")).toBeNull();
  });

  it("lista os serviços no card de detalhes da máquina que os roda", () => {
    const comQdrant = fakePeer({
      node_id: "q",
      node: fakeNode({ node_id: "q", short_id: "q", hostname: "atena", plugin_services: [qdrant] }),
    });
    wrap(<PeerDetailCard peer={comQdrant} index={0} group="qwen35" otherRanges={[]} />);
    expect(screen.getByTestId("plugin-services").textContent).toContain("Qdrant");
  });

  it("não cria a seção numa máquina sem serviços", () => {
    const semServico = fakePeer({ node_id: "z", node: fakeNode({ node_id: "z", short_id: "z" }) });
    wrap(<PeerDetailCard peer={semServico} index={0} group="qwen35" otherRanges={[]} />);
    expect(screen.queryByTestId("plugin-services")).toBeNull();
  });
});
