// @vitest-environment jsdom
/**
 * After a question in the chat, the menu stopped responding: the URL of the
 * clicked link went into the address bar and the screen remained the chat. Only
 * an F5 brought navigation back.
 *
 * The cause was not in the menu. At the end of the stream the chat freezes a
 * snapshot of that run's telemetry, and the effect that stores that snapshot
 * compared by **reference** a map that was born new on every render -
 * `useQueries` returns a new array every time. The effect stored state, the
 * state triggered another render, and React aborted the tree with "Maximum
 * update depth exceeded". Nothing else committed, the route change included.
 *
 * The test walks the user's path: it opens `/chat`, sends a question, and
 * clicks "Painel" in the menu. What it requires is the new screen on screen.
 */
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { MemoryRouter } from "react-router-dom";
import { LocaleProvider } from "../i18n/provider";
import { ProfileProvider, type Profile } from "../lib/profile";
import { ThemeProvider } from "../lib/theme";
import { ToastProvider } from "../lib/toast";
import type { RequestTrace } from "../lib/chatStream";

const nodeData = {
  hostname: "salem",
  short_id: "a1b2",
  status: { state: "idle" },
  config: null,
  build_commit: "abc1234",
};

/**
 * The `x_pom` the node sends in the last chunk. One hop is enough: the loop was
 * born from the per-node runtimes map, and one node already builds the map.
 */
const trace: RequestTrace = {
  model: "qwen",
  input_tokens: 8,
  output_tokens: 2,
  prefill_ms: 10,
  total_bytes_sent: 100,
  total_bytes_received: 200,
  hops: [{ node_id: "abcdef0123456789", start_layer: 0, end_layer: 15, ms: 10, bytes_sent: 100, bytes_received: 200, calls: 1 } as never],
};

vi.mock("../lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../lib/api")>();
  return {
    ...actual,
    // eslint-disable-next-line require-yield
    streamSse: async function* () {
      yield JSON.stringify({ choices: [{ delta: { content: "Olá" } }] });
      yield JSON.stringify({ x_pom: { tokens_per_sec: 3, tokens_generated: 2, duration_ms: 700, trace } });
      yield "[DONE]";
    },
  };
});

// The sidebar's plugin menu invalidates the plugin list; this file mounts no query client.
vi.mock("@tanstack/react-query", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@tanstack/react-query")>()),
  useQueryClient: () => ({ invalidateQueries: () => Promise.resolve() }),
}));

vi.mock("../lib/imageResize", () => ({
  resizeImageForVision: async () => ({
    data: "data:image/jpeg;base64,ZmFrZQ==",
    size: 4,
    width: 1,
    height: 1,
    resized: false,
  }),
  ImageResizeLimitError: class ImageResizeLimitError extends Error {},
}));

/**
 * A render loop hangs the worker forever - the original defect burned 100% of
 * CPU without ever returning -, and a test that does not finish warns nobody.
 * The cut turns the regression into a readable failure.
 */
const loop = vi.hoisted(() => ({ renders: 0, limit: 400, tripped: false }));

const chatScenario = vi.hoisted(() => ({
  cluster: null as unknown,
  capabilities: null as unknown,
  models: [{ id: "qwen" }] as { id: string }[],
  /** Models on this machine's disk, for the "what next" steps. */
  localModels: [] as { name: string; path: string; shard_range: null }[],
}));

/**
 * `usePeerRuntimes` returns a new array on every call on purpose: it is what
 * `useQueries` really does, and it is the instability the defect exploited. A
 * double that always returned the same array would hide the defect instead of
 * proving it.
 */
vi.mock("../lib/queries", () => ({
  useAdminSurface: () => ({ data: "available", isLoading: false, isError: false }),
  useAdminAccess: () => ({ data: { default_password: false } }),
  usePluginUi: () => ({ data: { plugins: [] }, isLoading: false, isError: false }),
  useMyQuota: () => ({ data: { available: false, configured: false } }),
  usePomCoreUpdateStatus: () => ({ data: { core_update: null } }),
  useNode: () => ({ data: nodeData, isLoading: false, isError: false }),
  useCluster: () => ({ data: chatScenario.cluster }),
  useNetworkChat: () => ({ data: { messages: [] }, isSuccess: true }),
  useChatCapabilities: () => ({ data: chatScenario.capabilities }),
  useInvalidate: () => vi.fn(),
  useServableModels: () => ({ data: chatScenario.models }),
  useLocalCatalog: () => ({ data: { models: chatScenario.localModels, shard_sets: [] } }),
  useInferenceHeads: () => ({ data: undefined, isLoading: false, isError: false }),
  useChatTelemetryNetwork: () => ({ data: undefined, isLoading: false, isError: false }),
  usePeerRuntimes: (nodeIds: string[]) => {
    if (++loop.renders > loop.limit) {
      loop.tripped = true;
      throw new Error(`laço de render: ${loop.limit} renders do chat sem estabilizar`);
    }
    return nodeIds.map(() => ({ data: undefined, isLoading: false }));
  },
  useJobs: () => ({ data: [] }),
  useOffers: () => ({ data: [] }),
  useTransfers: () => ({ data: [] }),
}));

vi.mock("../components/Logo", () => ({
  LogoFull: () => <div aria-hidden="true" />,
  LogoMark: () => <div aria-hidden="true" />,
}));
// The first run wizard asks the node which step is due; this shell has none.
vi.mock("../components/FirstRunWizard", () => ({
  FirstRunWizard: () => null,
  useFirstRunWizard: () => ({ loading: false, step: null }),
}));
vi.mock("./Dashboard", () => ({ Dashboard: () => <div>tela-painel</div> }));

import App from "../App";

function mount(profile: Profile = "admin") {
  return render(
    <LocaleProvider initialLocale="pt-BR">
      <ProfileProvider initialProfile={profile}>
        <ThemeProvider>
          <ToastProvider>
            <MemoryRouter
              initialEntries={["/chat"]}
              future={{ v7_startTransition: true, v7_relativeSplatPath: true }}
            >
              <App />
            </MemoryRouter>
          </ToastProvider>
        </ThemeProvider>
      </ProfileProvider>
    </LocaleProvider>,
  );
}

afterEach(() => {
  cleanup();
  window.localStorage.clear();
  loop.renders = 0;
  loop.tripped = false;
  chatScenario.cluster = null;
  chatScenario.capabilities = null;
  chatScenario.models = [{ id: "qwen" }];
  chatScenario.localModels = [];
});

describe("navigation after the chat", () => {
  it("opens an empty conversation without a title bar", async () => {
    mount();
    await screen.findByTestId("chat-empty");
    expect(screen.queryByRole("heading", { name: "Chat", level: 1 })).toBeNull();
    expect(screen.queryByText("cobertura indisponível")).toBeNull();
  });

  it("shows the missing puzzle piece, and no composer or RAG, while the network serves no model", async () => {
    chatScenario.models = [];
    chatScenario.capabilities = { vision: "unavailable", coverage: "unavailable", rag: "supported", rag_collections: ["docs"] };
    mount("admin");

    const missing = await screen.findByTestId("chat-missing-piece");
    expect(missing.textContent).toContain("Falta uma peça: a rede não está servindo nenhum modelo");
    expect(missing.querySelector('img[alt="POM"]')).toBeTruthy();
    expect(screen.getAllByTestId("puzzle-piece")).toHaveLength(3);
    expect(screen.getByTestId("missing-piece-slot")).toBeTruthy();
    // No model on this machine: the next step is downloading one.
    const steps = screen.getByTestId("no-model-steps");
    expect(steps.querySelector('[data-step="download"]')?.getAttribute("data-state")).toBe("current");
    expect(steps.querySelector('[data-step="download"] a')?.getAttribute("href")).toContain("/models/remote");
    expect(screen.getByRole("link", { name: "Como o POM funciona" })).toBeTruthy();
    // No conversation yet: no way to a history that does not exist.
    expect(screen.queryByRole("button", { name: "Ver conversas anteriores" })).toBeNull();
    expect(screen.queryByRole("textbox")).toBeNull();
    expect(screen.queryByRole("button", { name: /RAG/i })).toBeNull();
  });

  it("points an administrator with a model on disk to deploying it, and offers the earlier conversations", async () => {
    chatScenario.models = [];
    chatScenario.localModels = [{ name: "qwen.gguf", path: "/models/qwen.gguf", shard_range: null }];
    window.localStorage.setItem("pom.chat.sessions", JSON.stringify([
      // The open one is new and empty; an older one has turns.
      { id: "c0", title: "Nova", model: "qwen", messages: [], updatedAt: "2026-09-07T10:00:00.000Z" },
      { id: "c1", title: "Conversa antiga", model: "qwen", messages: [{ id: "m1", role: "user", content: "oi de antes", at: "2026-09-06T10:00:00.000Z" }], updatedAt: "2026-09-06T10:00:00.000Z" },
    ]));
    mount("admin");

    const steps = await screen.findByTestId("no-model-steps");
    expect(steps.querySelector('[data-step="download"]')?.getAttribute("data-state")).toBe("done");
    expect(steps.querySelector('[data-step="deploy"]')?.getAttribute("data-state")).toBe("current");
    expect(steps.querySelector('[data-step="deploy"] a')?.getAttribute("href")).toContain("/deploy");

    fireEvent.click(screen.getByRole("button", { name: "Ver conversas anteriores" }));
    expect(await screen.findByText("oi de antes")).toBeTruthy();
  });

  it("asks a machine that cannot deploy to call an administrator", async () => {
    chatScenario.models = [];
    mount("visitor");
    const missing = await screen.findByTestId("chat-missing-piece");
    expect(missing.textContent).toContain("Peça a um administrador");
    expect(screen.queryByTestId("no-model-steps")).toBeNull();
  });

  it("switches screen when the menu is clicked after a response", async () => {
    mount();

    const composer = await screen.findByRole("textbox");
    act(() => {
      fireEvent.change(composer, { target: { value: "oi" } });
    });
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: /enviar/i }));
    });

    // The response arrived: the turn ended and the telemetry was frozen.
    await screen.findByText("Olá");

    act(() => {
      fireEvent.click(screen.getByRole("link", { name: "Painel" }));
    });

    await waitFor(() => expect(screen.getByText("tela-painel")).toBeTruthy());
    expect(loop.tripped).toBe(false);
  });

  it("shows no conversation list or New chat while there is no conversation", async () => {
    mount();
    await screen.findByTestId("chat-empty");
    expect(screen.queryByTestId("chat-session-list")).toBeNull();
    expect(screen.queryByRole("button", { name: "Conversa nova" })).toBeNull();
  });

  it("does not duplicate the history when infer already has it in the main menu", async () => {
    window.localStorage.setItem("pom.chat.sessions", JSON.stringify([
      { id: "older", title: "Conversa antiga", model: "qwen", messages: [{ id: "m1", role: "user", content: "oi", at: "2026-09-06T10:00:00.000Z" }], updatedAt: "2026-09-06T10:00:00.000Z" },
    ]));
    mount("visitor");

    await screen.findByRole("textbox");
    expect(screen.getAllByTestId("chat-session-list")).toHaveLength(1);
    expect(screen.getAllByRole("button", { name: "Conversa nova" })).toHaveLength(1);
    expect(screen.getByRole("button", { name: "Rede" })).toBeTruthy();
  });

  it("shows the drop target, identifies the format and removes the attachment without erasing the draft", async () => {
    mount();

    const composer = await screen.findByRole("textbox");
    const dropTarget = screen.getByRole("group", { name: "Campo de mensagem" });
    act(() => {
      fireEvent.change(composer, { target: { value: "texto antes do anexo" } });
      fireEvent.dragEnter(dropTarget, {
        dataTransfer: { types: ["Files"], files: [] },
      });
    });
    expect(screen.getByText("Solte um ficheiro")).toBeTruthy();

    const file = new File(["{\"ok\":true}"], "dados.json", { type: "application/json" });
    Object.defineProperty(file, "text", { value: async () => "{\"ok\":true}" });
    act(() => {
      fireEvent.drop(dropTarget, {
        dataTransfer: { types: ["Files"], files: [file] },
      });
    });

    expect(await screen.findByText("dados.json")).toBeTruthy();
    expect(screen.getByLabelText("Formato JSON")).toBeTruthy();
    expect(screen.queryByText("Solte um ficheiro")).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "Remover dados.json" }));
    expect(screen.queryByText("dados.json")).toBeNull();
    expect((composer as HTMLTextAreaElement).value).toBe("texto antes do anexo");
  });

  it("accepts a drop of any file type", async () => {
    mount();

    const dropTarget = await screen.findByRole("group", { name: "Campo de mensagem" });
    const file = new File([new Uint8Array([0, 1, 2])], "dados.bin", { type: "application/octet-stream" });
    Object.defineProperty(file, "text", { value: async () => "AAEC" });
    fireEvent.drop(dropTarget, {
      dataTransfer: { types: ["Files"], files: [file] },
    });

    expect(await screen.findByText("dados.bin")).toBeTruthy();
  });

  it.each(["admin", "worker", "visitor"] as const)(
    "refuses a dropped image when the model has no vision in the %s profile",
    async (profile) => {
      mount(profile);

      const dropTarget = await screen.findByRole("group", { name: "Campo de mensagem" });
      const file = new File(["fake"], "foto.png", { type: "image/png" });
      fireEvent.drop(dropTarget, {
        dataTransfer: { types: ["Files"], files: [file] },
      });

      expect(await screen.findByText("este modelo não tem visão")).toBeTruthy();
    },
  );

  it.each(["worker", "visitor"] as const)(
    "allows an image attachment in the %s profile when the aggregated network confirms vision and coverage",
    async (profile) => {
      chatScenario.cluster = {
        active_model: "qwen",
        complete: false,
        model_inventory: [],
        peers: [],
      };
      chatScenario.capabilities = { vision: "supported", coverage: "complete" };

      mount(profile);

      expect(await screen.findByRole("button", { name: "Anexar ficheiro" })).toBeTruthy();
      expect(screen.queryByText("cobertura incompleta")).toBeNull();

      const dropTarget = screen.getByRole("group", { name: "Campo de mensagem" });
      const file = new File(["fake"], "foto.png", { type: "image/png" });
      fireEvent.drop(dropTarget, {
        dataTransfer: { types: ["Files"], files: [file] },
      });

      expect(await screen.findByText("foto.png")).toBeTruthy();
      expect(screen.queryByText("este modelo não tem visão")).toBeNull();
    },
  );

  it("shows unavailable when the aggregated reading is refused, without inventing empty coverage",
    async () => {
      chatScenario.cluster = {
        active_model: "qwen",
        complete: false,
        model_inventory: [],
        peers: [],
      };
      chatScenario.capabilities = { vision: "unavailable", coverage: "unavailable" };
      // The title bar, where coverage shows, belongs to a conversation with turns.
      window.localStorage.setItem("pom.chat.sessions", JSON.stringify([
        { id: "c1", title: "Conversa", model: "qwen", messages: [{ id: "m1", role: "user", content: "oi", at: "2026-09-06T10:00:00.000Z" }], updatedAt: "2026-09-06T10:00:00.000Z" },
      ]));

      mount("visitor");

      expect(await screen.findByText("cobertura indisponível")).toBeTruthy();
      expect(screen.queryByText("cobertura incompleta")).toBeNull();
      expect(screen.getByRole("button", { name: "Anexar ficheiro" })).toBeTruthy();
    },
  );
});
