// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import type { PomSetting, RuntimeParams } from "../lib/types";
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
) {
  render(
    <LocaleProvider initialLocale="en">
      <DeploymentInspector
        value={params}
        onChange={onChange}
        showHiddenStateDtype={showHiddenStateDtype}
        inferenceService={{ name: "Strata", settings: [], pomSettings }}
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

  it("has no POM card for a service that declares none", () => {
    mount(true, vi.fn(), []);
    expect(screen.queryByText("POM settings")).toBeNull();
  });

  it("leaves them out for a model on one machine", () => {
    mount(false);
    expect(screen.queryByRole("combobox", { name: "Hidden-state precision" })).toBeNull();
    expect(document.querySelector('input[name="prefill_max_inflight_chunks"]')).toBeNull();
  });
});
