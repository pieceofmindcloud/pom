// @vitest-environment jsdom
/**
 * A rota Documentação é auto-contida: o conteúdo viaja embutido no bundle
 * (`scripts/build-docs.mjs` gera `src/docs/` no build), então o que se testa
 * aqui é o contrato entre a tela e esse conjunto embutido - a árvore navega
 * pelas seções do repositório, a busca filtra título e corpo, e o documento
 * selecionado é um endereço (`?doc=`).
 *
 * O jsdom não calcula layout nem implementa scrollIntoView; o componente já
 * guarda a chamada, e estes testes não dependem de posição.
 */
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { MemoryRouter } from "react-router-dom";
import { LocaleProvider } from "../i18n/provider";
import { Documentation } from "./Documentation";

function renderDoc(initialEntry = "/documentacao") {
  return render(
    <MemoryRouter initialEntries={[initialEntry]}>
      <LocaleProvider initialLocale="en">
        <Documentation />
      </LocaleProvider>
    </MemoryRouter>,
  );
}

describe("Documentation", () => {
  afterEach(cleanup);

  it("abre o primeiro documento (AGENTS.md da raiz) e desenha a árvore de seções", () => {
    renderDoc();
    // O documento padrão é o primeiro do manifesto; o primeiro heading dele
    // rende como <h1> no viewer.
    expect(screen.getByRole("heading", { name: "CLAUDE.md", level: 1 })).toBeTruthy();
    // Cabeçalhos de seção; `getAllByRole` porque a mesma string pode casar
    // também com o nome de uma folha (ex.: "Open crates/").
    for (const pattern of [
      /Project root/,
      /Subsystem docs/,
      /crates\//,
      /apps\/frontend/,
      /scripts\//,
    ]) {
      expect(screen.getAllByRole("button", { name: pattern }).length).toBeGreaterThan(0);
    }
  });

  it("selecionar um documento da árvore o renderiza e ativa o item", () => {
    renderDoc();
    const leaf = screen.getByRole("button", { name: /Open .*Subsistema: transferência/ });
    fireEvent.click(leaf);
    // O h1 do viewer é o primeiro heading do README de subsistema.
    expect(
      screen.getByRole("heading", {
        name: "Subsistema: transferência de arquivos (fatias) entre máquinas",
        level: 1,
      }),
    ).toBeTruthy();
    // O item ativo marca aria-pressed no leaf.
    expect(leaf.getAttribute("aria-pressed")).toBe("true");
  });

  it("o parâmetro ?doc= é o endereço do documento", () => {
    renderDoc("/documentacao?doc=docs/subsystems/inferencia.md");
    // O h1 do viewer é o primeiro heading do README de subsistema.
    expect(
      screen.getByRole("heading", { name: "Caminho de inferência", level: 1 }),
    ).toBeTruthy();
  });

  it("a busca filtra por título e corpo; limpar volta à árvore", () => {
    renderDoc();
    const search = screen.getByLabelText("Search documentation");
    fireEvent.change(search, { target: { value: "transferência" } });
    // Com busca ativa, a árvore some e sobra a lista de resultados.
    expect(screen.queryByText("Project root")).toBeNull();
    expect(screen.getByRole("button", { name: /Open .*transferência/ })).toBeTruthy();

    fireEvent.click(screen.getByLabelText("Clear search"));
    expect(screen.getByText("Project root")).toBeTruthy();
  });

  it("uma busca sem resultado avisa em vez de quebrar", () => {
    renderDoc();
    fireEvent.change(screen.getByLabelText("Search documentation"), {
      target: { value: "zzz-não-existe" },
    });
    expect(screen.getByText("No documentation matches")).toBeTruthy();
  });
});
