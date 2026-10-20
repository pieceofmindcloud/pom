// @vitest-environment jsdom
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { ToastProvider } from "../lib/toast";
import type { LocalCaStatus } from "../lib/types";

const get = vi.fn();
const post = vi.fn();
vi.mock("../lib/api", async (original) => ({
  ...(await original<typeof import("../lib/api")>()),
  api: { get: (...args: unknown[]) => get(...args), post: (...args: unknown[]) => post(...args), getText: vi.fn() },
}));

const { HttpsCertificatePanel } = await import("./HttpsCertificatePanel");

function mount(status: LocalCaStatus) {
  get.mockResolvedValue(status);
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <LocaleProvider initialLocale="en">
        <ToastProvider>
          <HttpsCertificatePanel />
        </ToastProvider>
      </LocaleProvider>
    </QueryClientProvider>,
  );
}

const base: LocalCaStatus = {
  fingerprint: "AA:BB",
  trust: "not_trusted",
  declined: true,
  can_install: true,
  filename: "pom-machine-ca-aabb.crt",
};

describe("HttpsCertificatePanel", () => {
  afterEach(() => {
    cleanup();
    get.mockReset();
    post.mockReset();
  });

  it("offers the install again after a refusal, with the file name in the commands", async () => {
    mount(base);
    await screen.findByText("Not trusted");
    expect(screen.getByText(/refused the system dialog/)).toBeTruthy();
    expect(screen.getByTestId("https-certificate").textContent).toContain("certutil -user -addstore Root pom-machine-ca-aabb.crt");

    post.mockResolvedValue({ outcome: "installed", detail: "" });
    fireEvent.click(screen.getByText("Install on this computer"));
    await waitFor(() => expect(post).toHaveBeenCalledWith("/api/ui/tls/ca/install"));
  });

  it("does not offer the install from another machine or once trusted", async () => {
    mount({ ...base, can_install: false });
    await screen.findByText("Not trusted");
    expect(screen.queryByText("Install on this computer")).toBeNull();
    cleanup();
    mount({ ...base, trust: "trusted", declined: false });
    await screen.findByText("Trusted");
    expect(screen.queryByText("Install on this computer")).toBeNull();
    expect(screen.getByText("Download certificate (.crt)")).toBeTruthy();
  });
});
