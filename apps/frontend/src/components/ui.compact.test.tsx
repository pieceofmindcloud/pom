// @vitest-environment jsdom
/**
 * O defeito que este teste guarda: `clsx` concatena, não resolve conflito de
 * Tailwind. Enquanto o tamanho vinha do `className` do chamador, `h-6 px-1.5
 * text-[0.62rem]` e o padrão `h-9 px-3 text-sm` iam juntos para a marcação, e
 * quem decidia era a ordem da folha de estilo - o padrão. Os campos do inspetor
 * pediam compacto havia meses e renderizavam em tamanho cheio, sem erro nenhum
 * para denunciar.
 *
 * Por isso o teste é sobre a *ausência* das utilitárias de tamanho padrão: é o
 * empilhamento que causava o defeito, e ele voltaria mudo.
 */
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";

import { Input, Select } from "./ui";

describe("variante compacta", () => {
  afterEach(cleanup);

  it("troca as utilitárias de tamanho do Input em vez de empilhá-las", () => {
    render(<Input compact aria-label="campo" className="text-right font-mono" />);
    const cls = screen.getByLabelText("campo").className;

    expect(cls).toContain("h-6");
    expect(cls).toContain("px-1.5");
    expect(cls).toContain("text-[0.62rem]");
    // O tamanho padrão não pode viajar junto: era ele que vencia.
    expect(cls).not.toContain("h-9");
    expect(cls).not.toContain("px-3");
    expect(cls).not.toContain("text-sm");
    // O que o chamador manda que não é tamanho continua chegando.
    expect(cls).toContain("text-right");
  });

  it("mantém o tamanho padrão do Input sem a variante", () => {
    render(<Input aria-label="campo" />);
    const cls = screen.getByLabelText("campo").className;
    expect(cls).toContain("h-9");
    expect(cls).not.toContain("h-6");
  });

  it("troca as utilitárias de tamanho do Select em vez de empilhá-las", () => {
    render(<Select compact aria-label="escolha" />);
    const cls = screen.getByLabelText("escolha").className;
    expect(cls).toContain("h-6");
    expect(cls).not.toContain("h-9");
    expect(cls).not.toContain("text-sm");
  });
});
