// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { AddNodeDialog } from "./AddNodeDialog";
import { LocaleProvider } from "../i18n/provider";
import { api } from "../lib/api";

describe("AddNodeDialog", () => {
  afterEach(cleanup);

  it("avisa que a faixa escolhida cria uma rota paralela", () => {
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

  it("nao avisa nada quando a faixa preenche um buraco", () => {
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

  it("re-semeia a selecao a cada abertura e nao guarda o node_id de uma sessao passada", () => {
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

  it("mantem a maquina escolhida quando o painel repolla o cluster", () => {
    // O painel remonta `peers`/`slices` a cada resposta do cluster: arrays
    // novos, mesmo conteudo. A escolha do operador tem que sobreviver a isso.
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

  it("troca a escolha quando a maquina escolhida sai da rede", () => {
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

  it("reoferece as fatias e o modelo inteiro da maquina escolhida", async () => {
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
      expect(screen.getByText("Opções da máquina")).toBeTruthy();
      const comboboxes = screen.getAllByRole("combobox");
      fireEvent.change(comboboxes[1], { target: { value: "whole:0-39" } });
      fireEvent.change(comboboxes[2], { target: { value: "cpu" } });
      fireEvent.change(comboboxes[3], { target: { value: "q8_0" } });
      fireEvent.change(comboboxes[4], { target: { value: "bf16" } });
      fireEvent.click(screen.getByRole("button", { name: "Adicionar" }));
      await waitFor(() => {
        expect(post).toHaveBeenLastCalledWith(
          "/api/ui/deploy/node",
          expect.objectContaining({
            kind: "whole",
            start_layer: 0,
            end_layer: 39,
            offload_kqv: false,
            cache_type_k: "q8_0",
            cache_type_v: "bf16",
          }),
        );
      });
      expect(post).toHaveBeenLastCalledWith(
        "/api/ui/deploy/node",
        expect.objectContaining({ target_node_id: "n2" }),
      );
    } finally {
      post.mockRestore();
    }
  });

  it("não mostra o plano antigo enquanto lê o inventário real do alvo", async () => {
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
});
