// @vitest-environment jsdom
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { ApiError, api } from "./api";
import { useChatCapabilities } from "./queries";

function Probe() {
  const query = useChatCapabilities();
  return <output>{query.data ? JSON.stringify(query.data) : "pending"}</output>;
}

describe("useChatCapabilities", () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("turns an authorization refusal into unavailable instead of empty", async () => {
    vi.spyOn(api, "getConditional").mockRejectedValue(new ApiError(403, "forbidden"));
    const client = new QueryClient({
      defaultOptions: { queries: { retry: false, refetchInterval: false } },
    });

    render(
      <QueryClientProvider client={client}>
        <Probe />
      </QueryClientProvider>,
    );

    await waitFor(() => {
      expect(screen.getByRole("status").textContent).toBe(
        JSON.stringify({ active_model: "", vision: "unavailable", coverage: "unavailable", thinking: "unavailable", rag: "unavailable" }),
      );
    });
  });
});
