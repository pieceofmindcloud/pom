// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import type { RuntimeParams } from "../lib/types";
import { DeploymentInspector, type VisionControl } from "./DeploymentInspector";

afterEach(cleanup);

const params: RuntimeParams = {
  inference_mode: "auto",
  n_gpu_layers: 99,
  n_ctx: 8192,
  offload_kqv: true,
  fit_memory: true,
  fit_memory_margin_mb: 256,
  n_batch: 512,
  max_new_tokens: 512,
  max_sessions: 1,
  prefill_chunk_tokens: 512,
  prefill_max_inflight_chunks: 2,
  temperature: 0.7,
  top_p: 0.95,
  top_k: 40,
  repeat_penalty: 1.1,
  repeat_last_n: 64,
  no_thinking: true,
  no_embeddings: false,
  speculative_decoding_enabled: false,
  speculative_k: 4,
  speculative_lookup_window: 3,
  mtp_enabled: false,
  mtp_draft_tokens: 3,
  mtp_min_probability: 0.1,
  unload_if_idle: false,
  idle_minutes: 60,
};

function mount(
  value = params,
  onChange = vi.fn(),
  vision?: VisionControl,
  pomDesenv = true,
) {
    render(
      <LocaleProvider initialLocale="en">
      <DeploymentInspector value={value} onChange={onChange} vision={vision} pomDesenv={pomDesenv} />
    </LocaleProvider>,
  );
  return onChange;
}

describe("DeploymentInspector idle policy", () => {
  it("shows the default timeout in the POM development section", () => {
    mount();

    expect(screen.getByRole("checkbox", { name: "Unload if idle" })).toBeTruthy();
    expect(screen.getByRole("spinbutton", { name: "Idle time (minutes)" })).toHaveProperty(
      "value",
      "60",
    );
    expect(screen.getByRole("spinbutton", { name: "Idle time (minutes)" }).getAttribute("min"))
      .toBe("1");
  });

  it("removes the whole POM development section without losing its values", () => {
    const onChange = vi.fn();
    mount(
      {
        ...params,
        inference_mode: "moe",
        mtp_cpu_fallback_enabled: false,
        unload_if_idle: true,
        idle_minutes: 2,
      },
      onChange,
      undefined,
      false,
    );

    expect(screen.queryByText("POM settings (DEV)")).toBeNull();
    expect(screen.queryByRole("checkbox", { name: "Unload if idle" })).toBeNull();
    expect(screen.queryByRole("spinbutton", { name: "Idle time (minutes)" })).toBeNull();
    expect(screen.queryByRole("combobox", { name: "Inference engine" })).toBeNull();
    fireEvent.click(screen.getByRole("checkbox", { name: "Disable embeddings" }));
    expect(onChange).toHaveBeenCalledWith(expect.objectContaining({
      inference_mode: "moe",
      mtp_cpu_fallback_enabled: false,
      unload_if_idle: true,
      idle_minutes: 2,
      no_embeddings: true,
    }));
  });

  it("persists the toggle and the per-deployment timeout", () => {
    const onChange = mount();
    fireEvent.click(screen.getByRole("checkbox", { name: "Unload if idle" }));
    fireEvent.change(screen.getByRole("spinbutton", { name: "Idle time (minutes)" }), {
      target: { value: "15" },
    });

    expect(onChange).toHaveBeenCalledWith(expect.objectContaining({
      inference_mode: "auto",
      unload_if_idle: true,
    }));
    expect(onChange).toHaveBeenLastCalledWith(expect.objectContaining({ idle_minutes: 15 }));
  });

  it("shows and toggles vision for a sliced deployment", () => {
    const onVisionChange = vi.fn();
    mount(params, vi.fn(), {
      available: true,
      enabled: false,
      path: "/models/mmproj-vision.gguf",
      candidates: ["/models/mmproj-vision.gguf"],
      onChange: onVisionChange,
      onPathChange: vi.fn(),
    });

    const vision = screen.getByRole("checkbox", { name: "Enable vision" });
    expect(vision).toBeTruthy();
    expect(screen.getByText("/models/mmproj-vision.gguf")).toBeTruthy();
    fireEvent.click(vision);
    expect(onVisionChange).toHaveBeenCalledWith(true);
  });

  it("explains when a sliced deployment has no projector", () => {
    mount(params, vi.fn(), {
      available: false,
      enabled: false,
      path: null,
      candidates: [],
      reason: null,
      onChange: vi.fn(),
      onPathChange: vi.fn(),
    });

    expect(screen.getByRole("alert").textContent).toContain(
      "Vision unavailable: projector ownership could not be verified. Check the GGUF model identity and projection dimensions.",
    );
    expect(screen.queryByRole("checkbox", { name: "Enable vision" })).toBeNull();
  });
});

describe("DeploymentInspector default settings", () => {
  it("shows the full sampler, the seed and where MTP runs without POM_DESENV", () => {
    mount({ ...params, mtp_enabled: true, dry_multiplier: 0.8 }, vi.fn(), undefined, false);

    const labels = [
      ...["Temperature", "Top-p", "Top-k", "Min-p", "Penalty", "Frequency", "DRY base"]
        .map((name) => screen.getByRole("spinbutton", { name })),
      screen.getByRole("textbox", { name: "Seed" }),
    ];
    // Sampling keeps llama.cpp's order: candidate cuts, repetition penalties, DRY, then the seed.
    for (let index = 1; index < labels.length; index += 1) {
      expect(labels[index - 1].compareDocumentPosition(labels[index]) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    }
    const mtpProbability = screen.getByRole("spinbutton", { name: "MTP minimum probability" });
    const placement = screen.getByRole("combobox", { name: "Where MTP runs" });
    expect(mtpProbability.compareDocumentPosition(placement) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(screen.queryByRole("checkbox", { name: "Unload if idle" })).toBeNull();
  });
});

describe("DeploymentInspector DRY", () => {
  it("shows the DRY base, allowed length and window only while the multiplier turns DRY on", () => {
    mount({ ...params, dry_multiplier: 0 });
    expect(screen.getByRole("spinbutton", { name: "DRY multiplier" })).toBeTruthy();
    expect(screen.queryByRole("spinbutton", { name: "DRY base" })).toBeNull();
    cleanup();
    mount({ ...params, dry_multiplier: 0.8 });
    expect(screen.getByRole("spinbutton", { name: "DRY base" })).toBeTruthy();
  });
});

describe("DeploymentInspector MoE experts in RAM", () => {
  it("offers the experts in RAM and the GPU expert cache only under POM_DESENV", () => {
    const onChange = mount({ ...params }, vi.fn(), undefined, true);
    fireEvent.click(screen.getByRole("checkbox", { name: "MoE experts in RAM" }));
    expect(onChange).toHaveBeenCalledWith(expect.objectContaining({ moe_experts_in_ram: true }));
    fireEvent.change(screen.getByRole("spinbutton", { name: "Expert cache on GPU (MiB)" }), { target: { value: "8000" } });
    expect(onChange).toHaveBeenLastCalledWith(expect.objectContaining({ moe_cache_mib: 8000 }));
    cleanup();

    mount({ ...params }, vi.fn(), undefined, false);
    expect(screen.queryByRole("checkbox", { name: "MoE experts in RAM" })).toBeNull();
    expect(screen.queryByRole("spinbutton", { name: "Expert cache on GPU (MiB)" })).toBeNull();
  });
});
