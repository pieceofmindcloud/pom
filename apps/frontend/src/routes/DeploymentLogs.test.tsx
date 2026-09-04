// @vitest-environment jsdom
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { MemoryRouter } from "react-router-dom";
import { LocaleProvider } from "../i18n/provider";
import { DeploymentLogs } from "./DeploymentLogs";

const queryMocks = vi.hoisted(() => ({
  logs: {
    data: {
      entries: [
        {
          received_seq: 1,
          received_at: "2026-09-04T12:00:01Z",
          node_id: "node-a",
          node_name: "salem",
          deploy_id: "deploy-1",
          origin_at: "2026-09-04T12:00:30Z",
          level: "INFO",
          target: "node::second",
          message: "chegou primeiro",
        },
        {
          received_seq: 2,
          received_at: "2026-09-04T12:00:02Z",
          node_id: "node-b",
          node_name: "ksia",
          deploy_id: "deploy-1",
          origin_at: "2026-09-04T11:59:00Z",
          level: "WARN",
          target: "node::first",
          message: "chegou depois",
        },
      ],
      dropped: 3,
    },
    isLoading: false,
    isError: false,
    error: null,
    refetch: vi.fn(),
  },
  deployments: { data: { deployments: [] } },
}));

vi.mock("../lib/queries", () => ({
  useClusterLogs: () => queryMocks.logs,
  useDeployments: () => queryMocks.deployments,
}));

describe("DeploymentLogs", () => {
  afterEach(cleanup);

  it("mantém a ordem de chegada e exibe o nome do nó", () => {
    render(
      <MemoryRouter>
        <LocaleProvider initialLocale="pt-BR">
          <DeploymentLogs />
        </LocaleProvider>
      </MemoryRouter>,
    );

    const lines = screen.getAllByTestId("deployment-log-line");
    expect(lines).toHaveLength(2);
    expect(lines[0].textContent).toContain("salem");
    expect(lines[0].textContent).toContain("chegou primeiro");
    expect(lines[1].textContent).toContain("ksia");
    expect(lines[1].textContent).toContain("chegou depois");
    expect(screen.getByText("3 linhas descartadas antes do envio")).toBeTruthy();
  });
});
