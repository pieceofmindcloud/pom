// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { webcrypto } from "node:crypto";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import type { TrustAnchorView } from "../lib/types";
import { fingerprintFromTrustDocument } from "../lib/trustFingerprint";

vi.stubGlobal("crypto", webcrypto);

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
vi.mock("../lib/queries", () => ({ useTrustAnchor: () => anchorQuery, useNode: () => ({ data: undefined }) }));
vi.mock("../lib/toast", () => ({
  useToast: () => ({ success: vi.fn(), error: vi.fn(), info: vi.fn(), fromError: vi.fn() }),
}));

// Imported after the mocks: the screen reads `ApiError` in the module.
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
const CHAIN = JSON.stringify({ anchor: { principal: { pubkey: "public-key-from-admin" } } });

describe("TrustAnchor", () => {
  it("shows the fingerprint and the state of an anchored node, without a form", () => {
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
    // In blocks of eight, which is how one checks by eye.
    expect(screen.getByTestId("anchor-fingerprint").textContent?.replace(/\s/g, "")).toBe(
      FINGERPRINT,
    );
    expect(screen.getByText("cluster-do-operador")).toBeTruthy();
    expect(screen.queryByTestId("anchor-document")).toBeNull();
  });

  it("a node yet to be anchored shows the state and the form, with no computed fingerprint yet", () => {
    mount(view());

    expect(screen.getByTestId("anchor-state").textContent).toContain("Por ancorar");
    expect(screen.queryByTestId("anchor-fingerprint")).toBeNull();
    expect(screen.getByTestId("anchor-document")).toBeTruthy();
    expect(screen.getByTestId("anchor-fingerprint-confirm")).toBeTruthy();
  });

  it("computes the fingerprint of the document and makes the check explicit", async () => {
    mount(view());
    const chain = screen.getByTestId("anchor-document") as HTMLTextAreaElement;

    fireEvent.change(chain, {
      target: { value: CHAIN },
    });

    const fingerprint = await fingerprintFromTrustDocument(CHAIN);
    await waitFor(() => expect(screen.getByTestId("anchor-calculated-fingerprint").textContent?.replace(/\s/g, "")).toBe(fingerprint));
    expect((screen.getByTestId("anchor-fingerprint-confirm") as HTMLInputElement).checked).toBe(false);
  });

  it("sends the computed fingerprint after the operator confirmation", async () => {
    install.mockResolvedValue({ anchored: true, fingerprint: FINGERPRINT });
    mount(view());

    fireEvent.change(screen.getByTestId("anchor-document"), { target: { value: CHAIN } });
    const fingerprint = await fingerprintFromTrustDocument(CHAIN);
    await waitFor(() => expect(screen.getByTestId("anchor-calculated-fingerprint")).toBeTruthy());
    fireEvent.click(screen.getByTestId("anchor-fingerprint-confirm"));
    fireEvent.click(screen.getByRole("button", { name: /Instalar a raiz/i }));

    await waitFor(() => expect(install).toHaveBeenCalledWith(CHAIN, fingerprint));
    await waitFor(() => expect(anchorQuery.refetch).toHaveBeenCalled());
  });

  it("translates the wrong-fingerprint refusal instead of repeating the server text", async () => {
    install.mockRejectedValue(
      new FakeApiError(400, "a impressão digital conferida não corresponde à âncora desta cadeia", {
        reason: "fingerprint_mismatch",
      }),
    );
    mount(view());

    fireEvent.change(screen.getByTestId("anchor-document"), { target: { value: CHAIN } });
    await waitFor(() => expect(screen.getByTestId("anchor-calculated-fingerprint")).toBeTruthy());
    fireEvent.click(screen.getByTestId("anchor-fingerprint-confirm"));
    fireEvent.click(screen.getByRole("button", { name: /Instalar a raiz/i }));

    const refusal = await screen.findByTestId("anchor-refusal");
    expect(refusal.textContent).toContain("não corresponde à cadeia colada");
  });

  it("explains the refusal of an already anchored node", async () => {
    install.mockRejectedValue(
      new FakeApiError(409, "este nó já tem raiz de confiança", { reason: "already_anchored" }),
    );
    mount(view());

    fireEvent.change(screen.getByTestId("anchor-document"), { target: { value: CHAIN } });
    await waitFor(() => expect(screen.getByTestId("anchor-calculated-fingerprint")).toBeTruthy());
    fireEvent.click(screen.getByTestId("anchor-fingerprint-confirm"));
    fireEvent.click(screen.getByRole("button", { name: /Instalar a raiz/i }));

    const refusal = await screen.findByTestId("anchor-refusal");
    expect(refusal.textContent).toContain("reancoragem manual");
  });

  /** Viewing another machine's node, the screen does not offer what the server refuses. */
  it("does not draw the form outside loopback", () => {
    mount(view({ loopback: false, can_install: false }));

    expect(screen.queryByTestId("anchor-document")).toBeNull();
    expect(screen.getByText(/só é instalada localmente/i)).toBeTruthy();
  });
});
