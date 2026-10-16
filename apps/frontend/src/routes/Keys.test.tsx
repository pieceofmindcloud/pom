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
    key_hint: "sk-pom-0ld0…0ld0",
    label: "Inferência",
    node_id: "inference-node",
    active: true,
    created_at: "2026-08-31T00:00:00Z",
  },
  foreignKey: {
    id: "foreign-id",
    key_hint: "sk-pom-f0r3…1gn0",
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

describe("TokenKeys in the infer profile", () => {
  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it("shows only the key of the node itself and hides the selection of other nodes", () => {
    mount();

    expect(screen.getByText("Inferência")).toBeTruthy();
    expect(screen.queryByText("Outra máquina")).toBeNull();
    expect(screen.queryByText("Toda a rede")).toBeNull();
    expect(screen.queryByLabelText("Nó da rede")).toBeNull();
  });

  it("revokes the previous key after generating the new one", async () => {
    mocks.post.mockResolvedValueOnce({
      ...mocks.oldKey,
      id: "new-id",
      key: "sk-new",
      created_at: "2026-09-01T00:00:00Z",
    });
    mocks.del.mockResolvedValueOnce({ revoked: true });
    mount();

    fireEvent.click(screen.getByRole("button", { name: "Criar token" }));
    fireEvent.click(screen.getAllByRole("button", { name: "Criar token" }).at(-1)!);

    await waitFor(() => expect(mocks.del).toHaveBeenCalledWith("/admin/api-keys/old-id"));
    expect(mocks.post).toHaveBeenCalledWith("/admin/api-keys", {
      label: "Inferência",
      node_id: "inference-node",
    });
    expect(mocks.refetch).toHaveBeenCalled();
  });

  it("shows the new token only once, and in the list only the visible excerpt", async () => {
    mocks.post.mockResolvedValueOnce({
      ...mocks.oldKey,
      id: "new-id",
      key: "sk-pom-0123456789abcdef",
      key_hint: "sk-pom-0123…cdef",
    });
    mocks.del.mockResolvedValueOnce({ revoked: true });
    mount();

    expect(screen.getAllByTestId("key-hint").map((hint) => hint.textContent)).toEqual(["sk-pom-0ld0…0ld0"]);
    expect(screen.getByText("old-id")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Criar token" }));
    fireEvent.click(screen.getAllByRole("button", { name: "Criar token" }).at(-1)!);

    await waitFor(() => expect(screen.getByTestId("issued-secret").textContent).toBe("sk-pom-0123456789abcdef"));
    fireEvent.click(screen.getByRole("button", { name: "Já copiei" }));
    await waitFor(() => expect(screen.queryByTestId("issued-secret")).toBeNull());
  });

  it("opens the revocation confirmation without requiring the label twice", () => {
    mount("admin");

    fireEvent.click(screen.getByRole("button", { name: "Revogar Inferência" }));

    expect(screen.getByRole("dialog", { name: "Revogar o token Inferência?" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Revogar", exact: true })).toBeTruthy();
  });
});
