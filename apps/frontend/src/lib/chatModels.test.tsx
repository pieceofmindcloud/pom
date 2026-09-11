// @vitest-environment jsdom
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { Chat } from "../routes/Chat";

const baseModel = "Qwen3.8-27B-Q3_K_S-0fc04107";
const deploymentModel = `${baseModel}@d-80442220`;

vi.mock("./queries", () => ({
  useCluster: () => ({ data: undefined }),
  useChatCapabilities: () => ({ data: { vision: "unavailable", coverage: "unavailable" } }),
  useNode: () => ({ data: undefined }),
  useServableModels: () => ({ data: [{ id: deploymentModel }, { id: baseModel }] }),
  useInferenceHeads: () => ({ data: undefined, isLoading: false, isError: false }),
  useChatTelemetryNetwork: () => ({ data: undefined, isLoading: false, isError: false }),
  usePeerRuntimes: () => [],
}));

vi.mock("./chatStore", async () => {
  const actual = await vi.importActual<typeof import("./chatStore")>("./chatStore");
  return {
    ...actual,
    useChatSessions: () => ({ sessions: [], upsert: vi.fn(), remove: vi.fn() }),
  };
});

vi.mock("./toast", () => ({
  useToast: () => ({ error: vi.fn(), success: vi.fn() }),
}));

describe("Chat model picker", () => {
  afterEach(cleanup);

  it("keeps the base model visible and hides deployment-specific variants", () => {
    render(
      <LocaleProvider initialLocale="pt-BR">
        <Chat />
      </LocaleProvider>,
    );

    expect(screen.getByRole("option", { name: baseModel })).toBeTruthy();
    expect(screen.queryByRole("option", { name: deploymentModel })).toBeNull();
  });
});
