// @vitest-environment jsdom
/**
 * This machine's token budget: the plans that bind it, the session reset it
 * may spend and the request for more budget, all through `/api/ui/quota`.
 */
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { MyBudget } from "./MyBudget";
import { api } from "../lib/api";

vi.mock("../lib/api", () => ({ api: { get: vi.fn(), post: vi.fn() } }));
vi.mock("../lib/toast", () => ({ useToast: () => ({ success: vi.fn(), fromError: vi.fn() }) }));

const later = new Date(Date.now() + 95 * 60_000).toISOString();
const blocked = {
  available: true,
  configured: true,
  blocked: true,
  resettable: true,
  limits: [
    {
      assignment_id: "a", plan: "Pro - 5h session", period: "session", session_minutes: 300,
      scope: "everyone", subject: "", mode: "member", counter: "me", active: true, resets_at: later,
      used_input: 4_100_000, used_output: 251_000, input_limit: 5_000_000, output_limit: 250_000,
      extra_input: 0, extra_output: 0, exhausted: true,
    },
  ],
  reset_credits: { available: 1, used: 0 },
  grants: [],
  pending_request: null,
};

function mount() {
  return render(
    <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
      <LocaleProvider initialLocale="en">
        <MyBudget />
      </LocaleProvider>
    </QueryClientProvider>,
  );
}

describe("MyBudget", () => {
  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it("shows a spent session with its bars and spends a reset credit", async () => {
    vi.mocked(api.get).mockResolvedValue(blocked);
    vi.mocked(api.post).mockResolvedValue({ ...blocked, blocked: false, reset_credits: { available: 0, used: 1 } });
    mount();
    expect(await screen.findByText("Pro - 5h session")).toBeTruthy();
    expect(screen.getByText("Blocked")).toBeTruthy();
    expect(screen.getAllByRole("progressbar")).toHaveLength(2);
    expect(screen.getByText(/1 reset credit/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /Reset session/ }));
    await waitFor(() => expect(api.post).toHaveBeenCalledWith("/api/ui/quota/reset"));
  });

  it("sends a request for more budget with its note", async () => {
    vi.mocked(api.get).mockResolvedValue({ ...blocked, reset_credits: { available: 0, used: 0 } });
    vi.mocked(api.post).mockResolvedValue(blocked);
    mount();
    const reset = await screen.findByRole("button", { name: /Reset session/ });
    expect((reset as HTMLButtonElement).disabled).toBe(true);
    fireEvent.change(screen.getByPlaceholderText(/Why you need it/), { target: { value: "release day" } });
    fireEvent.click(screen.getByRole("button", { name: /Send request/ }));
    await waitFor(() => expect(api.post).toHaveBeenCalledWith("/api/ui/quota/request", { note: "release day" }));
  });

  it("waits for a machine that runs the plugin instead of saying there is no budget", async () => {
    vi.mocked(api.get).mockResolvedValue({ available: true, configured: false, pending: true });
    mount();
    await waitFor(() => expect(api.get).toHaveBeenCalledWith("/api/ui/quota"));
    expect(screen.queryByText("No budget limits this machine")).toBeNull();
  });

  it("says when no admin holds budgets", async () => {
    vi.mocked(api.get).mockResolvedValue({ available: false, configured: false });
    mount();
    expect(await screen.findByText("No admin answered")).toBeTruthy();
  });
});
