// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import type { RuntimeParams } from "../lib/types";
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

function mount(showHiddenStateDtype: boolean, onChange = vi.fn()) {
  render(
    <LocaleProvider initialLocale="en">
      <DeploymentInspector
        value={params}
        onChange={onChange}
        showHiddenStateDtype={showHiddenStateDtype}
        inferenceService={{ name: "Strata", settings: [] }}
      />
    </LocaleProvider>,
  );
  return onChange;
}

describe("DeploymentInspector with a plugin's inference service", () => {
  it("offers the POM's transfer between the slices when they cross machines", () => {
    const onChange = mount(true);
    const precision = screen.getByRole("combobox", { name: "Hidden-state precision" });
    fireEvent.change(precision, { target: { value: "f32" } });
    expect(onChange).toHaveBeenCalledWith(expect.objectContaining({ hidden_state_dtype: "f32" }));
    expect(document.querySelector('input[name="prefill_max_inflight_chunks"]')).not.toBeNull();
  });

  it("leaves them out for a model on one machine", () => {
    mount(false);
    expect(screen.queryByRole("combobox", { name: "Hidden-state precision" })).toBeNull();
    expect(document.querySelector('input[name="prefill_max_inflight_chunks"]')).toBeNull();
  });
});
