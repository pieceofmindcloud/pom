// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { MemoryRouter } from "react-router-dom";
import { LocaleProvider } from "../i18n/provider";
import { ProfileProvider } from "../lib/profile";
import { fakeNode } from "../test/fixtures";

const mocks = vi.hoisted(() => ({
  post: vi.fn(),
  del: vi.fn(),
  refetch: vi.fn(),
  oldKey: {
    id: "old-id",
    key: "sk-old",
    label: "Inferência",
    node_id: "inference-node",
    active: true,
    created_at: "2026-08-31T00:00:00Z",
  },
  foreignKey: {
    id: "foreign-id",
    key: "sk-foreign",
    label: "Outra máquina",
    node_id: "other-node",
    active: true,
    created_at: "2026-08-31T00:00:00Z",
  },
}));

vi.mock("../lib/api", async (original) => ({
  ...(await original<typeof import("../lib/api")>()),
  api: { post: mocks.post, del: mocks.del },
}));

vi.mock("../lib/queries", () => ({
  useApiKeys: () => ({ data: [mocks.oldKey, mocks.foreignKey], isLoading: false, isError: false, isFetching: false, refetch: mocks.refetch }),
  useCluster: () => ({ data: { peers: [] } }),
  useNode: () => ({ data: fakeNode({ node_id: "inference-node", role: "node" }) }),
}));

vi.mock("../lib/toast", () => ({
  useToast: () => ({
    success: vi.fn(),
    fromError: vi.fn(),
  }),
}));

import { TokenKeys } from "./Keys";

function mount(profile: "admin" | "infer" = "infer") {
  return render(
    <MemoryRouter
      initialEntries={["/config/keys/api"]}
      future={{ v7_startTransition: true, v7_relativeSplatPath: true }}
    >
      <LocaleProvider initialLocale="pt-BR">
        <ProfileProvider initialProfile={profile}>
          <TokenKeys />
        </ProfileProvider>
      </LocaleProvider>
    </MemoryRouter>,
  );
}

describe("TokenKeys no perfil infer", () => {
  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it("mostra apenas a chave do próprio nó e esconde a seleção de outros nós", () => {
    mount();

    expect(screen.getByText("Inferência")).toBeTruthy();
    expect(screen.queryByText("Outra máquina")).toBeNull();
    expect(screen.queryByText("Toda a rede")).toBeNull();
    expect(screen.queryByLabelText("Nó da rede")).toBeNull();
  });

  it("revoga a chave anterior depois de gerar a nova", async () => {
    mocks.post.mockResolvedValueOnce({
      ...mocks.oldKey,
      id: "new-id",
      key: "sk-new",
      created_at: "2026-09-01T00:00:00Z",
    });
    mocks.del.mockResolvedValueOnce({ revoked: true });
    mount();

    fireEvent.click(screen.getByRole("button", { name: "Gerar nova chave" }));

    await waitFor(() => expect(mocks.del).toHaveBeenCalledWith("/admin/api-keys/sk-old"));
    expect(mocks.post).toHaveBeenCalledWith("/admin/api-keys", {
      label: "Inferência",
      node_id: "inference-node",
    });
    expect(mocks.refetch).toHaveBeenCalled();
  });

  it("abre a confirmação de revogação sem exigir o label duas vezes", () => {
    mount("admin");

    fireEvent.click(screen.getByRole("button", { name: "Revogar Inferência" }));

    expect(screen.getByRole("dialog", { name: "Revogar o token Inferência?" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Revogar", exact: true })).toBeTruthy();
  });
});
