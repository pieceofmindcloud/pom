// @vitest-environment jsdom
/**
 * Quem pode virar o TLS do cluster, e o que vê quem não pode.
 *
 * A autoridade de verdade é do servidor (`save_tls` responde 403 fora do
 * principal). O que se afirma aqui é que a tela não mente antes do clique: no
 * principal o interruptor funciona, e num nó comum ele aparece desligado para
 * uso mas o **estado** continua visível - esconder o painel faria o operador de
 * um nó comum não ter onde ver se o tráfego da máquina dele está cifrado.
 */
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { ThermalPanel, TlsPanel } from "./NetworkSettings";

function mount(props: Parameters<typeof TlsPanel>[0]) {
  return render(
    <LocaleProvider initialLocale="pt-BR">
      <TlsPanel {...props} />
    </LocaleProvider>,
  );
}

function mountThermal(onSave: (policy: { limit_c: number; hysteresis_c: number; cooldown_secs: number }) => void) {
  return render(
    <LocaleProvider initialLocale="pt-BR">
      <ThermalPanel
        policy={{ limit_c: 85, hysteresis_c: 8, cooldown_secs: 30 }}
        onSave={onSave}
      />
    </LocaleProvider>,
  );
}

describe("TlsPanel", () => {
  afterEach(cleanup);

  it("no principal, o interruptor é usável e propaga a virada", () => {
    const onToggle = vi.fn();
    mount({ tlsRequired: false, authority: true, onToggle });
    const botao = screen.getByRole("switch");
    expect((botao as HTMLButtonElement).disabled).toBe(false);
    fireEvent.click(botao);
    expect(onToggle).toHaveBeenCalledWith(true);
  });

  it("ligado, o principal pode desligar - a virada vale nos dois sentidos", () => {
    const onToggle = vi.fn();
    mount({ tlsRequired: true, authority: true, onToggle });
    fireEvent.click(screen.getByRole("switch"));
    expect(onToggle).toHaveBeenCalledWith(false);
  });

  it("fora do principal, mostra o estado mas não deixa mexer, e diz quem decide", () => {
    mount({ tlsRequired: true, authority: false });
    expect((screen.getByRole("switch") as HTMLButtonElement).disabled).toBe(true);
    expect(screen.getByText("cifrado")).toBeTruthy();
    expect(screen.getByText(/só o nó principal decide/i)).toBeTruthy();
  });

  it("desligado, o estado é dito com todas as letras", () => {
    mount({ tlsRequired: false, authority: false });
    expect(screen.getByText("em texto claro")).toBeTruthy();
  });
});

describe("ThermalPanel", () => {
  afterEach(cleanup);

  it("expõe histerese e cooldown e envia a política editada", () => {
    const onSave = vi.fn();
    mountThermal(onSave);
    fireEvent.change(screen.getByLabelText(/desimplantar acima/i), { target: { value: "82" } });
    fireEvent.change(screen.getByLabelText(/margem de segurança/i), { target: { value: "7" } });
    fireEvent.change(screen.getByLabelText(/tempo de resfriamento/i), { target: { value: "45" } });
    fireEvent.click(screen.getByRole("button", { name: /salvar política térmica/i }));
    expect(onSave).toHaveBeenCalledWith({ limit_c: 82, hysteresis_c: 7, cooldown_secs: 45 });
    expect(screen.getByText(/limite de retomada automática: 75 °C/i)).toBeTruthy();
  });
});
