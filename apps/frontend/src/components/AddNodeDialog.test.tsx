// @vitest-environment jsdom
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { AddNodeDialog } from "./AddNodeDialog";
import { LocaleProvider } from "../i18n/provider";

describe("AddNodeDialog", () => {
  // Sem `globals: true` no vitest o auto-cleanup nao roda, e o DOM de um teste
  // apareceria como resultado do seguinte.
  afterEach(cleanup);

  // O idioma é fixado aqui de propósito: o aviso é o assunto do teste, e sem
  // provider `t` devolveria a chave crua, que passaria por qualquer texto.
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

  it("não avisa nada quando a faixa preenche um buraco", () => {
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
});
