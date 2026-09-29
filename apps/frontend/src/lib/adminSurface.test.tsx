// @vitest-environment jsdom
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderHook, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { ReactNode } from "react";
import { api, ApiError, getAdminAccessToken, setAdminAccessToken } from "./api";
import { DEFAULT_ADMIN_PASSWORD, useAdminSurface } from "./queries";

function wrapper({ children }: { children: ReactNode }) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return <QueryClientProvider client={client}>{children}</QueryClientProvider>;
}

afterEach(() => {
  vi.restoreAllMocks();
  setAdminAccessToken("");
});

describe("admin surface with the default password", () => {
  it("enters without asking while the password is still the default", async () => {
    let calls = 0;
    vi.spyOn(api, "get").mockImplementation(async (path: string) => {
      if (path === "/api/access") return { admin_exposure: "network", requires_password: true, default_password: true, gateway_port: 8080 } as never;
      calls += 1;
      if (calls === 1) throw new ApiError(401, "missing administrative access password");
      return {} as never;
    });
    const { result } = renderHook(() => useAdminSurface(), { wrapper });
    await waitFor(() => expect(result.current.data).toBe("available"));
    expect(getAdminAccessToken()).toBe(DEFAULT_ADMIN_PASSWORD);
  });

  it("asks for the password once it was changed", async () => {
    vi.spyOn(api, "get").mockImplementation(async (path: string) => {
      if (path === "/api/access") return { admin_exposure: "network", requires_password: true, default_password: false, gateway_port: 8080 } as never;
      throw new ApiError(401, "missing administrative access password");
    });
    const { result } = renderHook(() => useAdminSurface(), { wrapper });
    await waitFor(() => expect(result.current.data).toBe("requires-password"));
    expect(getAdminAccessToken()).toBe("");
  });
});
