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
        cycle_detection_enabled: true,
        cycle_detection_min_window_tokens: 7,
        cycle_detection_max_window_tokens: 9,
        cycle_detection_repetitions: 8,
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
      cycle_detection_enabled: true,
      cycle_detection_min_window_tokens: 7,
      cycle_detection_max_window_tokens: 9,
      cycle_detection_repetitions: 8,
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
      onChange: vi.fn(),
      onPathChange: vi.fn(),
    });

    expect(screen.getByRole("alert").textContent).toContain(
      "Vision unavailable: projector ownership could not be verified. Check the GGUF model identity and projection dimensions.",
    );
    expect(screen.queryByRole("checkbox", { name: "Enable vision" })).toBeNull();
  });
});
