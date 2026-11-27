// @vitest-environment jsdom
/**
 * "+ Instance" starts a second POM on a machine: it asks first, saying what
 * it is for and what hardware it needs; removing an instance acts at once.
 */
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";

const post = vi.hoisted(() => vi.fn(async () => ({})));
vi.mock("../lib/api", async (original) => {
  const actual = await original<typeof import("../lib/api")>();
  return { ...actual, api: { ...actual.api, post } };
});

import { MachineInstanceAction } from "./DeployModel";
import { ToastProvider } from "../lib/toast";

const mount = (machine: { node_id: string; label: string; machine_id?: string }) =>
  render(
    <QueryClientProvider client={new QueryClient()}>
      <LocaleProvider initialLocale="en">
        <ToastProvider>
          <MemoryRouter>
            <MachineInstanceAction machine={machine} />
          </MemoryRouter>
        </ToastProvider>
      </LocaleProvider>
    </QueryClientProvider>,
  );

describe("MachineInstanceAction", () => {
  afterEach(() => { cleanup(); post.mockClear(); });

  it("explains a new instance and adds it only once confirmed", async () => {
    mount({ node_id: "host", label: "machine-a" });
    fireEvent.click(screen.getByTestId("instance-add-host"));
    expect(post).not.toHaveBeenCalled();
    const text = screen.getByTestId("instance-confirm-text").textContent ?? "";
    expect(text).toContain("same graphics card can serve two models");
    expect(text).toContain("more than one graphics card");
    expect(text).toContain("hardware has to hold it");
    fireEvent.click(screen.getByRole("button", { name: "Add instance" }));
    await waitFor(() => expect(post).toHaveBeenCalledWith("/api/ui/instances", { node_id: "host" }));
  });

  it("removes an instance at once", async () => {
    mount({ node_id: "inst", label: "machine-a #2", machine_id: "host" });
    fireEvent.click(screen.getByTestId("instance-remove-inst"));
    await waitFor(() =>
      expect(post).toHaveBeenCalledWith("/api/ui/instances/remove", { node_id: "host", instance_node_id: "inst" }),
    );
  });
});
