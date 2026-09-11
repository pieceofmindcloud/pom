// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
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
  beforeEach(() => {
    HTMLElement.prototype.scrollTo = vi.fn();
    queryMocks.logs.data = {
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
    };
  });

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

  it("trata a réplica vazia como ausência de dados, não como bloqueio do secundário", () => {
    queryMocks.logs.data = { entries: [], dropped: 0 };
    render(
      <MemoryRouter>
        <LocaleProvider initialLocale="pt-BR">
          <DeploymentLogs />
        </LocaleProvider>
      </MemoryRouter>,
    );

    expect(screen.getByText("Nenhum log de processo ainda")).toBeTruthy();
    expect(screen.getAllByText(/réplica local/i).length).toBeGreaterThan(0);
    expect(screen.queryByText(/nó principal/i)).toBeNull();
  });

  it("segue linhas novas no fim enquanto a rolagem automática está ligada", async () => {
    const { rerender } = render(
      <MemoryRouter>
        <LocaleProvider initialLocale="pt-BR">
          <DeploymentLogs />
        </LocaleProvider>
      </MemoryRouter>,
    );
    const terminal = screen.getByTestId("deployment-log-terminal") as HTMLDivElement;
    Object.defineProperties(terminal, {
      clientHeight: { configurable: true, value: 100 },
      scrollHeight: { configurable: true, value: 1000 },
    });
    terminal.scrollTop = 900;
    terminal.scrollTo = vi.fn();

    queryMocks.logs.data = {
      ...queryMocks.logs.data,
      entries: [...queryMocks.logs.data.entries, {
        ...queryMocks.logs.data.entries[1],
        received_seq: 3,
        message: "linha nova",
      }],
    };
    rerender(
      <MemoryRouter>
        <LocaleProvider initialLocale="pt-BR">
          <DeploymentLogs />
        </LocaleProvider>
      </MemoryRouter>,
    );

    await waitFor(() => expect(terminal.scrollTo).toHaveBeenCalledWith({
      top: 1000,
      behavior: "auto",
    }));
  });

  it("para de perseguir o fim quando o leitor sobe e expõe o estado", async () => {
    const { rerender } = render(
      <MemoryRouter>
        <LocaleProvider initialLocale="pt-BR">
          <DeploymentLogs />
        </LocaleProvider>
      </MemoryRouter>,
    );
    const terminal = screen.getByTestId("deployment-log-terminal") as HTMLDivElement;
    Object.defineProperties(terminal, {
      clientHeight: { configurable: true, value: 100 },
      scrollHeight: { configurable: true, value: 1000 },
    });
    terminal.scrollTop = 100;
    fireEvent.scroll(terminal);

    const button = screen.getByRole("button", { name: "Ligar rolagem automática" });
    expect(button.getAttribute("aria-pressed")).toBe("false");

    terminal.scrollTo = vi.fn();
    queryMocks.logs.data = {
      ...queryMocks.logs.data,
      entries: [...queryMocks.logs.data.entries, {
        ...queryMocks.logs.data.entries[1],
        received_seq: 3,
        message: "não arrastar",
      }],
    };
    rerender(
      <MemoryRouter>
        <LocaleProvider initialLocale="pt-BR">
          <DeploymentLogs />
        </LocaleProvider>
      </MemoryRouter>,
    );
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(terminal.scrollTo).not.toHaveBeenCalled();
  });

  it("copia as últimas 500 linhas e confirma a ação", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: { writeText },
    });
    queryMocks.logs.data = {
      entries: Array.from({ length: 501 }, (_, index) => ({
        ...queryMocks.logs.data.entries[index % queryMocks.logs.data.entries.length],
        received_seq: index + 1,
        message: `linha-${index}`,
      })),
      dropped: 0,
    };
    render(
      <MemoryRouter>
        <LocaleProvider initialLocale="pt-BR">
          <DeploymentLogs />
        </LocaleProvider>
      </MemoryRouter>,
    );

    fireEvent.click(screen.getByRole("button", { name: "Copiar logs" }));

    await waitFor(() => expect(writeText).toHaveBeenCalledTimes(1));
    expect(writeText.mock.calls[0][0]).not.toContain("linha-0");
    expect(writeText.mock.calls[0][0]).toContain("linha-1");
    expect(writeText.mock.calls[0][0]).toContain("linha-500");
    expect(screen.getAllByText("Logs copiados").length).toBeGreaterThan(0);
  });

  it("mantém o layout normal fora da área preta do terminal", () => {
    render(
      <MemoryRouter>
        <LocaleProvider initialLocale="pt-BR">
          <DeploymentLogs />
        </LocaleProvider>
      </MemoryRouter>,
    );

    const terminal = screen.getByTestId("deployment-log-terminal");
    const page = terminal.closest(".bg-ink");
    expect(page).toBeTruthy();
    expect(page?.className).not.toContain("bg-[#050505]");
    expect(terminal.parentElement?.className).toContain("bg-black");
    expect(terminal.closest(".panel")?.className).not.toContain("bg-black");
  });
});
