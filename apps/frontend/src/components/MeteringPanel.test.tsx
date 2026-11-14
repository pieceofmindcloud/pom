// @vitest-environment jsdom
/**
 * The principal turns GPU work metering on and off at run time from the
 * Network screen; the whole network document goes back with only `metering`
 * changed, and a machine that does not run the ledger plugin says so.
 */
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { MeteringPanel, METERING_DEFAULTS } from "./MeteringPanel";
import { api } from "../lib/api";
import type { NetworkSettings } from "../lib/types";

vi.mock("../lib/api", async (orig) => ({ ...(await orig<object>()), api: { get: vi.fn(), post: vi.fn(), getConditional: vi.fn() } }));
vi.mock("../lib/toast", () => ({ useToast: () => ({ success: vi.fn(), fromError: vi.fn(), error: vi.fn() }) }));

const network = { public_name: "home", rules: "be kind", metering: { ...METERING_DEFAULTS } } as unknown as NetworkSettings;

function mount(props: Partial<React.ComponentProps<typeof MeteringPanel>>, plugins: unknown[] = []) {
  vi.mocked(api.get).mockImplementation(async (path: string) => (path === "/api/ui/plugins" ? plugins : {}));
  vi.mocked(api.getConditional).mockResolvedValue({ peers: [] } as never);
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <LocaleProvider>
        <MeteringPanel network={network} editable onSaved={vi.fn()} {...props} />
      </LocaleProvider>
    </QueryClientProvider>,
  );
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("MeteringPanel", () => {
  it("turns metering on and sends the network back with only metering changed", async () => {
    const onSaved = vi.fn();
    vi.mocked(api.post).mockResolvedValue({ network: { ...network, metering: { ...METERING_DEFAULTS, enabled: true } }, can_edit: true });
    mount({ onSaved }, [{ code: "metering", active: true }]);
    const save = screen.getByRole("button", { name: /save metering/i }) as HTMLButtonElement;
    expect(save.disabled).toBe(true);
    fireEvent.click(screen.getByTestId("metering-enabled"));
    fireEvent.change(screen.getByRole("combobox", { name: /payout cycle/i }), { target: { value: "3600" } });
    fireEvent.click(save);
    await waitFor(() => expect(api.post).toHaveBeenCalled());
    const [path, body] = vi.mocked(api.post).mock.calls[0] as [string, NetworkSettings & { metering: typeof METERING_DEFAULTS }];
    expect(path).toBe("/api/ui/network/settings");
    expect(body.rules).toBe("be kind");
    expect(body.metering.enabled).toBe(true);
    expect(body.metering.cycle_secs).toBe(3600);
    await waitFor(() => expect(onSaved).toHaveBeenCalled());
  });

  it("warns when this machine runs no ledger plugin", async () => {
    mount({});
    fireEvent.click(screen.getByTestId("metering-enabled"));
    expect(await screen.findByTestId("metering-no-ledger")).toBeTruthy();
  });

  it("is read-only for someone who cannot edit the network", () => {
    mount({ editable: false });
    expect(screen.queryByTestId("metering-enabled")).toBeNull();
    expect(screen.queryByRole("button", { name: /save metering/i })).toBeNull();
  });
});
