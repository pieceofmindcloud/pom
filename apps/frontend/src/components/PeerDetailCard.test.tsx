// @vitest-environment jsdom
/**
 * Máquina sem fatia não vira uma máquina com a fatia "0-0".
 *
 * O card foi escrito para quem serve uma faixa, e a aba Detalhes agora mostra
 * também quem está na rede sem servir nada. O que se afirma aqui é que nesse
 * caso ele não escreve faixa nenhuma, diz por quê, e não oferece o botão que
 * tira do modelo uma máquina que não está em modelo nenhum.
 */
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { PeerDetailCard, cardRange } from "./PeerDetailCard";
import { LocaleProvider } from "../i18n/provider";
import { fakeNode, fakePeer, fakeSlicePeer, fakeSystem } from "../test/fixtures";
import type { PeerView } from "../lib/types";

function mount(peer: PeerView) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <LocaleProvider initialLocale="pt-BR">
        <PeerDetailCard peer={peer} index={0} group="qwen35" otherRanges={[[16, 31]]} />
      </LocaleProvider>
    </QueryClientProvider>,
  );
}

const semFatia = fakePeer({
  node_id: "z",
  node: fakeNode({ node_id: "z", short_id: "z", hostname: "atena" }),
});

describe("PeerDetailCard sem fatia", () => {
  afterEach(cleanup);

  it("não escreve faixa de camadas nenhuma", () => {
    mount(semFatia);
    expect(screen.queryByText(/^L?0\s*[-–]\s*0$/)).toBeNull();
    expect(screen.queryByText(/NaN/)).toBeNull();
    expect(screen.getByText("sem fatia")).toBeTruthy();
  });

  it("diz que a máquina está na rede e não serve camada nenhuma", () => {
    mount(semFatia);
    expect(screen.getByText(/não serve nenhuma faixa de camadas/i)).toBeTruthy();
  });

  it("não oferece tirar do modelo uma máquina que não está em modelo nenhum", () => {
    mount(semFatia);
    expect(screen.queryByRole("button", { name: /remover/i })).toBeNull();
    // Sair da rede continua valendo: é do cluster que ela sai.
    expect(screen.getByRole("button", { name: /sair da rede/i })).toBeTruthy();
  });

  it("continua mostrando o que a máquina de fato reporta", () => {
    mount(semFatia);
    expect(screen.getByText("atena")).toBeTruthy();
    expect(screen.getByText("RAM")).toBeTruthy();
  });
});

describe("PeerDetailCard com fatia", () => {
  afterEach(cleanup);

  it("mostra a faixa que o peer anuncia", () => {
    mount(fakeSlicePeer("a", 0, 15));
    expect(screen.getByText(/0\s*[-–]\s*15/)).toBeTruthy();
  });
});

describe("PeerDetailCard sem GPU (nó Linux, ex. num container Docker)", () => {
  afterEach(cleanup);

  /// `wired_mb` só é populado no macOS (`sysmon.rs`); um nó Linux sem NVIDIA
  /// caía no mesmo ramo que mostrava "Wired memory (Metal buffers)" com
  /// `value={null}`, que trava o `Progress` no modo indeterminado para
  /// sempre — a barra "fica saltando e nunca se resolve", já que nada nesse
  /// nó jamais vai preencher `wired_mb`.
  const semGpu = fakePeer({
    node_id: "cpu-only",
    node: fakeNode({
      node_id: "cpu-only",
      short_id: "cpu-only",
      hostname: "node-cpu-1",
      system: fakeSystem({ hostname: "node-cpu-1", os: "linux", gpu_kind: "none", wired_mb: null }),
    }),
  });

  it("não mostra o medidor de memória wired do Metal", () => {
    mount(semGpu);
    expect(screen.queryByText(/wired/i)).toBeNull();
    expect(screen.queryByText(/metal/i)).toBeNull();
  });

  it("mostra uma barra num estado definido, não indeterminado para sempre", () => {
    mount(semGpu);
    const bars = screen.getAllByRole("progressbar");
    // Toda barra desta máquina tem que ter um valor definido — nenhuma pode
    // ficar sem `aria-valuenow`, que é como o indeterminado se denuncia.
    for (const bar of bars) {
      expect(bar.getAttribute("aria-valuenow")).not.toBeNull();
    }
  });
});

describe("cardRange", () => {
  it("devolve null quando não há faixa em lugar nenhum, em vez de 0-0", () => {
    expect(cardRange(semFatia)).toBeNull();
  });

  it("prefere o anúncio do peer", () => {
    expect(cardRange(fakeSlicePeer("a", 4, 9))).toEqual([4, 9]);
  });
});
