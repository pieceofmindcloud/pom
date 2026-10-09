// @vitest-environment jsdom
/**
 * A plugin's service shows up next to the machine it runs on: on the details
 * card and, in the Flow, hanging under its card.
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

describe("plugin services", () => {
  afterEach(cleanup);

  it("says what the service is and what state it is in", () => {
    wrap(<PluginServiceBadge service={qdrant} />);
    const badge = screen.getByTestId("plugin-service");
    expect(badge.textContent).toContain("Qdrant");
    expect(badge.textContent).toContain("Servidor de RAG");
    expect(badge.getAttribute("title")).toContain("v1.19.1");
    expect(badge.getAttribute("data-status")).toBe("ready");
  });

  it("writes the state when the service is not up", () => {
    wrap(<PluginServiceBadge service={{ ...qdrant, status: "installing" }} />);
    expect(screen.getByTestId("plugin-service").textContent).toContain("instalando");
  });

  it("shows an unknown type as the plugin called it", () => {
    wrap(<PluginServiceBadge service={{ ...qdrant, kind: "search" }} />);
    expect(screen.getByTestId("plugin-service").textContent).toContain("search");
  });

  it("hangs the services under the machine card in the Flow", () => {
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

  it("inside the expanded card it sits under its border, whatever the height", () => {
    wrap(<PluginServiceTether services={[qdrant]} />);
    const tether = screen.getByTestId("plugin-service-tether");
    expect(tether.style.top).toBe("100%");
    expect(tether.style.left).toBe("16px");
  });

  it("draws nothing for a machine without services", () => {
    wrap(<PluginServiceTether services={[]} position={{ left: 0, top: 0 }} size={{ width: 1, height: 1 }} />);
    expect(screen.queryByTestId("plugin-service-tether")).toBeNull();
  });

  it("lists the services in the details card of the machine that runs them", () => {
    const withQdrant = fakePeer({
      node_id: "q",
      node: fakeNode({ node_id: "q", short_id: "q", hostname: "atena", plugin_services: [qdrant] }),
    });
    wrap(<PeerDetailCard peer={withQdrant} index={0} group="qwen35" otherRanges={[]} />);
    expect(screen.getByTestId("plugin-services").textContent).toContain("Qdrant");
  });

  it("creates no section on a machine without services", () => {
    const withoutService = fakePeer({ node_id: "z", node: fakeNode({ node_id: "z", short_id: "z" }) });
    wrap(<PeerDetailCard peer={withoutService} index={0} group="qwen35" otherRanges={[]} />);
    expect(screen.queryByTestId("plugin-services")).toBeNull();
  });
});
