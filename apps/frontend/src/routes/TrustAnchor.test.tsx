// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import type { TrustAnchorView } from "../lib/types";

const install = vi.fn();
const anchorQuery = {
  data: undefined as TrustAnchorView | undefined,
  isError: false,
  isLoading: false,
  error: null as unknown,
  refetch: vi.fn(),
};

class FakeApiError extends Error {
  constructor(
    readonly status: number,
    message: string,
    readonly body?: unknown,
  ) {
    super(message);
  }
}

vi.mock("../lib/api", () => ({ ApiError: FakeApiError }));
vi.mock("../lib/adminAuthority", () => ({ trustAnchorApi: { install: (...args: unknown[]) => install(...args) } }));
vi.mock("../lib/queries", () => ({ useTrustAnchor: () => anchorQuery }));
vi.mock("../lib/toast", () => ({
  useToast: () => ({ success: vi.fn(), error: vi.fn(), info: vi.fn(), fromError: vi.fn() }),
}));

// Importado depois dos mocks: a tela lê `ApiError` no módulo.
const { TrustAnchor } = await import("./TrustAnchor");

const view = (overrides: Partial<TrustAnchorView> = {}): TrustAnchorView => ({
  anchored: false,
  fingerprint: null,
  cluster_id: null,
  anchor_node_id: null,
  is_anchor: false,
  loopback: true,
  can_install: true,
  trust_filename: ".pom-admin-trust.json",
  ...overrides,
});

function mount(state: TrustAnchorView) {
  anchorQuery.data = state;
  anchorQuery.isError = false;
  anchorQuery.isLoading = false;
  return render(
    <LocaleProvider initialLocale="pt-BR">
      <TrustAnchor />
    </LocaleProvider>,
  );
}

afterEach(() => {
  cleanup();
  install.mockReset();
  anchorQuery.refetch.mockReset();
});

const FINGERPRINT = "a".repeat(64);

describe("TrustAnchor", () => {
  it("mostra a impressão digital e o estado de um nó ancorado, sem formulário", () => {
    mount(
      view({
        anchored: true,
        is_anchor: true,
        fingerprint: FINGERPRINT,
        cluster_id: "cluster-do-operador",
        anchor_node_id: "ksia-node-id",
        can_install: false,
      }),
    );

    expect(screen.getByTestId("anchor-state").textContent).toContain("Ancorado");
    // Em blocos de oito, que é como se confere a olho.
    expect(screen.getByTestId("anchor-fingerprint").textContent?.replace(/\s/g, "")).toBe(
      FINGERPRINT,
    );
    expect(screen.getByText("cluster-do-operador")).toBeTruthy();
    expect(screen.queryByTestId("anchor-document")).toBeNull();
  });

  it("um nó por ancorar mostra o estado e o formulário, sem impressão digital", () => {
    mount(view());

    expect(screen.getByTestId("anchor-state").textContent).toContain("Por ancorar");
    expect(screen.queryByTestId("anchor-fingerprint")).toBeNull();
    expect(screen.getByTestId("anchor-document")).toBeTruthy();
    expect(screen.getByTestId("anchor-typed-fingerprint")).toBeTruthy();
  });

  /** O campo é o que valida o arquivo: pré-preenchê-lo anularia a conferência. */
  it("nunca pré-preenche a impressão digital a partir do conteúdo colado", () => {
    mount(view());
    const chain = screen.getByTestId("anchor-document") as HTMLTextAreaElement;
    const typed = screen.getByTestId("anchor-typed-fingerprint") as HTMLInputElement;

    fireEvent.change(chain, {
      target: { value: JSON.stringify({ anchor: { fingerprint: FINGERPRINT } }) },
    });

    expect(typed.value).toBe("");
  });

  it("envia o que o operador digitou, sem tocar no valor", async () => {
    install.mockResolvedValue({ anchored: true, fingerprint: FINGERPRINT });
    mount(view());

    fireEvent.change(screen.getByTestId("anchor-document"), { target: { value: "{\"a\":1}" } });
    fireEvent.change(screen.getByTestId("anchor-typed-fingerprint"), {
      target: { value: FINGERPRINT },
    });
    fireEvent.click(screen.getByRole("button", { name: /Instalar a raiz/i }));

    await waitFor(() => expect(install).toHaveBeenCalledWith("{\"a\":1}", FINGERPRINT));
    await waitFor(() => expect(anchorQuery.refetch).toHaveBeenCalled());
  });

  it("traduz a recusa por impressão digital errada em vez de repetir o texto do servidor", async () => {
    install.mockRejectedValue(
      new FakeApiError(400, "a impressão digital conferida não corresponde à âncora desta cadeia", {
        reason: "fingerprint_mismatch",
      }),
    );
    mount(view());

    fireEvent.change(screen.getByTestId("anchor-document"), { target: { value: "{}" } });
    fireEvent.change(screen.getByTestId("anchor-typed-fingerprint"), {
      target: { value: "b".repeat(64) },
    });
    fireEvent.click(screen.getByRole("button", { name: /Instalar a raiz/i }));

    const refusal = await screen.findByTestId("anchor-refusal");
    expect(refusal.textContent).toContain("não corresponde à cadeia colada");
  });

  it("explica a recusa de um nó já ancorado", async () => {
    install.mockRejectedValue(
      new FakeApiError(409, "este nó já tem raiz de confiança", { reason: "already_anchored" }),
    );
    mount(view());

    fireEvent.change(screen.getByTestId("anchor-document"), { target: { value: "{}" } });
    fireEvent.change(screen.getByTestId("anchor-typed-fingerprint"), { target: { value: "x" } });
    fireEvent.click(screen.getByRole("button", { name: /Instalar a raiz/i }));

    const refusal = await screen.findByTestId("anchor-refusal");
    expect(refusal.textContent).toContain("reancoragem manual");
  });

  /** Vendo o nó de outra máquina, a tela não oferece o que o servidor recusa. */
  it("não desenha o formulário fora do loopback", () => {
    mount(view({ loopback: false, can_install: false }));

    expect(screen.queryByTestId("anchor-document")).toBeNull();
    expect(screen.getByText(/só é instalada localmente/i)).toBeTruthy();
  });
});
