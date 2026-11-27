// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";

const calls = vi.hoisted(() => [] as Array<[string, string | undefined]>);
vi.mock("../components/LogsTabs", () => ({
  useLogSources: () => ({
    selfId: "a",
    loading: false,
    sources: [{ code: "vllm", name: "vLLM", machines: [{ nodeId: "a", name: "machine-a" }, { nodeId: "b", name: "machine-b" }] }],
  }),
}));
vi.mock("../lib/queries", () => ({
  useLogSource: (source: string, nodeId: string | undefined) => {
    calls.push([source, nodeId]);
    return { data: { source, running: true, lines: [nodeId ? `remote ${nodeId}` : "local line"] }, isLoading: false, isError: false };
  },
}));

import { LogSource } from "./LogSource";

const renderAt = (path: string) =>
  render(
    <LocaleProvider initialLocale="en">
      <MemoryRouter initialEntries={[path]}>
        <Routes>
          <Route path="/logs/source/:code" element={<LogSource />} />
        </Routes>
      </MemoryRouter>
    </LocaleProvider>,
  );

describe("LogSource", () => {
  afterEach(() => { cleanup(); calls.length = 0; });

  it("reads this machine first and another machine through the picker", () => {
    renderAt("/logs/source/vllm");
    expect(screen.getByText("local line")).toBeTruthy();
    expect(calls.at(-1)).toEqual(["vllm", undefined]);
    fireEvent.change(screen.getByLabelText("Machine"), { target: { value: "b" } });
    expect(screen.getByText("remote b")).toBeTruthy();
    expect(calls.at(-1)).toEqual(["vllm", "b"]);
  });
});
