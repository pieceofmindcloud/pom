// @vitest-environment jsdom
/**
 * The Payments screen warns before charging cuts off the visitors on an
 * older release: the first Save shows them, the second turns charging on.
 */
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { Payments } from "./Payments";
import { ChatError } from "./Chat";
import { api } from "../lib/api";

vi.mock("../lib/api", async (orig) => ({ ...(await orig<object>()), api: { get: vi.fn(), post: vi.fn() } }));
vi.mock("../lib/toast", () => ({ useToast: () => ({ success: vi.fn(), fromError: vi.fn(), error: vi.fn() }) }));

const overview = {
  cut_off: [{ node_id: "old-visitor-node", name: "old-laptop", version: "1.0.0" }],
  enabled: false,
  inference_access: "direct",
  prices: {},
  principal: true,
  accounts: [],
  requests: [],
  cycles: { list: [{ cycle: 7, income_micro: 1_500_000, accounts: { "net:a": 1_500_000 }, payouts: { closed: true, payout: 2.5, earnings: 3, nodes: 2 } }], unmetered_income_micro: 0 },
  ledger: [],
};

function mount(ui: React.ReactNode) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <LocaleProvider>
        <MemoryRouter>{ui}</MemoryRouter>
      </LocaleProvider>
    </QueryClientProvider>,
  );
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("Payments", () => {
  it("shows who would be cut off before turning charging on", async () => {
    vi.mocked(api.get).mockImplementation(async (path: string) =>
      path === "/api/ui/payments" ? overview : { network: { public_name: "n" }, can_edit: true },
    );
    vi.mocked(api.post).mockResolvedValue({});
    mount(<Payments />);
    const select = await screen.findByRole("combobox");
    fireEvent.change(select, { target: { value: "on" } });
    expect((await screen.findByTestId("payments-cut-off")).textContent).toContain("old-laptop");
    fireEvent.click(screen.getByRole("button", { name: /save/i }));
    expect(api.post).not.toHaveBeenCalled();
    fireEvent.click(await screen.findByRole("button", { name: /turn on anyway/i }));
    await waitFor(() => expect(api.post).toHaveBeenCalledWith("/api/ui/network/settings", expect.objectContaining({
      inference_access: "member_gateway_only",
      payments: { enabled: true, prices: {} },
    })));
  });

  it("lists income beside the payouts of each cycle", async () => {
    vi.mocked(api.get).mockImplementation(async (path: string) =>
      path === "/api/ui/payments" ? overview : { network: {}, can_edit: true },
    );
    mount(<Payments />);
    expect(await screen.findByText("1.5")).toBeTruthy();
    expect(screen.getByText("2.5")).toBeTruthy();
  });
});

describe("ChatError", () => {
  it("points a visitor out of credit to its Credit page", () => {
    mount(<ChatError message="the account lacks 50000 micro-credits" kind="payment_required" />);
    expect(screen.getByRole("link").getAttribute("href")).toBe("/config/credit");
  });
});
