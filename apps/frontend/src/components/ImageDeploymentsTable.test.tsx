// @vitest-environment jsdom
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { ImageDeployment } from "../lib/types";

const post = vi.fn();
vi.mock("../lib/api", () => ({
  api: { get: vi.fn(() => Promise.resolve({})), post: (url: string, body: unknown) => { post(url, body); return Promise.resolve({ version: 1 }); } },
  getConnectionKey: () => "",
  setConnectionKey: () => {},
  ApiError: class extends Error {},
}));

import { ImageDeploymentsTable } from "./ImageDeploymentsTable";
import { LocaleProvider } from "../i18n/provider";
import { ToastProvider } from "../lib/toast";

const replica = (node_id: string) => ({ node_id, hostname: node_id, status: "ready", in_flight: 0 });
const stage = (component: "text_encoder" | "diffusion" | "vae", nodes: string[]) => ({
  component, file: { name: component, size_bytes: 1 }, replicas: nodes.map((n) => ({ node_id: n, hostname: n, path: `/m/${component}` })),
});
const deployment: ImageDeployment = {
  deploy_id: "img-1", name: "Z-Image", recorded: true, active_version: 2, serving: true, components: [],
  stages: [
    { component: "text_encoder", file: { name: "q", size_bytes: 1 }, replicas: [replica("alpha")] },
    { component: "diffusion", file: { name: "z", size_bytes: 1 }, replicas: [replica("beta"), replica("gamma")] },
    { component: "vae", file: { name: "ae", size_bytes: 1 }, replicas: [replica("alpha")] },
  ],
  versions: [
    { version: 1, created_at: "", created_by: "", stages: [stage("text_encoder", ["alpha"]), stage("diffusion", ["beta"]), stage("vae", ["alpha"])] },
    { version: 2, created_at: "", created_by: "", stages: [stage("text_encoder", ["alpha"]), stage("diffusion", ["beta", "gamma"]), stage("vae", ["alpha"])] },
  ],
};

function wrap() {
  const client = new QueryClient();
  return render(<QueryClientProvider client={client}><LocaleProvider><ToastProvider><ImageDeploymentsTable deployments={[deployment]} /></ToastProvider></LocaleProvider></QueryClientProvider>);
}

afterEach(() => { cleanup(); post.mockReset(); });

describe("ImageDeploymentsTable", () => {
  it("lists image deployments with a badge and their stages", () => {
    wrap();
    expect(screen.getByText("Z-Image")).toBeTruthy();
    expect(screen.getAllByText("Image model").length).toBeGreaterThan(0);
    expect(screen.getByText(/Diffusion model: beta, gamma/)).toBeTruthy();
  });

  it("lists each version's stages and replicas, newest first", () => {
    wrap();
    fireEvent.click(screen.getByRole("button", { name: "Versions" }));
    const versions = screen.getByTestId("image-versions-img-1");
    expect(versions.textContent).toMatch(/v2.*Active.*v1/);
    expect(versions.textContent).toMatch(/beta\+gamma/);
  });

  it("redeploys an older version through the image route", async () => {
    wrap();
    fireEvent.click(screen.getByRole("button", { name: "Versions" }));
    fireEvent.click(screen.getByRole("button", { name: "Redeploy this version" }));
    await waitFor(() => expect(post).toHaveBeenCalledWith("/api/ui/image-deploy/img-1/redeploy", { version: 1 }));
  });
});
