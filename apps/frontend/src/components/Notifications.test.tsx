// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { BrowserRouter } from "react-router-dom";
import { LocaleProvider } from "../i18n/provider";
import { api } from "../lib/api";
import type { NetworkOffer } from "../lib/types";
import { Notifications } from "./Notifications";

const queryMocks = vi.hoisted(() => ({
  jobs: { data: { jobs: [] } },
  transfers: { data: { jobs: [] }, refetch: vi.fn() },
  offers: { data: { offers: [] as NetworkOffer[] }, refetch: vi.fn() },
}));

vi.mock("../lib/api", () => ({ api: { post: vi.fn() } }));
vi.mock("../lib/queries", () => ({
  useJobs: () => queryMocks.jobs,
  useTransfers: () => queryMocks.transfers,
  useOffers: () => queryMocks.offers,
}));
vi.mock("../lib/toast", () => ({
  useToast: () => ({ success: vi.fn(), fromError: vi.fn() }),
}));

function renderNotifications(offers: NetworkOffer[]) {
  queryMocks.offers.data = { offers };
  return render(
    <BrowserRouter future={{ v7_startTransition: true, v7_relativeSplatPath: true }}>
      <LocaleProvider initialLocale="pt-BR">
        <Notifications />
      </LocaleProvider>
    </BrowserRouter>,
  );
}

const principalTransfer: NetworkOffer = {
  kind: "principal_transfer",
  id: "transfer-1",
  from_node_id: "principal-1",
  from_host: "captain",
  target_node_id: "secondary-1",
  target_host: "salem",
  status: "pending",
  issued_at: "2026-09-07T00:00:00Z",
};

describe("Notifications network offers", () => {
  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it("shows principal transfer actions and preserves the no-change warning", async () => {
    vi.mocked(api.post).mockResolvedValue(undefined);
    renderNotifications([principalTransfer]);

    fireEvent.click(screen.getByRole("button", { name: /atividade/i }));
    expect(screen.getByText("Papel de principal oferecido")).toBeTruthy();
    expect(screen.getByText(/até você aceitar/i)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Aceitar" }));

    await waitFor(() =>
      expect(vi.mocked(api.post)).toHaveBeenCalledWith("/api/transfer/offers/transfer-1/accept"),
    );
  });

  it("shows a node relinquish notice without approval controls", () => {
    renderNotifications([
      {
        kind: "node_relinquish_notice",
        id: "notice-1",
        from_node_id: "node-1",
        from_host: "salem",
        target_node_id: "node-1",
        target_host: "salem",
        status: "notice",
        issued_at: "2026-09-07T00:00:00Z",
        active_model: "qwen",
      },
    ]);

    fireEvent.click(screen.getByRole("button", { name: /atividade/i }));
    expect(screen.getByText("Nó deixando o papel de node")).toBeTruthy();
    expect(screen.getByText(/inferência nova vai para outro lugar/i)).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Aceitar" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Recusar" })).toBeNull();
  });

  it("shows a rejected principal transfer as a terminal outcome", () => {
    renderNotifications([{ ...principalTransfer, status: "rejected" }]);

    fireEvent.click(screen.getByRole("button", { name: /atividade/i }));
    expect(screen.getByText("Transferência de principal recusada")).toBeTruthy();
    expect(screen.getByText(/o papel de principal não mudou/i)).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Aceitar" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Recusar" })).toBeNull();
  });
});
