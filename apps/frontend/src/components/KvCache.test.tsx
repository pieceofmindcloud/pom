// @vitest-environment jsdom
/**
 * The KV cache on the machine card: how much disk its `kv-slots` takes, and
 * clearing that machine and no other.
 */
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { KvCache } from "./KvCache";
import { LocaleProvider } from "../i18n/provider";
import { ToastProvider } from "../lib/toast";
import type { KvCacheDisk, KvCacheRam } from "../lib/types";

const GIB = 1024 * 1024 * 1024;

const disk: KvCacheDisk = {
  enabled: true,
  path: "/data/kv-slots",
  snapshots: 3,
  bytes: 2 * GIB,
};

const ram: KvCacheRam = {
  enabled: true,
  limit_bytes: 32 * GIB,
  user_reserve_bytes: 32 * GIB,
  bytes: GIB,
  entries: 1,
  tokens: 4000,
  parked: 1,
  restored: 0,
  evicted: 0,
};

function mount(props: Partial<Parameters<typeof KvCache>[0]> = {}) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <LocaleProvider initialLocale="pt-BR">
        <ToastProvider>
          <KvCache nodeId="b" hostname="atena" isSelf={false} disk={disk} ram={ram} {...props} />
        </ToastProvider>
      </LocaleProvider>
    </QueryClientProvider>,
  );
}

describe("KvCache", () => {
  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
  });

  it("shows the disk used by the snapshots and the RAM cache", () => {
    mount();
    expect(screen.getByTestId("kv-cache-disk").textContent).toMatch(/2[.,]00? ?GB.*3 snapshots/);
    expect(screen.getByTestId("kv-cache-ram").textContent).toMatch(/1[.,]00? ?GB de 32/);
  });

  it("draws nothing for a node that does not publish the field yet", () => {
    mount({ disk: undefined });
    expect(screen.queryByTestId("kv-cache")).toBeNull();
  });

  it("says the RAM cache does not exist on a small machine", () => {
    mount({ ram: { ...ram, limit_bytes: null, user_reserve_bytes: null, bytes: 0 } });
    expect(screen.queryByTestId("kv-cache-ram")).toBeNull();
    expect(screen.getByText("indisponível nesta máquina")).toBeTruthy();
  });

  it("does not offer to clear what is empty", () => {
    mount({ disk: { ...disk, snapshots: 0, bytes: 0 }, ram: undefined });
    const button = screen.getByRole("button", { name: /limpar cache/i }) as HTMLButtonElement;
    expect(button.disabled).toBe(true);
  });

  it("asks for confirmation and clears only the machine of the card", async () => {
    const fetchMock = vi.fn(async () =>
      new Response(
        JSON.stringify({
          cleared: true,
          node_id: "b",
          disk_files: 7,
          disk_bytes: 2 * GIB,
          ram_entries: 1,
          ram_bytes: GIB,
        }),
        { status: 200, headers: { "content-type": "application/json" } },
      ),
    );
    vi.stubGlobal("fetch", fetchMock);
    mount();

    fireEvent.click(screen.getByRole("button", { name: /limpar cache/i }));
    expect(fetchMock).not.toHaveBeenCalled();
    expect(screen.getByText("Limpar o cache de KV de atena?")).toBeTruthy();

    const confirm = screen.getAllByRole("button", { name: /limpar cache/i }).at(-1)!;
    fireEvent.click(confirm);

    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(1));
    const [url, init] = fetchMock.mock.calls[0] as unknown as [string, RequestInit];
    expect(url).toContain("/api/ui/kv-cache/clear");
    expect(JSON.parse(String(init.body))).toEqual({ node_id: "b" });
  });
});
