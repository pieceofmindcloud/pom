// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { BrowserRouter } from "react-router-dom";
import { LocaleProvider } from "../i18n/provider";
import { api } from "../lib/api";
import type { NetworkOffer } from "../lib/types";
import { Notifications } from "./Notifications";
import { addPendingPluginConfirm, addPluginNotice, pendingPluginConfirms, pluginNotices, resetPluginInboxForTest } from "../lib/pluginInbox";
import { resetDefaultPasswordNoticeForTest } from "../lib/defaultPasswordNotice";

const queryMocks = vi.hoisted(() => ({
  access: { data: { default_password: false, boot_id: "boot-1" } },
  jobs: { data: { jobs: [] } },
  transfers: { data: { jobs: [] }, refetch: vi.fn() },
  offers: { data: { offers: [] as NetworkOffer[] }, refetch: vi.fn() },
  jobsEnabled: [] as boolean[],
  transfersEnabled: [] as boolean[],
  offersEnabled: [] as boolean[],
}));

const profileMock = vi.hoisted(() => ({ profile: "admin" as string | undefined }));
vi.mock("../lib/profile", () => ({ useProfile: () => ({ profile: profileMock.profile }) }));

const admissionMocks = vi.hoisted(() => ({
  admissions: { data: [] as unknown[], refetch: vi.fn() },
  requests: { data: [] as unknown[], refetch: vi.fn() },
  api: { decide: vi.fn(), join: vi.fn(), forget: vi.fn() },
}));

vi.mock("../lib/api", () => ({ api: { post: vi.fn() } }));
vi.mock("../lib/queries", () => ({
  useAdminAccess: () => queryMocks.access,
  useJobs: (enabled = true) => {
    queryMocks.jobsEnabled.push(enabled);
    return queryMocks.jobs;
  },
  useTransfers: (enabled = true) => {
    queryMocks.transfersEnabled.push(enabled);
    return queryMocks.transfers;
  },
  useOffers: (enabled = true) => {
    queryMocks.offersEnabled.push(enabled);
    return queryMocks.offers;
  },
}));
vi.mock("../lib/admissions", () => ({
  useAdmissions: () => admissionMocks.admissions,
  useAdmissionRequests: () => admissionMocks.requests,
  admissionsApi: admissionMocks.api,
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
    resetPluginInboxForTest();
    resetDefaultPasswordNoticeForTest();
    queryMocks.access.data = { default_password: false, boot_id: "boot-1" };
    queryMocks.jobsEnabled = [];
    queryMocks.jobs.data = { jobs: [] };
    localStorage.clear();
    queryMocks.transfersEnabled = [];
    queryMocks.offersEnabled = [];
    profileMock.profile = "admin";
  });

  it("does not poll the job and transfer routes a visitor's node refuses", () => {
    profileMock.profile = "visitor";
    renderNotifications([]);
    expect(queryMocks.jobsEnabled).not.toContain(true);
    expect(queryMocks.transfersEnabled).not.toContain(true);
    expect(queryMocks.offersEnabled).not.toContain(true);
    cleanup();

    profileMock.profile = "worker";
    renderNotifications([]);
    expect(queryMocks.jobsEnabled.at(-1)).toBe(true);
    expect(queryMocks.transfersEnabled.at(-1)).toBe(true);
    expect(queryMocks.offersEnabled.at(-1)).toBe(true);
  });

  it("warns about the default password once per start of the node", () => {
    queryMocks.access.data = { default_password: true, boot_id: "boot-1" };
    const first = renderNotifications([]);
    const bell = () => screen.getByRole("button", { name: /atividade/i });
    expect(bell().textContent).toBe("1");

    // Opening the bell reads it: it stays listed and stops counting.
    fireEvent.click(bell());
    expect(screen.getByText("Senha administrativa padrão")).toBeTruthy();
    expect(screen.getByText(/ainda usa a senha padrão/)).toBeTruthy();
    expect(screen.queryByText(/\(admin\)/)).toBeNull();
    expect(screen.getByRole("button", { name: "Trocar senha" })).toBeTruthy();
    fireEvent.click(bell());
    expect(bell().textContent).toBe("");

    // A page reload on the same start does not bring it back as new.
    first.unmount();
    const reloaded = renderNotifications([]);
    expect(bell().textContent).toBe("");

    // Dismissed, it leaves the bell for this start.
    fireEvent.click(bell());
    fireEvent.click(screen.getByRole("button", { name: "Remover aviso" }));
    expect(screen.queryByTestId("default-password-notice")).toBeNull();

    // The node restarted: the notice is sent again.
    reloaded.unmount();
    queryMocks.access.data = { default_password: true, boot_id: "boot-2" };
    renderNotifications([]);
    expect(bell().textContent).toBe("1");
  });

  it("sends no password notice once the password was changed", () => {
    renderNotifications([]);
    fireEvent.click(screen.getByRole("button", { name: /atividade/i }));
    expect(screen.queryByTestId("default-password-notice")).toBeNull();
  });

  it("shows a plugin notice without actions and marks it read when the bell opens", () => {
    addPluginNotice({
      id: "n1",
      code: "base",
      title: "Deploy concluído",
      body: "O modelo está no ar.",
      level: "success",
      createdAt: new Date().toISOString(),
      source: "network",
    });
    renderNotifications([]);

    const bell = screen.getByRole("button", { name: /atividade/i });
    expect(bell.textContent).toContain("1");
    fireEvent.click(bell);
    expect(screen.getByText("Deploy concluído")).toBeTruthy();
    expect(screen.getByText("O modelo está no ar.")).toBeTruthy();
    expect(screen.getByText("Nova")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Aceitar" })).toBeNull();
    expect(pluginNotices()[0].read).toBe(true);
    expect(bell.textContent).not.toContain("1");

    fireEvent.click(screen.getByRole("button", { name: "Remover aviso" }));
    expect(pluginNotices()).toEqual([]);
  });

  it("keeps a deploy that just finished, unread until the bell opens, and lets it be removed", () => {
    queryMocks.jobs.data = {
      jobs: [{
        id: "deploy-1",
        kind: "apply",
        state: "done",
        title: "Implantar qwen",
        step: "pronto",
        message: "",
        progress: 100,
        outputs: [],
        started_at: new Date(Date.now() - 20_000).toISOString(),
        finished_at: new Date().toISOString(),
      }],
    } as never;
    renderNotifications([]);

    const bell = screen.getByRole("button", { name: /atividade/i });
    expect(bell.textContent).toContain("1");
    fireEvent.click(bell);
    // The tray is not empty: the outcome is there, and opening read it.
    expect(screen.getByText("Implantar qwen")).toBeTruthy();
    expect(screen.getByText(/terminou/)).toBeTruthy();
    expect(bell.textContent).not.toContain("1");

    fireEvent.click(screen.getByRole("button", { name: "Remover do sino" }));
    expect(screen.queryByText("Implantar qwen")).toBeNull();
  });

  it("shows why a finished job failed", () => {
    queryMocks.jobs.data = {
      jobs: [{
        id: "deploy-2",
        kind: "apply",
        state: "failed",
        title: "Implantar qwen",
        step: "carregando",
        message: "a máquina ksia não respondeu",
        progress: null,
        outputs: [],
        started_at: new Date(Date.now() - 20_000).toISOString(),
        finished_at: new Date().toISOString(),
      }],
    } as never;
    renderNotifications([]);
    fireEvent.click(screen.getByRole("button", { name: /atividade/i }));
    expect(screen.getByText("a máquina ksia não respondeu")).toBeTruthy();
    expect(screen.getByText(/falhou/)).toBeTruthy();
  });

  it("lists a plugin confirmation with accept and reject, like a transfer offer", () => {
    const resolve = vi.fn();
    addPendingPluginConfirm(
      { id: "c1", code: "base", request: { title: "Continuar o tutorial?", body: "Pergunta do plugin." }, createdAt: "now" },
      resolve,
    );
    renderNotifications([]);

    fireEvent.click(screen.getByRole("button", { name: /atividade/i }));
    expect(screen.getByText("Continuar o tutorial?")).toBeTruthy();
    expect(screen.getByText("Pergunta do plugin.")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Recusar" }));

    expect(resolve).toHaveBeenCalledWith("cancelled");
    expect(pendingPluginConfirms()).toEqual([]);
    expect(api.post).not.toHaveBeenCalled();
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

  it("shows a rejected principal transfer as a terminal outcome", () => {
    renderNotifications([{ ...principalTransfer, status: "rejected" }]);

    fireEvent.click(screen.getByRole("button", { name: /atividade/i }));
    expect(screen.getByText("Transferência de principal recusada")).toBeTruthy();
    expect(screen.getByText(/o papel de principal não mudou/i)).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Aceitar" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Recusar" })).toBeNull();
  });

  it("shows an admission request with what the admin needs to decide, and opens its thread", async () => {
    admissionMocks.admissions.data = [{
      node_id: "candidate-node-id-0001",
      role: "worker",
      status: "pending",
      requested_at: "2026-10-05T12:00:00Z",
      card: {
        hostname: "forasteiro", os: "linux", arch: "x86_64", cpu_count: 16, ram_total_mb: 65536,
        gpus: [{ name: "RTX 5060 Ti", vram_total_mb: 16384 }], rtt_ms: 42, relayed: true,
      },
    }, {
      node_id: "already-decided", role: "visitor", status: "approved", requested_at: "2026-10-05T11:00:00Z",
      card: { hostname: "antigo", os: "", arch: "", cpu_count: 0, ram_total_mb: 0, gpus: [] },
    }];
    admissionMocks.api.decide.mockResolvedValue([]);
    renderNotifications([]);
    fireEvent.click(screen.getByRole("button", { name: /atividade/i }));

    const item = screen.getByTestId("admission-candidate-node-id-0001");
    expect(item.textContent).toContain("forasteiro pede para entrar nesta rede");
    // Role, how it connects, latency and hardware, in one line.
    expect(item.textContent).toContain("Worker");
    expect(item.textContent).toContain("por um relé");
    expect(item.textContent).toContain("42 ms");
    expect(item.textContent).toContain("64 GB de RAM");
    expect(item.textContent).toContain("RTX 5060 Ti (16 GB VRAM)");
    // A request that was already decided is not asked again.
    expect(screen.queryByTestId("admission-already-decided")).toBeNull();

    // The decision is taken in the candidate's thread, not here.
    expect(screen.queryByRole("button", { name: "Aprovar" })).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Abrir a conversa" }));
    expect(window.location.search).toBe("?lobby=candidate-node-id-0001");
    expect(admissionMocks.api.decide).not.toHaveBeenCalled();
    admissionMocks.admissions.data = [];
  });

  it("tells the candidate it was approved and joins only when asked to", async () => {
    admissionMocks.requests.data = [
      { key: "pk:curitiba", name: "Rede Curitiba", role: "worker", status: "approved" },
      { key: "pk:waiting", name: "Rede Espera", role: "worker", status: "pending" },
      // Admitted before and never asked again: not an answer, so not news.
      { key: "pk:antiga", name: "Rede Antiga", role: "worker", status: "approved", standing: true },
    ];
    admissionMocks.api.join.mockResolvedValue({ joined: true });
    renderNotifications([]);
    fireEvent.click(screen.getByRole("button", { name: /atividade/i }));

    expect(screen.getByText("Rede Curitiba aprovou esta máquina")).toBeTruthy();
    expect(screen.queryByTestId("admission-answer-pk:waiting")).toBeNull();
    expect(screen.queryByTestId("admission-answer-pk:antiga")).toBeNull();
    expect(screen.getAllByRole("button", { name: "Entrar agora" }).length).toBe(1);
    expect(admissionMocks.api.join).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Entrar agora" }));
    await waitFor(() => expect(admissionMocks.api.join).toHaveBeenCalledWith("pk:curitiba"));
    admissionMocks.requests.data = [];
  });
});
