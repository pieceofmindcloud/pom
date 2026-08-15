// @vitest-environment jsdom
import { act, cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { AddNodeDialog } from "./AddNodeDialog";
import { LocaleProvider } from "../i18n/provider";

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
});