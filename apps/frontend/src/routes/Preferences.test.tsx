// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { Preferences, ThermalPanel } from "./Preferences";

const mocks = vi.hoisted(() => ({
  post: vi.fn(),
  saveHuggingFaceToken: vi.fn(),
  invalidate: vi.fn(),
  success: vi.fn(),
  fromError: vi.fn(),
  setAdminAccessToken: vi.fn(),
  clearAdminAccessToken: vi.fn(),
}));

vi.mock("../lib/api", () => ({
  api: { post: mocks.post },
  setAdminAccessToken: mocks.setAdminAccessToken,
  clearAdminAccessToken: mocks.clearAdminAccessToken,
  saveHuggingFaceToken: (...args: unknown[]) => mocks.saveHuggingFaceToken(...args),
}));

vi.mock("../lib/queries", () => ({
  useAdminAccessInfo: () => ({
    data: { admin_exposure: "localhost", gateway_port: 8080, access_token: null },
    refetch: mocks.invalidate,
  }),
  useHuggingFaceTokenConfig: () => ({
    data: { configured: true, masked: "••••••••cret", source: "pom.json" },
    isLoading: false,
    isError: false,
  }),
  useInvalidate: () => mocks.invalidate,
  useNode: () => ({
    data: {
      thermal: { limit_c: 85, hysteresis_c: 8, cooldown_secs: 30 },
      tls_required: false,
      tls_authority: true,
      quic_enabled: false,
      quic_authority: true,
      cluster_logs_enabled: false,
      cluster_logs_authority: true,
    },
  }),
}));

vi.mock("../lib/toast", () => ({
  useToast: () => ({ success: mocks.success, fromError: mocks.fromError }),
}));

afterEach(() => {
  cleanup();
  mocks.post.mockReset();
  mocks.saveHuggingFaceToken.mockReset();
  mocks.invalidate.mockReset();
  mocks.success.mockReset();
  mocks.fromError.mockReset();
  mocks.setAdminAccessToken.mockReset();
  mocks.clearAdminAccessToken.mockReset();
});

function mount(child: React.ReactNode) {
  return render(
    <LocaleProvider initialLocale="pt-BR">
      <MemoryRouter>{child}</MemoryRouter>
    </LocaleProvider>,
  );
}

describe("Preferences", () => {
  it("mostra as duas configurações que saíram de Rede", () => {
    mount(<Preferences />);

    expect(screen.getByText("Preferências")).toBeTruthy();
    expect(screen.getByText("Proteção térmica da GPU")).toBeTruthy();
    expect(screen.getByText("Hugging Face")).toBeTruthy();
    expect(screen.getByText("••••••••cret")).toBeTruthy();
  });

  it("mantém a leitura da política e salva alterações no endpoint térmico", async () => {
    mocks.post.mockResolvedValue({ limit_c: 82, hysteresis_c: 7, cooldown_secs: 45 });
    mount(<Preferences />);

    expect((screen.getByLabelText(/desimplantar acima/i) as HTMLInputElement).value).toBe("85");
    fireEvent.change(screen.getByLabelText(/desimplantar acima/i), { target: { value: "82" } });
    fireEvent.change(screen.getByLabelText(/margem de segurança/i), { target: { value: "7" } });
    fireEvent.change(screen.getByLabelText(/tempo de resfriamento/i), { target: { value: "45" } });
    fireEvent.click(screen.getByRole("button", { name: /salvar política térmica/i }));

    await waitFor(() => {
      expect(mocks.post).toHaveBeenCalledWith("/api/ui/thermal", {
        limit_c: 82,
        hysteresis_c: 7,
        cooldown_secs: 45,
      });
    });
  });

  it("mostra QUIC ao lado de TLS e salva a preferência do principal", async () => {
    mocks.post.mockResolvedValue({ quic_enabled: true });
    mount(<Preferences administrative />);

    expect(screen.getByText("Transporte QUIC")).toBeTruthy();
    fireEvent.click(screen.getByRole("switch", { name: /ligar quic/i }));

    await waitFor(() => {
      expect(mocks.post).toHaveBeenCalledWith("/api/ui/network/quic", {
        quic_enabled: true,
      });
    });
  });

  it("gera e guarda o token quando a interface passa para a rede", async () => {
    mocks.post.mockResolvedValue({
      saved: true,
      restart: false,
      admin_exposure: "network",
      gateway_port: 8080,
      access_token: "new-admin-token",
    });
    mount(<Preferences />);

    fireEvent.change(screen.getByLabelText(/quem pode abrir a interface/i), {
      target: { value: "network" },
    });
    fireEvent.click(screen.getByRole("button", { name: /salvar exposição/i }));

    await waitFor(() => {
      expect(mocks.post).toHaveBeenCalledWith("/api/ui/access", {
        admin_exposure: "network",
        gateway_port: 8080,
        regenerate_token: false,
      });
      expect(mocks.setAdminAccessToken).toHaveBeenCalledWith("new-admin-token");
    });
  });

  it("mantém o salvamento da credencial do Hugging Face", async () => {
    mocks.saveHuggingFaceToken.mockResolvedValue({
      configured: true,
      masked: "••••••••cret",
      source: "pom.json",
    });
    mount(<Preferences />);

    fireEvent.change(screen.getByLabelText(/token de acesso/i), {
      target: { value: "hf_super_secret" },
    });
    fireEvent.click(screen.getByRole("button", { name: /salvar token/i }));

    await waitFor(() => {
      expect(mocks.saveHuggingFaceToken).toHaveBeenCalledWith("hf_super_secret");
    });
  });
});

describe("ThermalPanel", () => {
  it("continua enviando a política editada quando usado isoladamente", () => {
    const onSave = vi.fn();
    mount(
      <ThermalPanel
        policy={{ limit_c: 85, hysteresis_c: 8, cooldown_secs: 30 }}
        onSave={onSave}
      />,
    );

    fireEvent.change(screen.getByLabelText(/desimplantar acima/i), { target: { value: "82" } });
    fireEvent.change(screen.getByLabelText(/margem de segurança/i), { target: { value: "7" } });
    fireEvent.change(screen.getByLabelText(/tempo de resfriamento/i), { target: { value: "45" } });
    fireEvent.click(screen.getByRole("button", { name: /salvar política térmica/i }));

    expect(onSave).toHaveBeenCalledWith({ limit_c: 82, hysteresis_c: 7, cooldown_secs: 45 });
    expect(screen.getByText(/limite de retomada automática: 75 °C/i)).toBeTruthy();
  });
});
