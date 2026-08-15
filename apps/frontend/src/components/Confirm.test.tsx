// @vitest-environment jsdom
/**
 * A janela tem que sair de onde o botão está.
 *
 * `position: fixed` para de valer contra a janela do navegador quando algum
 * ancestral tem `transform`, `filter` ou `backdrop-filter` — ele vira o bloco
 * de contenção. A barra do topo e o cabeçalho do Painel têm `backdrop-blur`, e
 * as confirmações abertas de lá eram centralizadas dentro de uma faixa de
 * ~76 px: medido no navegador, a janela nascia em `y = -66` numa viewport de
 * 2029 px, com o título fora da tela.
 *
 * jsdom não calcula layout, então o que se afirma aqui é a causa e não o
 * sintoma: a janela é filha do `body`, longe de qualquer ancestral borrado.
 */
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { useState } from "react";
import { Confirm, Modal } from "./Confirm";

describe("Confirm", () => {
  afterEach(cleanup);

  it("desenha a janela no body, não dentro do ancestral com backdrop-blur", () => {
    render(
      <div className="backdrop-blur-md">
        <Confirm
          open
          title="Tirar ksia da rede?"
          description="a máquina para de anunciar"
          confirmLabel="Sair da rede"
          onConfirm={() => {}}
          onCancel={() => {}}
        />
      </div>,
    );

    const dialog = screen.getByRole("dialog");
    // O véu é o pai da janela; acima dele tem que estar o `body`.
    expect(dialog.parentElement?.parentElement).toBe(document.body);
    expect(dialog.closest(".backdrop-blur-md")).toBeNull();
  });

  it("não desenha nada enquanto está fechada", () => {
    render(
      <Confirm
        open={false}
        title="Tirar ksia da rede?"
        description="a máquina para de anunciar"
        confirmLabel="Sair da rede"
        onConfirm={() => {}}
        onCancel={() => {}}
      />,
    );
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  /**
   * Uma janela abre outra: a confirmação de "Sair da rede" nasce de dentro do
   * card que a janela de detalhes mostra. Como as duas escutam o Esc no
   * `document`, sem a pilha um Esc só fecharia as duas, e quem desistiu da
   * confirmação perderia também a tela que estava lendo.
   */
  it("o Esc fecha só a janela de cima", () => {
    const fecharDeBaixo = vi.fn();
    const fecharDeCima = vi.fn();
    function Tela() {
      const [confirmando, setConfirmando] = useState(false);
      return (
        <>
          <Modal
            open
            title="Detalhes de ksia"
            onClose={fecharDeBaixo}
            footer={<button onClick={() => setConfirmando(true)}>Sair da rede</button>}
          >
            card
          </Modal>
          <Modal open={confirmando} title="Tirar ksia da rede?" onClose={fecharDeCima} footer={null}>
            conteúdo
          </Modal>
        </>
      );
    }
    render(<Tela />);
    act(() => {
      fireEvent.click(screen.getByText("Sair da rede"));
    });

    act(() => {
      fireEvent.keyDown(document, { key: "Escape" });
    });
    expect(fecharDeCima).toHaveBeenCalledTimes(1);
    expect(fecharDeBaixo).not.toHaveBeenCalled();
  });
});
