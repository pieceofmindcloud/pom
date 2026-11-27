// @vitest-environment jsdom
/**
 * A failed load shows one clear line on the machine's card and the whole
 * reason, with the engine's log, one click away.
 */
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { EngineFailure } from "./EngineFailure";
import { LocaleProvider } from "../i18n/provider";

const failure = {
  engine: "llama.cpp",
  message: "engine aborted: ggml-cuda.cu:96: CUDA error: out of memory",
  log_tail: ["llama_model_load: loading", "load_tensors: allocating 9 GiB"],
  attempts: 1,
  at: "2026-11-27T10:00:00Z",
};

describe("EngineFailure", () => {
  afterEach(cleanup);

  it("shows the error in a line and the details in a window", () => {
    render(
      <LocaleProvider initialLocale="en">
        <EngineFailure failure={failure} machine="machine-a" />
      </LocaleProvider>,
    );
    expect(screen.getByRole("alert").textContent).toContain("CUDA error: out of memory");
    expect(screen.queryByTestId("engine-failure-log")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Details" }));
    expect(screen.getByText("Deployment error on machine-a")).toBeTruthy();
    expect(screen.getByText("llama.cpp failed to load the slice (1 attempt)", { exact: false })).toBeTruthy();
    expect(screen.getByTestId("engine-failure-log").textContent).toContain("allocating 9 GiB");
  });

  it("says why there is no log for an engine outside the process", () => {
    render(
      <LocaleProvider initialLocale="en">
        <EngineFailure variant="icon" failure={{ ...failure, engine: "strata", log_tail: [] }} machine="machine-b" />
      </LocaleProvider>,
    );
    fireEvent.click(screen.getByTestId("engine-failure-icon"));
    expect(screen.getByText(/runs outside POM/)).toBeTruthy();
  });
});
