// @vitest-environment jsdom
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, renderHook, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { ReactNode } from "react";
import { api, ApiError, getAdminAccessToken, setAdminAccessToken } from "./api";
import { DEFAULT_ADMIN_PASSWORD, keys, useAdminSurface } from "./queries";

function wrapper({ children }: { children: ReactNode }) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return <QueryClientProvider client={client}>{children}</QueryClientProvider>;
}

afterEach(() => {
  cleanup();
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

  it("probes once when a live session is lost, instead of restarting its own probe forever", async () => {
    let probes = 0;
    vi.stubGlobal("fetch", vi.fn(async (path: string) => {
      if (path === "/api/access") {
        return new Response(JSON.stringify({ admin_exposure: "network", requires_password: true, default_password: false, gateway_port: 8081 }), { status: 200 });
      }
      if (path === "/api/ui/node") probes += 1;
      return new Response(JSON.stringify({ error: "missing administrative access password" }), {
        status: 401,
        headers: { "x-pom-admin-error": "missing-token" },
      });
    }));
    // The tab was inside: the probe had already said "available".
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    client.setQueryData(keys.adminSurface, "available");
    const { result } = renderHook(() => useAdminSurface(), {
      wrapper: ({ children }: { children: ReactNode }) => <QueryClientProvider client={client}>{children}</QueryClientProvider>,
    });

    // Another node on the same host swapped the cookie: the next read returns 401.
    await expect(api.get("/api/ui/cluster")).rejects.toThrow();

    await waitFor(() => expect(result.current.data).toBe("requires-password"));
    await new Promise((resolve) => setTimeout(resolve, 50));
    // The probe's own 401 also signals "session lost". Invalidating the probe
    // in flight cancelled and restarted it: thousands of probes and the screen
    // stuck on the panel.
    expect(probes).toBe(1);
    vi.unstubAllGlobals();
  });
});
