// @vitest-environment jsdom
/**
 * A failed load must show the engine's reason, not a bare "failed", and its
 * log lines must be one click away.
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

  it("shows the engine's message and opens its log", () => {
    render(
      <LocaleProvider initialLocale="en">
        <EngineFailure failure={failure} machine="machine-a" />
      </LocaleProvider>,
    );
    expect(screen.getByRole("alert").textContent).toContain("CUDA error: out of memory");
    expect(screen.getByText("llama.cpp failed to load the slice (1 attempt)")).toBeTruthy();
    expect(screen.queryByText(/allocating 9 GiB/)).toBeNull();
    fireEvent.click(screen.getByText("2 engine log lines"));
    expect(screen.getByText(/allocating 9 GiB/)).toBeTruthy();
  });
});
