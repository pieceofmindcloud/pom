// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import type {
  AdminAuthorityMember,
  AdminAuthorityView,
  AdminNode,
} from "../lib/types";
import { AdminAuthorityPanel } from "./AdminAuthorityPanel";

const member = (
  node_id: string,
  hostname: string,
  active = true,
): AdminAuthorityMember => ({
  node_id,
  short_id: node_id.slice(0, 8),
  hostname,
  active,
});

const candidate = (node_id: string, hostname: string): AdminNode => ({
  node_id,
  short_id: node_id.slice(0, 8),
  hostname,
  addr: "127.0.0.1:8080",
  role: "worker",
  is_self: false,
  connected: true,
  blocked: false,
  verified: true,
});

const baseAuthority = (
  overrides: Partial<AdminAuthorityView> = {},
): AdminAuthorityView => ({
  viewer: "principal",
  principal: member("principal-1", "captain"),
  secondary: [member("secondary-1", "salem"), member("secondary-2", "ksia")],
  grants: [
    {
      sequence: 1,
      node_id: "secondary-1",
      node_hostname: "salem",
      promoted_by_node_id: "principal-1",
      promoted_by_hostname: "captain",
    },
    {
      sequence: 2,
      node_id: "secondary-2",
      node_hostname: "ksia",
      promoted_by_node_id: "principal-1",
      promoted_by_hostname: "captain",
    },
  ],
  resignation: {
    kind: "eligible",
    successor: member("secondary-2", "ksia"),
  },
  pending_transfer: null,
  permissions: { promote: true, resign: true, demote: true },
  ...overrides,
});

function mount(
  authority: AdminAuthorityView,
  callbacks: Partial<React.ComponentProps<typeof AdminAuthorityPanel>> = {},
) {
  return render(
    <LocaleProvider initialLocale="pt-BR">
      <AdminAuthorityPanel
        authority={authority}
        candidates={[candidate("node-1", "node-box")]}
        busyAction={null}
        onPromote={vi.fn()}
        onResign={vi.fn()}
        onDemote={vi.fn()}
        {...callbacks}
      />
    </LocaleProvider>,
  );
}

describe("AdminAuthorityPanel", () => {
  afterEach(cleanup);

  it("principal sees promotion, successor preview, demotion and the admin chain", () => {
    const onPromote = vi.fn();
    const onResign = vi.fn();
    const onDemote = vi.fn();
    mount(baseAuthority(), { onPromote, onResign, onDemote });

    expect(screen.getByRole("heading", { name: "Administração da rede" })).toBeTruthy();
    expect(screen.getByText("captain")).toBeTruthy();
    expect(screen.getByText("salem")).toBeTruthy();
    expect(screen.getAllByText("ksia")).toHaveLength(2);
    expect(screen.getAllByText(/último nó promovido/i)).toHaveLength(2);
    expect(screen.getByRole("combobox", { name: /nó para promover/i })).toBeTruthy();
    expect(screen.getByRole("button", { name: /promover a administrador/i })).toBeTruthy();
    const resign = screen.getByRole("button", { name: /transferir papel de principal/i });
    expect(resign).toBeTruthy();
    expect(screen.getAllByRole("button", { name: /despromover/i })).toHaveLength(2);

    fireEvent.change(screen.getByRole("combobox", { name: /nó para promover/i }), {
      target: { value: "node-1" },
    });
    fireEvent.click(screen.getByRole("button", { name: /promover a administrador/i }));
    fireEvent.click(
      within(screen.getByRole("dialog")).getByRole("button", {
        name: /promover a administrador/i,
      }),
    );
    expect(onPromote).toHaveBeenCalledWith("node-1");

    fireEvent.click(resign);
    fireEvent.click(
      within(screen.getAllByRole("dialog").at(-1)!).getByRole("button", {
        name: /transferir papel de principal/i,
      }),
    );
    expect(onResign).toHaveBeenCalledOnce();

    fireEvent.click(screen.getByRole("button", { name: /despromover salem/i }));
    fireEvent.click(
      within(screen.getAllByRole("dialog").at(-1)!).getByRole("button", {
        name: /despromover/i,
      }),
    );
    expect(onDemote).toHaveBeenCalledWith("secondary-1");
  });

  it("secondary admin sees the authority chain but no principal actions", () => {
    mount(
      baseAuthority({
        viewer: "admin",
        permissions: { promote: false, resign: false, demote: false },
      }),
    );

    expect(screen.getByText(/somente o principal administra a cadeia/i)).toBeTruthy();
    expect(screen.queryByRole("combobox", { name: /nó para promover/i })).toBeNull();
    expect(screen.queryByRole("button", { name: /transferir papel de principal/i })).toBeNull();
    expect(screen.queryByRole("button", { name: /despromover/i })).toBeNull();
  });

  it("common node sees who administers the network without actions", () => {
    mount(
      baseAuthority({
        viewer: "member",
        permissions: { promote: false, resign: false, demote: false },
      }),
    );

    expect(screen.getByText("captain")).toBeTruthy();
    expect(screen.getByText(/somente administradores podem alterar/i)).toBeTruthy();
    expect(screen.queryByRole("button", { name: /promover a administrador/i })).toBeNull();
  });

  it("explains separately why resignation is refused", () => {
    const { rerender } = render(
      <LocaleProvider initialLocale="pt-BR">
        <AdminAuthorityPanel
          authority={baseAuthority({
            secondary: [],
            resignation: { kind: "no_secondary" },
          })}
          candidates={[]}
          busyAction={null}
          onPromote={vi.fn()}
          onResign={vi.fn()}
          onDemote={vi.fn()}
        />
      </LocaleProvider>,
    );
    expect(screen.getByRole("alert").textContent).toMatch(/não há admins secundários/i);

    rerender(
      <LocaleProvider initialLocale="pt-BR">
        <AdminAuthorityPanel
          authority={baseAuthority({
            resignation: { kind: "no_active_secondary" },
          })}
          candidates={[]}
          busyAction={null}
          onPromote={vi.fn()}
          onResign={vi.fn()}
          onDemote={vi.fn()}
        />
      </LocaleProvider>,
    );
    expect(screen.getByRole("alert").textContent).toMatch(/nenhum admin secundário está ativo/i);
  });
});
