// @vitest-environment jsdom
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { GpuTemperature } from "./GpuTemperature";

const policy = { limit_c: 85, hysteresis_c: 8, cooldown_secs: 30 };

function mount(props: Parameters<typeof GpuTemperature>[0]) {
  return render(
    <LocaleProvider initialLocale="pt-BR">
      <GpuTemperature {...props} />
    </LocaleProvider>,
  );
}

describe("GpuTemperature", () => {
  afterEach(cleanup);

  it("renders nothing when the machine has no GPU", () => {
    const { container } = mount({
      gpuKind: "none",
      temperature: null,
      policy,
    });

    expect(container.innerHTML).toBe("");
  });

  it("shows a single indication when the GPU sensor does not answer", () => {
    mount({
      gpuKind: "nvidia",
      temperature: null,
      policy,
    });

    expect(screen.getAllByText("sensor indisponível")).toHaveLength(1);
    expect(screen.queryByText("temperatura desconhecida")).toBeNull();
  });
});
