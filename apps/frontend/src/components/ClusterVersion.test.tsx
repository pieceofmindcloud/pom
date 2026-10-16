// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { ToastProvider } from "../lib/toast";
import { ClusterVersion } from "./ClusterVersion";

const mocks = vi.hoisted(() => ({ post: vi.fn(), status: { current: undefined as unknown }, invalidate: vi.fn() }));

vi.mock("../lib/api", () => ({ api: { post: mocks.post } }));
vi.mock("../lib/queries", () => ({
  useNode: () => ({ data: { cluster_version: mocks.status.current } }),
  useInvalidate: () => mocks.invalidate,
}));

function mount() {
  return render(
    <LocaleProvider initialLocale="pt-BR">
      <ToastProvider>
        <ClusterVersion />
      </ToastProvider>
    </LocaleProvider>,
  );
}

describe("ClusterVersion", () => {
  beforeEach(() => {
    mocks.post.mockReset();
    mocks.invalidate.mockReset();
  });
  afterEach(cleanup);

  it("shows the version and does not ask for a sync when current", () => {
    mocks.status.current = { version: "0.2.1.0-9f3c21ab77de0011", outdated: false, newer_on: [] };
    mount();
    expect(screen.getByTestId("cluster-version").textContent).toBe("0.2.1 · 9f3c21ab");
    expect(screen.getByText("Atualizada")).toBeTruthy();
    expect(mocks.post).not.toHaveBeenCalled();
  });

  it("re-syncs by itself once when a peer has a newer version", async () => {
    const outdated = { version: "0.1.0.0-aaaaaaaa00000000", outdated: true, newer_on: ["salem"] };
    mocks.status.current = outdated;
    mocks.post.mockResolvedValue({ ...outdated, updated: false });
    const view = mount();
    expect(screen.getByText("Desatualizada - sincronizando")).toBeTruthy();
    expect(screen.getByText(/Versão mais nova vista em: salem/)).toBeTruthy();
    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith("/api/ui/network/trust-sync"));
    await waitFor(() => expect(mocks.invalidate).toHaveBeenCalled());
    // Still outdated on the next render: no second automatic attempt.
    view.rerender(
      <LocaleProvider initialLocale="pt-BR">
        <ToastProvider>
          <ClusterVersion />
        </ToastProvider>
      </LocaleProvider>,
    );
    expect(mocks.post).toHaveBeenCalledTimes(1);
    // The button still asks again.
    fireEvent.click(screen.getByRole("button", { name: "Sincronizar agora" }));
    await waitFor(() => expect(mocks.post).toHaveBeenCalledTimes(2));
  });
});
