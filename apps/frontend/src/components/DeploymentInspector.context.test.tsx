// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import type { RuntimeParams } from "../lib/types";
import { DeploymentInspector, type VisionControl } from "./DeploymentInspector";

afterEach(cleanup);

const params: RuntimeParams = {
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

describe("DeploymentInspector context ceiling", () => {
  it("caps n_ctx at the model's declared context_length", () => {
    const onChange = vi.fn();
    render(
      <LocaleProvider initialLocale="en">
        <DeploymentInspector value={params} onChange={onChange} contextLimit={262144} />
      </LocaleProvider>,
    );
    const field = screen.getByRole("spinbutton", { name: "Context" });
    expect(field.getAttribute("max")).toBe("262144");
    fireEvent.change(field, { target: { value: "300000" } });
    expect(onChange).toHaveBeenLastCalledWith(expect.objectContaining({ n_ctx: 262144 }));
    fireEvent.change(field, { target: { value: "200000" } });
    expect(onChange).toHaveBeenLastCalledWith(expect.objectContaining({ n_ctx: 200000 }));
  });

  it("sets no ceiling when the model declares none", () => {
    render(
      <LocaleProvider initialLocale="en">
        <DeploymentInspector value={params} onChange={vi.fn()} />
      </LocaleProvider>,
    );
    expect(screen.getByRole("spinbutton", { name: "Context" }).getAttribute("max")).toBeNull();
  });
});

describe("DeploymentInspector hidden-state precision", () => {
  const cap = (state: string) => ({ state, source: "peers", reason: "test" });
  function f8Option(state: string) {
    render(
      <LocaleProvider initialLocale="en">
        <DeploymentInspector
          value={params}
          onChange={vi.fn()}
          showHiddenStateDtype
          capabilities={{
            hidden_state_f8: cap(state),
            hidden_state_i4g64: cap(state),
            hidden_state_i4g128: cap(state),
          } as never}
        />
      </LocaleProvider>,
    );
    const select = screen.getByRole("combobox", { name: /hidden/i }) as HTMLSelectElement;
    return [...select.options].find((option) => option.value === "f8")!;
  }

  it("lets f8 be chosen while the surveyed plan cannot confirm the peers", () => {
    expect(f8Option("unknown").disabled).toBe(false);
  });

  it("blocks f8 when a peer does not announce it", () => {
    expect(f8Option("unsupported").disabled).toBe(true);
  });
});
