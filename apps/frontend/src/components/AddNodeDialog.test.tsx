// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { AddNodeDialog } from "./AddNodeDialog";
import { LocaleProvider } from "../i18n/provider";
import { api } from "../lib/api";

describe("AddNodeDialog", () => {
  afterEach(cleanup);

  it("warns that the chosen range creates a parallel route", () => {
    render(
      <LocaleProvider initialLocale="pt-BR">
        <AddNodeDialog
          open
          peers={[{ node_id: "n2", short_id: "n2", hostname: "salem" }]}
          slices={[{ start_layer: 0, end_layer: 19, label: "0-19" }]}
          existingRanges={[[0, 19]]}
          onClose={() => {}}
        />
      </LocaleProvider>,
    );
    expect(screen.getByText(/rota paralela/i)).toBeTruthy();
  });

  it("warns about nothing when the range fills a hole", () => {
    render(
      <LocaleProvider initialLocale="pt-BR">
        <AddNodeDialog
          open
          peers={[{ node_id: "n2", short_id: "n2", hostname: "salem" }]}
          slices={[{ start_layer: 20, end_layer: 39, label: "20-39" }]}
          existingRanges={[[0, 19]]}
          onClose={() => {}}
        />
      </LocaleProvider>,
    );
    expect(screen.queryByText(/rota paralela/i)).toBeNull();
  });

  it("re-seeds the selection on every open and does not keep the node_id of a past session", () => {
    const { rerender } = render(
      <LocaleProvider initialLocale="pt-BR">
        <AddNodeDialog
          open
          peers={[{ node_id: "n1", short_id: "n1", hostname: "maq1" }]}
          slices={[{ start_layer: 0, end_layer: 19, label: "0-19" }]}
          existingRanges={[]}
          onClose={() => {}}
        />
      </LocaleProvider>,
    );

    const select = screen.getByDisplayValue("maq1") as HTMLSelectElement;
    expect(select.value).toBe("n1");

    rerender(
      <LocaleProvider initialLocale="pt-BR">
        <AddNodeDialog
          open={false}
          peers={[{ node_id: "n1", short_id: "n1", hostname: "maq1" }]}
          slices={[{ start_layer: 0, end_layer: 19, label: "0-19" }]}
          existingRanges={[]}
          onClose={() => {}}
        />
      </LocaleProvider>,
    );

    rerender(
      <LocaleProvider initialLocale="pt-BR">
        <AddNodeDialog
          open
          peers={[{ node_id: "n3", short_id: "n3", hostname: "maq3" }]}
          slices={[{ start_layer: 20, end_layer: 39, label: "20-39" }]}
          existingRanges={[[0, 19]]}
          onClose={() => {}}
        />
      </LocaleProvider>,
    );

    const select2 = screen.getByDisplayValue("maq3") as HTMLSelectElement;
    expect(select2.value).toBe("n3");
  });

  it("keeps the chosen machine when the panel re-polls the cluster", () => {
    // The panel remounts `peers`/`slices` on every cluster response: new
    // arrays, same content. The operator's choice has to survive that.
    const peers = () => [
      { node_id: "n1", short_id: "n1", hostname: "maq1" },
      { node_id: "n2", short_id: "n2", hostname: "maq2" },
    ];
    const slices = () => [
      { start_layer: 0, end_layer: 19, label: "0-19" },
      { start_layer: 20, end_layer: 39, label: "20-39" },
    ];
    const view = () => (
      <LocaleProvider initialLocale="pt-BR">
        <AddNodeDialog
          open
          peers={peers()}
          slices={slices()}
          existingRanges={[]}
          onClose={() => {}}
        />
      </LocaleProvider>
    );
    const { rerender } = render(view());

    const machine = screen.getByDisplayValue("maq1") as HTMLSelectElement;
    const slice = screen.getByDisplayValue("0-19") as HTMLSelectElement;
    act(() => {
      machine.value = "n2";
      machine.dispatchEvent(new Event("change", { bubbles: true }));
      slice.value = "20-39";
      slice.dispatchEvent(new Event("change", { bubbles: true }));
    });
    expect(machine.value).toBe("n2");
    expect(slice.value).toBe("20-39");

    rerender(view());

    expect((screen.getByDisplayValue("maq2") as HTMLSelectElement).value).toBe("n2");
    expect((screen.getByDisplayValue("20-39") as HTMLSelectElement).value).toBe("20-39");
  });

  it("switches the choice when the chosen machine leaves the network", () => {
    const { rerender } = render(
      <LocaleProvider initialLocale="pt-BR">
        <AddNodeDialog
          open
          peers={[
            { node_id: "n1", short_id: "n1", hostname: "maq1" },
            { node_id: "n2", short_id: "n2", hostname: "maq2" },
          ]}
          slices={[{ start_layer: 0, end_layer: 19, label: "0-19" }]}
          existingRanges={[]}
          onClose={() => {}}
        />
      </LocaleProvider>,
    );
    const machine = screen.getByDisplayValue("maq1") as HTMLSelectElement;
    act(() => {
      machine.value = "n2";
      machine.dispatchEvent(new Event("change", { bubbles: true }));
    });

    rerender(
      <LocaleProvider initialLocale="pt-BR">
        <AddNodeDialog
          open
          peers={[{ node_id: "n1", short_id: "n1", hostname: "maq1" }]}
          slices={[{ start_layer: 0, end_layer: 19, label: "0-19" }]}
          existingRanges={[]}
          onClose={() => {}}
        />
      </LocaleProvider>,
    );

    expect((screen.getByDisplayValue("maq1") as HTMLSelectElement).value).toBe("n1");
  });

  it("offers again the slices and the whole model of the chosen machine", async () => {
    const get = vi.spyOn(api, "get").mockImplementation(async (path) => {
      if (path !== "/api/ui/deployments") {
        throw new Error(`rota inesperada: ${path}`);
      }
      return {
        node_id: "n1",
        deployments: [
          {
            deployment: {
              deploy_id: "d1",
              active_version: 1,
              versions: [{ version: 1, params: { n_ctx: 8192, offload_kqv: true } }],
            },
            serving: true,
            is_default: true,
            holders: [],
          },
        ],
      } as never;
    });
    const post = vi.spyOn(api, "post").mockImplementation(async (path, body) => {
      if (path === "/api/ui/deploy/node") {
        return { overlapping_layers: [] } as never;
      }
      if (path !== "/api/ui/deploy/node/options") {
        throw new Error(`rota inesperada: ${path}`);
      }
      const target = (body as { target_node_id: string }).target_node_id;
      return (target === "n1"
        ? {
            options: [{ kind: "shards", start_layer: 0, end_layer: 9, label: "0-9" }],
            cache_types: { cache_type_k: "f16", cache_type_v: "f16", choices: ["f16", "q8_0"] },
            defaults: { n_gpu_layers: 99, offload_kqv: true },
          }
        : {
            options: [
              { kind: "whole", start_layer: 0, end_layer: 39, label: "whole" },
              { kind: "shards", start_layer: 20, end_layer: 39, label: "20-39" },
            ],
            cache_types: { cache_type_k: "q8_0", cache_type_v: "bf16", choices: ["f16", "q8_0", "bf16"] },
            defaults: { n_gpu_layers: 24, offload_kqv: false },
          }) as never;
    });

    try {
      render(
        <LocaleProvider initialLocale="pt-BR">
          <AddNodeDialog
            open
            group="modelo"
            deployId="d1"
            peers={[
              { node_id: "n1", short_id: "n1", hostname: "maq1" },
              { node_id: "n2", short_id: "n2", hostname: "maq2" },
            ]}
            slices={[]}
            existingRanges={[]}
            onClose={() => {}}
          />
        </LocaleProvider>,
      );

      await waitFor(() => expect(screen.getByDisplayValue("0-9")).toBeTruthy());
      const machine = screen.getByDisplayValue("maq1") as HTMLSelectElement;
      fireEvent.change(machine, { target: { value: "n2" } });

      await waitFor(() => {
        expect(screen.getByRole("option", { name: /modelo inteiro/i })).toBeTruthy();
        expect(screen.getByRole("option", { name: "20-39" })).toBeTruthy();
      });
      // the machine's configuration is the deployment's active version, edited with the deployment inspector
      await waitFor(() => expect(get).toHaveBeenCalledWith("/api/ui/deployments"));
      await waitFor(() => expect(screen.getAllByRole("combobox").length).toBeGreaterThan(5));
      fireEvent.change(screen.getAllByRole("combobox")[1], { target: { value: "whole:0-39" } });
      fireEvent.click(screen.getByRole("button", { name: "Adicionar" }));
      await waitFor(() => {
        expect(post).toHaveBeenLastCalledWith(
          "/api/ui/deploy/node",
          expect.objectContaining({
            kind: "whole",
            start_layer: 0,
            end_layer: 39,
            cache_type_k: "q8_0",
            cache_type_v: "bf16",
            overrides: { params: expect.objectContaining({ n_ctx: 8192, offload_kqv: true }) },
          }),
        );
      });
      expect(post).toHaveBeenLastCalledWith(
        "/api/ui/deploy/node",
        expect.objectContaining({ target_node_id: "n2" }),
      );
    } finally {
      post.mockRestore();
      get.mockRestore();
    }
  });

  it("does not show the old plan while reading the real inventory of the target", async () => {
    const post = vi.spyOn(api, "post").mockResolvedValue({
      options: [],
      cache_types: { cache_type_k: "f16", cache_type_v: "f16", choices: ["f16"] },
      defaults: { n_gpu_layers: 99, offload_kqv: true },
    } as never);

    try {
      render(
        <LocaleProvider initialLocale="pt-BR">
          <AddNodeDialog
            open
            group="modelo"
            peers={[{ node_id: "salem", short_id: "salem", hostname: "salem-gpu" }]}
            slices={[{ start_layer: 16, end_layer: 40, label: "L16-40" }]}
            existingRanges={[]}
            onClose={() => {}}
          />
        </LocaleProvider>,
      );

      await waitFor(() => expect(post).toHaveBeenCalled());
      expect(screen.queryByRole("option", { name: "L16-40" })).toBeNull();
    } finally {
      post.mockRestore();
    }
  });

  it("offers the engines of the chosen machine and sends the chosen one with its machine", async () => {
    const get = vi.spyOn(api, "get").mockImplementation(async () => ({
      node_id: "n1",
      deployments: [
        {
          deployment: {
            deploy_id: "d1",
            active_version: 1,
            versions: [{ version: 1, params: { n_ctx: 8192 } }],
          },
          serving: true,
          is_default: true,
          holders: [],
        },
      ],
    }) as never);
    const post = vi.spyOn(api, "post").mockImplementation(async (path, body) => {
      if (path === "/api/ui/deploy/node") return { overlapping_layers: [] } as never;
      return {
        options: [
          { kind: "whole", start_layer: 0, end_layer: 39, label: "whole" },
          { kind: "shards", start_layer: 20, end_layer: 39, label: "20-39" },
        ],
        cache_types: { cache_type_k: "f16", cache_type_v: "f16", choices: ["f16"] },
        defaults: { n_gpu_layers: 99, offload_kqv: true },
      } as never;
    });
    const strata = {
      code: "strata",
      name: "Strata",
      architectures: ["qwen35moe"],
      settings: [],
      pomSettings: [],
      nodeIds: ["n2"],
      stages: false,
      vision: false,
    };
    try {
      render(
        <LocaleProvider initialLocale="pt-BR">
          <AddNodeDialog
            open
            group="modelo"
            deployId="d1"
            services={[strata]}
            peers={[
              { node_id: "n1", short_id: "n1", hostname: "maq1" },
              { node_id: "n2", short_id: "n2", hostname: "maq2" },
            ]}
            slices={[]}
            existingRanges={[]}
            onClose={() => {}}
          />
        </LocaleProvider>,
      );
      // the first machine does not run Strata: no engine choice there
      await waitFor(() => expect(get).toHaveBeenCalledWith("/api/ui/deployments"));
      expect(screen.queryByRole("option", { name: "Strata" })).toBeNull();

      fireEvent.change(screen.getByDisplayValue("maq1"), { target: { value: "n2" } });
      await waitFor(() => expect(screen.getByRole("option", { name: "Strata" })).toBeTruthy());
      fireEvent.change(screen.getByRole("option", { name: "Strata" }).closest("select") as HTMLSelectElement, {
        target: { value: "strata" },
      });
      // a service that runs no layer slices takes the whole model only
      await waitFor(() => expect(screen.queryByRole("option", { name: "20-39" })).toBeNull());
      fireEvent.click(screen.getByRole("button", { name: "Adicionar" }));
      await waitFor(() => {
        expect(post).toHaveBeenLastCalledWith(
          "/api/ui/deploy/node",
          expect.objectContaining({
            target_node_id: "n2",
            kind: "whole",
            overrides: { params: expect.objectContaining({ inference_service: "strata", n_ctx: 8192 }) },
          }),
        );
      });
    } finally {
      post.mockRestore();
      get.mockRestore();
    }
  });
});
