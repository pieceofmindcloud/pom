// @vitest-environment jsdom
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { ImagePlan } from "../lib/types";

const post = vi.fn();
let plan: ImagePlan = { name: "z", stages: [], memory: [], copies: [], blocked: [] };
const peers = [
  { node_id: "a", short_id: "a", node: { hostname: "alpha" } },
  { node_id: "b", short_id: "b", node: { hostname: "beta" } },
];
const catalog = {
  peers: [
    {
      node_id: "a",
      image_files: [
        { name: "org/z/qwen.safetensors", path: "/m/qwen.safetensors", size_bytes: 8, category: "text_encoder", metadata_error: null },
        { name: "org/z/ae.safetensors", path: "/m/ae.safetensors", size_bytes: 3, category: "vae", metadata_error: null },
        { name: "org/z/z_image.safetensors", path: "/m/z_image.safetensors", size_bytes: 12, category: "diffusion", metadata_error: null },
      ],
    },
  ],
};
vi.mock("../lib/api", () => ({
  api: {
    get: vi.fn((url: string) =>
      url === "/api/ui/jobs"
        ? Promise.resolve({ jobs: [] })
        : url === "/api/ui/cluster"
          ? Promise.resolve({ peers })
          : url === "/api/ui/cluster/models"
            ? Promise.resolve(catalog)
            : Promise.resolve({}),
    ),
    post: (url: string, body: unknown) => {
      post(url, body);
      if (url === "/api/ui/image-deploy/plan") return Promise.resolve(plan);
      if (url === "/api/ui/image-deploy") return Promise.resolve({ deploy_id: "img-1", job_id: "deploy-img-1" });
      return Promise.resolve({});
    },
  },
  getConnectionKey: () => "",
  setConnectionKey: () => {},
  ApiError: class extends Error {},
}));

import { ImageDeployStudio, ImageStageCanvas, networkImageFiles, prefill } from "./ImageDeployStudio";
import { LocaleProvider } from "../i18n/provider";
import { ToastProvider } from "../lib/toast";
import { emptyChoices } from "../lib/imagePipeline";

function wrap(node: React.ReactNode) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <LocaleProvider>
        <ToastProvider>
          <MemoryRouter>{node}</MemoryRouter>
        </ToastProvider>
      </LocaleProvider>
    </QueryClientProvider>,
  );
}

function drop(target: Element, nodeId: string) {
  fireEvent.drop(target, { dataTransfer: { getData: (type: string) => (type === "application/x-pom-node" ? nodeId : "") } });
}

afterEach(() => {
  cleanup();
  post.mockReset();
  plan = { name: "z", stages: [], memory: [], copies: [], blocked: [] };
});

describe("ImageStageCanvas", () => {
  it("drops a machine on a stage", () => {
    const onDrop = vi.fn();
    wrap(<ImageStageCanvas plan={undefined} choices={emptyChoices()} labelOf={(id) => id} onDrop={onDrop} onRemove={() => {}} />);
    drop(screen.getByTestId("stage-diffusion"), "b");
    expect(onDrop).toHaveBeenCalledWith("diffusion", "b");
  });

  it("shows the same machine on two stages and replicas on one", () => {
    const choices = emptyChoices();
    choices.text_encoder.nodeIds = ["a"];
    choices.vae.nodeIds = ["a"];
    choices.diffusion.nodeIds = ["a", "b"];
    wrap(<ImageStageCanvas plan={undefined} choices={choices} labelOf={(id) => id} onDrop={() => {}} onRemove={() => {}} />);
    expect(screen.getByTestId("replica-text_encoder-a")).toBeTruthy();
    expect(screen.getByTestId("replica-vae-a")).toBeTruthy();
    expect(screen.getByTestId("replica-diffusion-b")).toBeTruthy();
    expect(screen.getByText(/2 replicas/)).toBeTruthy();
  });
});

describe("image files", () => {
  it("prefills the text encoder and VAE from the diffusion file's folder", () => {
    const files = networkImageFiles(catalog.peers);
    const diffusion = files.find((f) => f.category === "diffusion")!;
    const choices = prefill(files, diffusion, emptyChoices());
    expect(choices.diffusion.file?.name).toBe("z_image.safetensors");
    expect(choices.text_encoder.file?.name).toBe("qwen.safetensors");
    expect(choices.vae.file?.name).toBe("ae.safetensors");
  });
});

describe("ImageDeployStudio", () => {
  it("shows the verdict and keeps Deploy off", async () => {
    plan = { name: "z", stages: [], memory: [], copies: [], blocked: ["diffusion has no machine"] };
    wrap(<ImageDeployStudio modeBar={null} />);
    expect((await screen.findByTestId("image-verdict")).textContent).toMatch(/diffusion model/i);
    const diffusionSelect = await screen.findByLabelText("Diffusion model");
    await waitFor(() => expect(diffusionSelect.querySelectorAll("option").length).toBeGreaterThan(1));
    fireEvent.change(diffusionSelect, { target: { value: "z_image.safetensors|12" } });
    await waitFor(() => expect(screen.getByTestId("image-verdict").textContent).toBe("diffusion has no machine"));
    expect((screen.getByRole("button", { name: "Deploy" }) as HTMLButtonElement).disabled).toBe(true);
  });

  it("deploys the stages it planned", async () => {
    wrap(<ImageDeployStudio modeBar={null} />);
    const diffusionSelect = await screen.findByLabelText("Diffusion model");
    await waitFor(() => expect(diffusionSelect.querySelectorAll("option").length).toBeGreaterThan(1));
    fireEvent.change(diffusionSelect, { target: { value: "z_image.safetensors|12" } });
    const ok = (node_id: string, path: string) => ({ node_id, hostname: node_id, ok: true, path });
    plan = {
      name: "z_image",
      stages: [
        { component: "text_encoder", file: { name: "qwen.safetensors", size_bytes: 8 }, chosen: [ok("a", "/m/qwen.safetensors")], alternatives: [] },
        { component: "diffusion", file: { name: "z_image.safetensors", size_bytes: 12 }, chosen: [ok("a", "/m/z_image.safetensors")], alternatives: [] },
        { component: "vae", file: { name: "ae.safetensors", size_bytes: 3 }, chosen: [ok("a", "/m/ae.safetensors")], alternatives: [] },
      ],
      memory: [],
      copies: [],
      blocked: [],
    };
    for (const stage of ["text_encoder", "diffusion", "vae"]) drop(screen.getByTestId(`stage-${stage}`), "a");
    const deploy = screen.getByRole("button", { name: "Deploy" }) as HTMLButtonElement;
    await waitFor(() => expect(deploy.disabled).toBe(false));
    fireEvent.click(deploy);
    await waitFor(() => expect(post).toHaveBeenCalledWith("/api/ui/image-deploy", expect.anything()));
    const body = post.mock.calls.find(([url]) => url === "/api/ui/image-deploy")![1] as { stages: Array<{ component: string; node_ids: string[] }> };
    expect(body.stages.map((s) => s.component)).toEqual(["text_encoder", "diffusion", "vae"]);
    expect(body.stages.every((s) => s.node_ids.includes("a"))).toBe(true);
  });
});
