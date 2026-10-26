// @vitest-environment jsdom
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import type { ConnectionStatus } from "../lib/types";
import { ConnectionBadge } from "./ConnectionStatus";

let status: ConnectionStatus | undefined;
vi.mock("../lib/queries", () => ({ useConnection: () => ({ data: status }) }));

function mount() {
  return render(<LocaleProvider><ConnectionBadge /></LocaleProvider>);
}

describe("ConnectionBadge", () => {
  afterEach(cleanup);
  beforeEach(() => {
    status = undefined;
  });

  it("shows nothing on an admin or a node off the network", () => {
    status = { phase: "standalone", since: 0, last_error: null, peers: 3, admin_in_sight: false };
    mount();
    expect(screen.queryByTestId("connection-badge")).toBeNull();
  });

  it("stays on a connected member, with how many machines it sees", () => {
    status = { phase: "connected", since: 0, last_error: null, peers: 2, admin_in_sight: true };
    mount();
    const badge = screen.getByTestId("connection-badge");
    expect(badge.closest("[data-phase]")?.getAttribute("data-phase")).toBe("connected");
    expect(badge.textContent).toMatch(/2/);
  });

  it("warns when a connected member sees no machine of its network", () => {
    status = { phase: "connected", since: 0, last_error: null, peers: 0, admin_in_sight: false };
    mount();
    expect(screen.getByTestId("connection-badge").textContent).toMatch(/fora de alcance|out of sight/);
  });
});
