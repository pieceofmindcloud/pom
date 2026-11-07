// @vitest-environment jsdom
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { ImageDeployment } from "../lib/types";

const post = vi.fn();
vi.mock("../lib/api", () => ({
  api: {
    get: vi.fn(() => Promise.resolve({})),
    post: (url: string, body: unknown) => {
      post(url, body);
      if (url === "/api/ui/image-deploy/plan") {
        return Promise.resolve({
          name: "Z", memory: [], copies: [], blocked: [],
          stages: [{ component: "diffusion", file: { name: "z", size_bytes: 1 }, chosen: [], alternatives: [
            { node_id: "b", hostname: "beta", ok: true, path: "/m/z" },
            { node_id: "c", hostname: "gamma", ok: true, path: "/m/z" },
            { node_id: "d", hostname: "delta", ok: false, reason: "does not run ComfyUI" },
          ] }],
        });
      }
      if (url === "/api/ui/image-undeploy") return Promise.resolve({ released: [{ node_id: "a", ok: true }, { node_id: "b", ok: true }] });
      return Promise.resolve({ version: 2 });
    },
  },
  getConnectionKey: () => "",
  setConnectionKey: () => {},
  ApiError: class extends Error {},
}));

import { ImageAddReplicaDialog, ImagePipelineMap, ImageUndeploy } from "./ImagePipelineMap";
import { LocaleProvider } from "../i18n/provider";
import { ToastProvider } from "../lib/toast";

const replica = (node_id: string, hostname: string, status = "ready", in_flight: number | null = 0) => ({ node_id, hostname, status, in_flight });
const deployment: ImageDeployment = {
  deploy_id: "img-1", name: "Z-Image", recorded: true, active_version: 1, serving: true, components: [],
  stages: [
    { component: "text_encoder", file: { name: "qwen.safetensors", size_bytes: 1 }, replicas: [replica("a", "alpha")] },
    { component: "diffusion", file: { name: "z.safetensors", size_bytes: 1 }, replicas: [replica("b", "beta", "ready", 2), replica("c", "gamma", "loading")] },
    { component: "vae", file: { name: "ae.safetensors", size_bytes: 1 }, replicas: [replica("a", "alpha")] },
  ],
};

function wrap(node: React.ReactNode) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(<QueryClientProvider client={client}><LocaleProvider><ToastProvider><MemoryRouter>{node}</MemoryRouter></ToastProvider></LocaleProvider></QueryClientProvider>);
}

afterEach(() => { cleanup(); post.mockReset(); });

describe("ImagePipelineMap", () => {
  it("draws three stages in flow order with the chips of every model", () => {
    wrap(<ImagePipelineMap deployment={deployment} models={[{ id: "qwen3", label: "qwen3" }, { id: "image:img-1", label: "Z-Image", image: true }]} onModelChange={() => {}} />);
    const stages = screen.getAllByTestId(/^map-stage-/).map((el) => el.dataset.testid);
    expect(stages).toEqual(["map-stage-text_encoder", "map-stage-diffusion", "map-stage-vae"]);
    expect(screen.getByRole("button", { name: /Z-Image/ }).getAttribute("aria-pressed")).toBe("true");
    expect(screen.getByText("3 of 3 stages ready")).toBeTruthy();
  });

  it("stacks replicas under their stage with status, RTT and calls in flight", () => {
    wrap(<ImagePipelineMap deployment={deployment} models={[]} onModelChange={() => {}} localNodeId="me" rtt={[{ from_node_id: "me", to_node_id: "b", rtt_ms: 3.25, transport: "p2p" }]} />);
    const beta = screen.getByTestId("map-replica-diffusion-b");
    expect(beta.textContent).toMatch(/Ready/);
    expect(beta.textContent).toMatch(/3\.3 ms/);
    expect(beta.textContent).toMatch(/2 in flight/);
    expect(screen.getByTestId("map-replica-diffusion-c").textContent).toMatch(/Loading/);
  });
});

describe("image deployment actions", () => {
  it("lists every replica machine before undeploying", async () => {
    wrap(<ImageUndeploy deployment={deployment} />);
    fireEvent.click(screen.getByRole("button", { name: "Undeploy" }));
    const list = await screen.findByTestId("image-undeploy-machines");
    expect(within(list).getAllByRole("listitem").map((li) => li.textContent)).toEqual(["alpha", "beta", "gamma"]);
    const dialog = screen.getByRole("dialog");
    fireEvent.click(within(dialog).getByRole("button", { name: "Undeploy" }));
    await waitFor(() => expect(post).toHaveBeenCalledWith("/api/ui/image-undeploy", { deploy_id: "img-1" }));
  });

  it("offers only machines that hold the stage file and are not replicas yet", async () => {
    const one = { ...deployment, stages: deployment.stages.map((s) => s.component === "diffusion" ? { ...s, replicas: [replica("b", "beta")] } : s) };
    wrap(<ImageAddReplicaDialog deployment={one} open onClose={() => {}} />);
    const machine = await screen.findByLabelText("Machine");
    await waitFor(() => expect(machine.querySelectorAll("option").length).toBe(2));
    expect([...machine.querySelectorAll("option")].map((o) => o.textContent)).toEqual(["Choose a machine", "gamma"]);
    fireEvent.change(machine, { target: { value: "c" } });
    fireEvent.click(screen.getByRole("button", { name: "Add" }));
    await waitFor(() => expect(post).toHaveBeenCalledWith("/api/ui/image-deploy/img-1/add", { component: "diffusion", node_id: "c" }));
  });
});
