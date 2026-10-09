// @vitest-environment jsdom
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { HuggingFaceSettingsPanel } from "./HuggingFaceSettingsPanel";

const saveHuggingFaceToken = vi.fn();

vi.mock("../lib/api", () => ({
  saveHuggingFaceToken: (...args: unknown[]) => saveHuggingFaceToken(...args),
}));

vi.mock("../lib/queries", () => ({
  useHuggingFaceTokenConfig: () => ({
    data: {
      configured: true,
      masked: "••••••••cret",
      source: "pom.json",
    },
    isLoading: false,
    isError: false,
  }),
  useInvalidate: () => vi.fn(),
}));

afterEach(() => {
  cleanup();
  saveHuggingFaceToken.mockReset();
});

function mount() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <LocaleProvider initialLocale="pt-BR">
        <HuggingFaceSettingsPanel />
      </LocaleProvider>
    </QueryClientProvider>,
  );
}

describe("HuggingFaceSettingsPanel", () => {
  it("shows only the mask, accepts a new token and never preloads the secret", async () => {
    saveHuggingFaceToken.mockResolvedValue({
      configured: true,
      masked: "••••••••cret",
      source: "pom.json",
    });
    mount();

    const input = screen.getByLabelText(/token/i) as HTMLInputElement;
    expect(input.type).toBe("password");
    expect(input.value).toBe("");
    expect(input.placeholder).toContain("••••");
    expect(document.body.textContent).not.toContain("hf_super_secret");

    fireEvent.change(input, { target: { value: "hf_super_secret" } });
    fireEvent.click(screen.getByRole("button", { name: /salvar/i }));

    await waitFor(() => expect(saveHuggingFaceToken).toHaveBeenCalledWith("hf_super_secret"));
  });
});
