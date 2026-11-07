// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import type { ChatImageWorkflow } from "../lib/types";
import { WorkflowForm } from "./WorkflowForm";

afterEach(cleanup);

const WORKFLOW: ChatImageWorkflow = {
  id: "pub:clip",
  name: "Clip",
  node_id: "n2",
  params: [
    { key: "prompt", label: "Prompt", type: "text", default: "a cat" },
    { key: "frames", label: "Frames", type: "integer", min: 8, max: 64, default: 16 },
    { key: "loop", label: "Loop", type: "boolean", default: false },
    { key: "sampler", label: "Sampler", type: "choice", options: ["euler", "dpmpp_2m"], default: "euler" },
    { key: "image", label: "Start frame", type: "image" },
  ],
};

describe("WorkflowForm", () => {
  it("draws a field per parameter and submits the values", () => {
    const onRun = vi.fn();
    render(<LocaleProvider><WorkflowForm workflow={WORKFLOW} open onClose={() => {}} onRun={onRun} /></LocaleProvider>);
    expect((screen.getByLabelText("Prompt") as HTMLTextAreaElement).value).toBe("a cat");
    const frames = screen.getByLabelText("Frames") as HTMLInputElement;
    expect(frames.min).toBe("8");
    fireEvent.change(frames, { target: { value: "32" } });
    fireEvent.click(screen.getByLabelText("Loop"));
    fireEvent.change(screen.getByLabelText("Sampler"), { target: { value: "dpmpp_2m" } });
    expect(screen.getByLabelText("Start frame")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Run" }));
    expect(onRun).toHaveBeenCalledWith({ prompt: "a cat", frames: 32, loop: true, sampler: "dpmpp_2m" });
  });
});
