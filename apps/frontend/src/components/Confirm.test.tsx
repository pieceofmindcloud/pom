// @vitest-environment jsdom
/**
 * The window has to come out from where the button is.
 *
 * `position: fixed` stops applying against the browser window when some
 * ancestor has `transform`, `filter` or `backdrop-filter` - it becomes the
 * containing block. The top bar and the Dashboard header have `backdrop-blur`, and
 * the confirmations opened from there were centered inside a strip of
 * ~76 px: measured in the browser, the window was born at `y = -66` in a viewport of
 * 2029 px, with the title off screen.
 *
 * jsdom does not compute layout, so what is asserted here is the cause and not the
 * symptom: the window is a child of `body`, far from any blurred ancestor.
 */
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { useState } from "react";
import { Confirm, Modal } from "./Confirm";

describe("Confirm", () => {
  afterEach(cleanup);

  it("draws the window in the body, not inside the ancestor with backdrop-blur", () => {
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
    // The veil is the window's parent; above it there must be the `body`.
    expect(dialog.parentElement?.parentElement).toBe(document.body);
    expect(dialog.closest(".backdrop-blur-md")).toBeNull();
  });

  it("draws nothing while it is closed", () => {
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
   * One window opens another: the "Leave the network" confirmation is born from inside the
   * card that the details window shows. Since both listen for Esc on
   * `document`, without the stack one Esc would close both, and whoever gave up on the
   * confirmation would also lose the screen they were reading.
   */
  it("Esc closes only the top window", () => {
    const closeFromBelow = vi.fn();
    const closeFromAbove = vi.fn();
    function Screen() {
      const [confirming, setConfirming] = useState(false);
      return (
        <>
          <Modal
            open
            title="Detalhes de ksia"
            onClose={closeFromBelow}
            footer={<button onClick={() => setConfirming(true)}>Sair da rede</button>}
          >
            card
          </Modal>
          <Modal open={confirming} title="Tirar ksia da rede?" onClose={closeFromAbove} footer={null}>
            conteúdo
          </Modal>
        </>
      );
    }
    render(<Screen />);
    act(() => {
      fireEvent.click(screen.getByText("Sair da rede"));
    });

    act(() => {
      fireEvent.keyDown(document, { key: "Escape" });
    });
    expect(closeFromAbove).toHaveBeenCalledTimes(1);
    expect(closeFromBelow).not.toHaveBeenCalled();
  });
});
