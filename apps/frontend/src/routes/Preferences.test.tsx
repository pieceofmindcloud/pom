// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";
import { ProfileProvider } from "../lib/profile";
import { LocaleProvider } from "../i18n/provider";
import { Preferences, ThermalPanel } from "./Preferences";

const mocks = vi.hoisted(() => ({
  get: vi.fn(),
  post: vi.fn(),
  saveHuggingFaceToken: vi.fn(),
  invalidate: vi.fn(),
  success: vi.fn(),
  fromError: vi.fn(),
  setAdminAccessToken: vi.fn(),
  clearAdminAccessToken: vi.fn(),
  logsPluginActive: true,
  passwordConfigured: false,
  node: {
    thermal: { limit_c: 85, hysteresis_c: 8, cooldown_secs: 30 },
    quic_enabled: false,
    quic_authority: true,
    cluster_logs_enabled: false,
    cluster_logs_authority: true,
  },
}));

vi.mock("../lib/api", () => ({
  api: { get: mocks.get, post: mocks.post },
  setAdminAccessToken: mocks.setAdminAccessToken,
  clearAdminAccessToken: mocks.clearAdminAccessToken,
  saveHuggingFaceToken: (...args: unknown[]) => mocks.saveHuggingFaceToken(...args),
}));

vi.mock("../lib/queries", () => ({
  useEnterpriseStatus: () => ({ data: { active: mocks.logsPluginActive }, isLoading: false }),
  useAdminAccessInfo: () => ({
    data: { admin_exposure: "localhost", gateway_port: 8080, password_configured: mocks.passwordConfigured },
    refetch: mocks.invalidate,
  }),
  useAdminAccess: () => ({ data: { default_password: false } }),
  useHuggingFaceTokenConfig: () => ({
    data: { configured: true, masked: "••••••••cret", source: "pom.json" },
    isLoading: false,
    isError: false,
  }),
  useInvalidate: () => mocks.invalidate,
  useLocalCa: () => ({ data: undefined, isError: true, refetch: mocks.invalidate }),
  useNode: () => ({ data: mocks.node }),
}));

vi.mock("../lib/toast", () => ({
  useToast: () => ({ success: mocks.success, fromError: mocks.fromError }),
}));

afterEach(() => {
  cleanup();
  mocks.get.mockReset();
  mocks.post.mockReset();
  mocks.saveHuggingFaceToken.mockReset();
  mocks.invalidate.mockReset();
  mocks.success.mockReset();
  mocks.fromError.mockReset();
  mocks.setAdminAccessToken.mockReset();
  mocks.clearAdminAccessToken.mockReset();
  mocks.get.mockResolvedValue({ enabled: false, active: false, searching: false, found: false, port: 0 });
  mocks.node.quic_enabled = false;
});

mocks.get.mockResolvedValue({ enabled: false, active: false, searching: false, found: false, port: 0 });

function mount(child: React.ReactNode) {
  return render(
    <LocaleProvider initialLocale="pt-BR">
      <MemoryRouter future={{ v7_startTransition: true, v7_relativeSplatPath: true }}>{child}</MemoryRouter>
    </LocaleProvider>,
  );
}

describe("Preferences", () => {
  it("shows the two settings that left Network", () => {
    mount(<Preferences />);

    expect(screen.getByText("Preferências")).toBeTruthy();
    expect(screen.getByText("Proteção térmica da GPU")).toBeTruthy();
    // The token moved to the Hugging Face plugin's Settings tab.
    expect(screen.queryByText("Hugging Face")).toBeNull();
  });

  it("in the infer profile, hides the GPU thermal protection and Hugging Face", () => {
    render(
      <LocaleProvider initialLocale="pt-BR">
        <ProfileProvider initialProfile="visitor">
          <MemoryRouter future={{ v7_startTransition: true, v7_relativeSplatPath: true }}><Preferences /></MemoryRouter>
        </ProfileProvider>
      </LocaleProvider>,
    );
    expect(screen.queryByText("Proteção térmica da GPU")).toBeNull();
    expect(screen.queryByText("Hugging Face")).toBeNull();
  });

  it("keeps the policy reading and saves changes to the thermal endpoint", async () => {
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

  it("shows QUIC next to TLS and saves the principal preference", async () => {
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

  it("keeps local discovery off and allows enabling it through the administrative preference", async () => {
    mocks.post.mockResolvedValue({ enabled: true, active: true, searching: false, found: false, port: 42777 });
    mount(<Preferences administrative />);

    await waitFor(() => expect(screen.getByText("Descoberta automática local")).toBeTruthy());
    fireEvent.click(screen.getByRole("switch", { name: "Descoberta automática local" }));

    await waitFor(() => {
      expect(mocks.post).toHaveBeenCalledWith("/api/ui/network/auto-discovery", { enabled: true });
    });
  });

  it("shows the process logs between nodes only with the Logs plugin active on this machine", () => {
    mount(<Preferences administrative />);
    expect(screen.getByText("Logs de processo entre nós")).toBeTruthy();

    cleanup();
    mocks.logsPluginActive = false;
    mount(<Preferences administrative />);
    expect(screen.queryByText("Logs de processo entre nós")).toBeNull();
    mocks.logsPluginActive = true;
  });

  it("shows the QUIC policy and no TLS switch: TLS between nodes is always on", () => {
    mocks.node.quic_enabled = false;
    mount(<Preferences administrative />);

    expect(screen.getByText("Transporte QUIC")).toBeTruthy();
    expect(screen.queryByText("Criptografia entre nós")).toBeNull();
  });

  it("moves the exposure alone and sends to the password screen when none is set", async () => {
    mount(<Preferences />);
    expect(screen.getByTestId("preferences-change-password").getAttribute("href")).toBe("/config/password");
    expect(screen.queryByLabelText(/nova senha/i)).toBeNull();

    fireEvent.change(screen.getByLabelText(/quem pode abrir a interface/i), {
      target: { value: "network" },
    });
    expect(screen.getByText(/defina uma senha do painel/i)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /salvar exposição/i }));
    expect(mocks.post).not.toHaveBeenCalledWith("/api/ui/access", expect.anything());
    cleanup();

    mocks.passwordConfigured = true;
    mocks.post.mockResolvedValue({ saved: true, restart: false, admin_exposure: "network", gateway_port: 8080, password_configured: true });
    mount(<Preferences />);
    fireEvent.change(screen.getByLabelText(/quem pode abrir a interface/i), {
      target: { value: "network" },
    });
    fireEvent.click(screen.getByRole("button", { name: /salvar exposição/i }));
    await waitFor(() => {
      expect(mocks.post).toHaveBeenCalledWith("/api/ui/access", { admin_exposure: "network", gateway_port: 8080 });
    });
    mocks.passwordConfigured = false;
  });

});

describe("ThermalPanel", () => {
  it("accepts exactly 70 °C and 60 seconds", () => {
    const onSave = vi.fn();
    mount(
      <ThermalPanel
        policy={{ limit_c: 85, hysteresis_c: 8, cooldown_secs: 30 }}
        onSave={onSave}
      />,
    );

    fireEvent.change(screen.getByLabelText(/desimplantar acima/i), { target: { value: "70" } });
    fireEvent.change(screen.getByLabelText(/tempo de resfriamento/i), { target: { value: "60" } });
    fireEvent.click(screen.getByRole("button", { name: /salvar política térmica/i }));

    expect(onSave).toHaveBeenCalledWith({ limit_c: 70, hysteresis_c: 8, cooldown_secs: 60 });
  });

  it("refuses a cutoff temperature below 70 °C", () => {
    const onSave = vi.fn();
    mount(
      <ThermalPanel
        policy={{ limit_c: 85, hysteresis_c: 8, cooldown_secs: 30 }}
        onSave={onSave}
      />,
    );

    fireEvent.change(screen.getByLabelText(/desimplantar acima/i), { target: { value: "69" } });
    fireEvent.click(screen.getByRole("button", { name: /salvar política térmica/i }));

    expect(onSave).not.toHaveBeenCalled();
    expect(screen.getByText(/temperatura de corte deve ser de pelo menos 70 °C/i)).toBeTruthy();
  });

  it("refuses a cooldown time above 60 seconds", () => {
    const onSave = vi.fn();
    mount(
      <ThermalPanel
        policy={{ limit_c: 85, hysteresis_c: 8, cooldown_secs: 30 }}
        onSave={onSave}
      />,
    );

    fireEvent.change(screen.getByLabelText(/tempo de resfriamento/i), { target: { value: "61" } });
    fireEvent.click(screen.getByRole("button", { name: /salvar política térmica/i }));

    expect(onSave).not.toHaveBeenCalled();
    expect(screen.getByText(/tempo de resfriamento deve ser de no máximo 60 segundos/i)).toBeTruthy();
  });

  it("keeps sending the edited policy when used standalone", () => {
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
