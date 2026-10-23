// @vitest-environment jsdom
/**
 * The panel password screen: with the default in force only the new password
 * twice; afterwards the current one too. Nothing is sent until the new one is
 * strong and both copies match.
 */
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";

const mocks = vi.hoisted(() => ({
  post: vi.fn(),
  setAdminAccessToken: vi.fn(),
  defaultPassword: true,
}));

vi.mock("../lib/api", async (original) => ({
  ...(await original<typeof import("../lib/api")>()),
  api: { post: mocks.post },
  setAdminAccessToken: mocks.setAdminAccessToken,
}));
vi.mock("../lib/queries", async (original) => ({
  ...(await original<typeof import("../lib/queries")>()),
  useAdminAccess: () => ({ data: { default_password: mocks.defaultPassword }, isLoading: false }),
}));
vi.mock("../lib/toast", () => ({ useToast: () => ({ success: vi.fn(), fromError: vi.fn() }) }));

const { ChangePassword, passwordChecks } = await import("./ChangePassword");

function mount() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <LocaleProvider initialLocale="en">
        <MemoryRouter initialEntries={["/config/password"]} future={{ v7_startTransition: true, v7_relativeSplatPath: true }}>
          <ChangePassword />
        </MemoryRouter>
      </LocaleProvider>
    </QueryClientProvider>,
  );
}

const type = (id: string, value: string) => fireEvent.change(screen.getByTestId(id), { target: { value } });
const save = () => screen.getByText("Save password").closest("button") as HTMLButtonElement;

afterEach(() => {
  cleanup();
  mocks.post.mockReset();
  mocks.defaultPassword = true;
});

describe("ChangePassword", () => {
  it("asks only the new password twice while the default is in force", async () => {
    mocks.post.mockResolvedValue({ saved: true });
    mount();
    expect(screen.queryByTestId("password-current")).toBeNull();

    type("password-new", "short");
    expect(save().disabled).toBe(true);
    type("password-new", "Strong pass 42");
    type("password-confirm", "Strong pass 4");
    expect(screen.getByTestId("password-mismatch")).toBeTruthy();
    expect(save().disabled).toBe(true);

    type("password-confirm", "Strong pass 42");
    fireEvent.click(save());
    await waitFor(() =>
      expect(mocks.post).toHaveBeenCalledWith("/api/ui/access/password", { new_password: "Strong pass 42" }),
    );
    expect(mocks.setAdminAccessToken).toHaveBeenCalledWith("Strong pass 42");
  });

  it("asks the current password once the default is gone", async () => {
    mocks.defaultPassword = false;
    mocks.post.mockResolvedValue({ saved: true });
    mount();
    type("password-new", "Strong pass 42");
    type("password-confirm", "Strong pass 42");
    expect(save().disabled).toBe(true);
    type("password-current", "old one");
    fireEvent.click(save());
    await waitFor(() =>
      expect(mocks.post).toHaveBeenCalledWith("/api/ui/access/password", {
        current_password: "old one",
        new_password: "Strong pass 42",
      }),
    );
  });

  it("checks the same rule as the node", () => {
    expect(passwordChecks("admin").notDefault).toBe(false);
    expect(passwordChecks("Ab1!short").length).toBe(false);
    expect(passwordChecks("alllowercase").mixed).toBe(false);
    expect(passwordChecks("lower1UPPER")).toEqual({ length: true, mixed: true, notDefault: true });
  });
});
