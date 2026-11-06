// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { ImageDeploy } from "./ImageDeploy";

const file = (path: string, category: string, size = 100) => ({
  id: `folder:${path}`,
  name: path.replace(/^\/m\//, ""),
  path,
  size_bytes: size,
  source: "folder",
  source_label: "Folder",
  format: "safetensors",
  category,
  family: null,
  dtype: "bf16",
  metadata_error: null,
});

const { state, apiPost } = vi.hoisted(() => ({
  state: { deployments: [] as unknown[] },
  apiPost: vi.fn(),
}));

vi.mock("../lib/queries", () => ({
  useClusterCatalog: () => ({
    data: {
      peers: [
        {
          node_id: "a",
          hostname: "gpu-a",
          is_self: true,
          models: [],
          shard_sets: [],
          image_files: [
            file("/m/Comfy-Org/z/z_image.safetensors", "diffusion", 12),
            file("/m/Comfy-Org/z/qwen_3_4b.safetensors", "text_encoder", 8),
            file("/m/Comfy-Org/z/ae.safetensors", "vae", 3),
          ],
        },
        {
          node_id: "b",
          hostname: "gpu-b",
          is_self: false,
          models: [],
          shard_sets: [],
          image_files: [file("/m/Comfy-Org/z/ae.safetensors", "vae", 3)],
        },
        {
          node_id: "c",
          hostname: "no-comfy",
          is_self: false,
          models: [],
          shard_sets: [],
          image_files: [file("/m/Comfy-Org/z/ae.safetensors", "vae", 3)],
        },
      ],
    },
    isLoading: false,
  }),
  useCluster: () => ({
    data: {
      peers: ["a", "b", "c"].map((node_id) => ({
        node_id,
        hostname: node_id === "c" ? "no-comfy" : `gpu-${node_id}`,
        node: {
          system: { gpus: [] },
          plugin_services:
            node_id === "c" ? [] : [{ plugin_code: "comfyui", id: "comfyui-image", kind: "image", name: "ComfyUI", status: "ready" }],
        },
      })),
    },
  }),
  useImageDeployments: () => ({ data: { deployments: state.deployments }, refetch: vi.fn() }),
}));

vi.mock("../lib/api", () => ({ api: { post: apiPost } }));
vi.mock("../lib/toast", () => ({ useToast: () => ({ success: vi.fn(), fromError: vi.fn() }) }));

afterEach(cleanup);
beforeEach(() => {
  apiPost.mockReset();
  apiPost.mockResolvedValue({ deploy_id: "img-1" });
  state.deployments = [];
});

function renderScreen() {
  return render(
    <MemoryRouter future={{ v7_startTransition: true, v7_relativeSplatPath: true }}>
      <LocaleProvider initialLocale="en">
        <ImageDeploy />
      </LocaleProvider>
    </MemoryRouter>,
  );
}

function row(component: string) {
  return within(screen.getByTestId(`image-component-${component}`));
}

describe("ImageDeploy", () => {
  it("only machines that hold the file are offered", () => {
    renderScreen();
    fireEvent.change(row("vae").getByLabelText("File"), { target: { value: "ae.safetensors|3" } });
    const machines = row("vae").getByLabelText("Machine") as HTMLSelectElement;
    const options = Array.from(machines.options).map((option) => option.textContent);
    expect(options.some((text) => text?.includes("gpu-a"))).toBe(true);
    expect(options.some((text) => text?.includes("gpu-b"))).toBe(true);
    expect(options.some((text) => text?.includes("no-comfy"))).toBe(false);
  });

  it("the text encoder and VAE are pre-filled from the same folder", () => {
    renderScreen();
    fireEvent.change(row("diffusion").getByLabelText("File"), { target: { value: "z_image.safetensors|12" } });
    expect((row("text_encoder").getByLabelText("File") as HTMLSelectElement).value).toBe("qwen_3_4b.safetensors|8");
    expect((row("vae").getByLabelText("File") as HTMLSelectElement).value).toBe("ae.safetensors|3");
    expect((screen.getByLabelText("Name") as HTMLInputElement).value).toBe("z_image");
  });

  it("deploy sends the chosen components", async () => {
    renderScreen();
    fireEvent.change(row("diffusion").getByLabelText("File"), { target: { value: "z_image.safetensors|12" } });
    fireEvent.change(row("vae").getByLabelText("Machine"), { target: { value: "b" } });
    fireEvent.click(screen.getByRole("button", { name: "Deploy" }));
    await waitFor(() => expect(apiPost).toHaveBeenCalled());
    expect(apiPost).toHaveBeenCalledWith("/api/ui/image-deploy", {
      name: "z_image",
      components: [
        { component: "diffusion", node_id: "a", path: "/m/Comfy-Org/z/z_image.safetensors" },
        { component: "text_encoder", node_id: "a", path: "/m/Comfy-Org/z/qwen_3_4b.safetensors" },
        { component: "vae", node_id: "b", path: "/m/Comfy-Org/z/ae.safetensors" },
      ],
    });
  });

  it("served image models are listed with their status and can be undeployed", async () => {
    state.deployments = [
      {
        deploy_id: "img-9",
        name: "Z-Image",
        created_at: "2026-11-06T10:00:00Z",
        components: [
          { component: "diffusion", node_id: "a", hostname: "gpu-a", path: "/m/z.safetensors", status: "ready" },
          { component: "vae", node_id: "b", hostname: "gpu-b", path: "/m/ae.safetensors", status: "missing" },
        ],
      },
    ];
    renderScreen();
    const served = within(screen.getByTestId("image-deployment-img-9"));
    expect(served.getByText("Z-Image")).toBeTruthy();
    expect(served.getByText("Ready")).toBeTruthy();
    expect(served.getByText("Not served")).toBeTruthy();
    fireEvent.click(served.getByRole("button", { name: "Undeploy" }));
    fireEvent.click(screen.getByRole("button", { name: "Undeploy Z-Image" }));
    await waitFor(() => expect(apiPost).toHaveBeenCalledWith("/api/ui/image-undeploy", { deploy_id: "img-9" }));
  });
});
