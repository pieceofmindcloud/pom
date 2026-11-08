// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import type { EngineSetting, PomSetting, RuntimeParams } from "../lib/types";
import { DeploymentInspector } from "./DeploymentInspector";

afterEach(cleanup);

const params = {
  n_ctx: 8192,
  max_sessions: 1,
  prefill_max_inflight_chunks: 8,
  hidden_state_dtype: "f16",
  unload_if_idle: false,
  idle_minutes: 60,
} as RuntimeParams;

function mount(
  showHiddenStateDtype: boolean,
  onChange = vi.fn(),
  pomSettings: PomSetting[] = ["prefill_max_inflight_chunks", "hidden_state_dtype"],
  settings: EngineSetting[] = [],
) {
  render(
    <LocaleProvider initialLocale="en">
      <DeploymentInspector
        value={params}
        onChange={onChange}
        showHiddenStateDtype={showHiddenStateDtype}
        inferenceService={{ name: "Strata", settings, pomSettings }}
      />
    </LocaleProvider>,
  );
  return onChange;
}

describe("DeploymentInspector with a plugin's inference service", () => {
  const draftSidecars = {
    options: [
      {
        kind: "mtp" as const,
        file_name: "dspark-DeepSeek-V4.1-Flash-1017-Q8_0.gguf",
        size_bytes: 7976914240,
        general_name: null,
        quant: "Q8_0",
        holders: [{ node_id: "n1", hostname: "salem", path: "/m/dspark.gguf" }],
      },
    ],
    vision: { value: { mode: "link" as const }, onChange: vi.fn(), slot: null, linkLabel: null },
    mtp: { value: { mode: "link" as const }, onChange: vi.fn(), slot: null, linkLabel: null, allowEmbedded: false },
  };

  it("offers the draft files when the service takes one for this model", () => {
    render(
      <LocaleProvider initialLocale="en">
        <DeploymentInspector
          value={params}
          onChange={vi.fn()}
          sidecars={draftSidecars}
          inferenceService={{ name: "Strata", settings: [], pomSettings: [], takesDraft: true }}
        />
      </LocaleProvider>,
    );
    expect(document.body.textContent).toContain("dspark-DeepSeek-V4.1-Flash-1017-Q8_0.gguf");
  });

  it("hides the draft files when the service would ignore the choice", () => {
    render(
      <LocaleProvider initialLocale="en">
        <DeploymentInspector
          value={params}
          onChange={vi.fn()}
          sidecars={draftSidecars}
          inferenceService={{ name: "Strata", settings: [], pomSettings: [] }}
        />
      </LocaleProvider>,
    );
    expect(document.body.textContent).not.toContain("dspark-DeepSeek-V4.1-Flash-1017-Q8_0.gguf");
  });

  it("offers the POM's transfer between the slices when they cross machines", () => {
    const onChange = mount(true);
    const precision = screen.getByRole("combobox", { name: "Hidden-state precision" });
    fireEvent.change(precision, { target: { value: "f32" } });
    expect(onChange).toHaveBeenCalledWith(expect.objectContaining({ hidden_state_dtype: "f32" }));
    const chunks = document.querySelector('input[name="prefill_max_inflight_chunks"]');
    expect(chunks).not.toBeNull();
    // They are the POM's, in its own card, not among the engine's settings.
    const card = precision.closest("section");
    expect(card?.textContent).toContain("POM settings");
    expect(card?.textContent).not.toContain("Strata settings");
    expect(chunks?.closest("section")).toBe(card);
  });

  it("shows only the POM settings the service declares", () => {
    mount(true, vi.fn(), ["hidden_state_dtype"]);
    expect(screen.getByRole("combobox", { name: "Hidden-state precision" })).toBeTruthy();
    expect(document.querySelector('input[name="prefill_max_inflight_chunks"]')).toBeNull();
  });

  it("draws a setting placed in the POM card in it, in both the slices and the whole model", () => {
    const settings: EngineSetting[] = [
      { key: "session_save", type: "boolean", placement: "pom", label: { en: "Save the KV cache" } },
      { key: "vram_reserve_mib", type: "integer", label: { en: "VRAM reserve" } },
    ];
    for (const slices of [true, false]) {
      cleanup();
      mount(slices, vi.fn(), [], settings);
      const save = screen.getByText("Save the KV cache").closest("section");
      expect(save?.textContent).toContain("POM settings");
      const reserve = screen.getByText("VRAM reserve").closest("section");
      expect(reserve?.textContent).not.toContain("POM settings");
    }
  });

  it("a service that declares no POM setting still gets the POM card, with only Yield the GPU to images, off", () => {
    const onChange = vi.fn();
    mount(true, onChange, []);
    expect(screen.getByText("POM settings")).toBeTruthy();
    expect(screen.getByText("Yield the GPU to images")).toBeTruthy();
    expect(screen.queryByRole("combobox", { name: "Hidden-state precision" })).toBeNull();
  });

  it("leaves them out for a model on one machine", () => {
    mount(false);
    expect(screen.queryByRole("combobox", { name: "Hidden-state precision" })).toBeNull();
    expect(document.querySelector('input[name="prefill_max_inflight_chunks"]')).toBeNull();
  });

  it("offers the vision projector to a plugin engine that reads images", () => {
    const onVision = vi.fn();
    render(
      <LocaleProvider initialLocale="en">
        <DeploymentInspector
          value={params}
          onChange={vi.fn()}
          inferenceService={{ name: "Strata", settings: [], pomSettings: [] }}
          vision={{
            available: true,
            enabled: false,
            path: "/models/mmproj-qwen.gguf",
            candidates: ["/models/mmproj-qwen.gguf"],
            onChange: onVision,
            onPathChange: vi.fn(),
          }}
        />
      </LocaleProvider>,
    );
    fireEvent.click(screen.getByRole("checkbox", { name: "Enable vision" }));
    expect(onVision).toHaveBeenCalledWith(true);
  });
});
