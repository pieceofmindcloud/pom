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
import { keys } from "../lib/queries";
import type { PeerRuntime, PeerView } from "../lib/types";

function mount(peer: PeerView, runtime?: PeerRuntime) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  if (runtime) client.setQueryData(keys.peerRuntime(peer.node_id), runtime);
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

  it("avisa quando o MTP foi pedido mas não ficou disponível", () => {
    const peer = fakeSlicePeer("mtp", 0, 31, {
      node: fakeNode({
        node_id: "mtp",
        short_id: "mtp",
        hostname: "node-mtp",
        config: { mtp_enabled: true } as never,
      }),
    });
    mount(peer, {
      loaded: true,
      health: {
        mtp_ready: false,
        mtp_unavailable_reason: "falha ao criar o contexto MTP por falta de memória",
        placement: null,
      } as never,
    });

    expect(screen.getByText("MTP indisponível")).toBeTruthy();
    expect(screen.getByText("falha ao criar o contexto MTP por falta de memória")).toBeTruthy();
  });

  it("mostra quando o MTP ficou ativo na RAM", () => {
    const peer = fakeSlicePeer("mtp-ram", 0, 31, {
      node: fakeNode({
        node_id: "mtp-ram",
        short_id: "mtp-ram",
        hostname: "node-mtp-ram",
        config: { mtp_enabled: true } as never,
      }),
    });
    mount(peer, {
      loaded: true,
      health: {
        mtp_ready: true,
        mtp_allocation: "ram",
        placement: null,
      } as never,
    });

    expect(screen.getByText("MTP ativo na RAM")).toBeTruthy();
  });
});

describe("PeerDetailCard · parâmetros de carga", () => {
  const params = {
    n_ctx: 8192,
    n_ctx_seq: 8192,
    n_batch: 512,
    n_ubatch: 128,
    n_seq_max: 8,
    n_threads: 8,
    n_threads_batch: 8,
    flash_attn: true,
    flash_attn_auto: true,
    kv_unified: true,
    embeddings: true,
    n_rs_seq: 0,
    offload_kqv: true,
    cache_type_k: "f16",
    cache_type_v: "f16",
    n_gpu_layers: 99,
    fit_memory_applied: true,
  };

  function montaCom(load_params: unknown) {
    mount(fakeSlicePeer("lp", 0, 31), {
      loaded: true,
      health: { mtp_ready: false, placement: null, load_params } as never,
    });
  }

  afterEach(cleanup);

  it("mostra o lote e o microlote com que o contexto subiu", () => {
    montaCom(params);
    expect(screen.getByText("Parâmetros de carga do llama.cpp")).toBeTruthy();
    // O microlote é o que governa o prefill, e ele não é o n_batch: com o fit
    // ligado o llama.cpp o corta. Mostrar os dois é o ponto da seção.
    expect(screen.getByText("512 / 128")).toBeTruthy();
  });

  it("diz que o Flash Attention veio da sonda, não do pedido do operador", () => {
    montaCom(params);
    expect(screen.getByText("ligado (auto)")).toBeTruthy();
  });

  it("distingue o Flash Attention pedido do resolvido automaticamente", () => {
    montaCom({ ...params, flash_attn: false, flash_attn_auto: false });
    expect(screen.getByText("desligado (pedido)")).toBeTruthy();
  });

  it("diz que não sabe quando a ponte de diagnóstico não respondeu", () => {
    // O caminho que não pode existir é o `null` virar "desligado": isso faria
    // o operador caçar um problema de Flash Attention que ninguém reportou.
    montaCom({ ...params, flash_attn: null, flash_attn_auto: null });
    expect(screen.getByText("não sei")).toBeTruthy();
  });

  it("avisa que o fit mexeu nos valores pedidos", () => {
    montaCom(params);
    expect(screen.getByText(/O fit do llama.cpp ajustou este contexto/)).toBeTruthy();
  });

  it("omite a seção inteira num nó cujo /health não traz os parâmetros", () => {
    montaCom(undefined);
    expect(screen.queryByText("Parâmetros de carga do llama.cpp")).toBeNull();
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

  it("omite a seção de temperatura quando a máquina não tem GPU", () => {
    mount(semGpu);
    expect(screen.queryByText(/temperatura da gpu/i)).toBeNull();
    expect(screen.queryByLabelText(/temperatura da gpu/i)).toBeNull();
  });
});

describe("temperatura das GPUs", () => {
  afterEach(cleanup);

  it("mostra uma leitura e a faixa térmica para cada GPU", () => {
    const peer = fakeSlicePeer("thermal", 0, 15, {
      node: fakeNode({
        node_id: "thermal",
        short_id: "thermal",
        hostname: "node-thermal",
        system: fakeSystem({
          gpu_kind: "nvidia",
          gpus: [
            { index: 0, name: "RTX A", vendor: "nvidia", vram_total_mb: 1024, vram_used_mb: 512, vram_used_pct: 50, utilization_pct: 20, temperature_c: 72 },
            { index: 1, name: "RTX B", vendor: "nvidia", vram_total_mb: 1024, vram_used_mb: 512, vram_used_pct: 50, utilization_pct: 20, temperature_c: 90 },
          ],
        }),
      }),
    });
    mount(peer);
    expect(screen.getByText(/72,0 °C/)).toBeTruthy();
    expect(screen.getByText(/90,0 °C/)).toBeTruthy();
    expect(screen.getByText(/crítica/)).toBeTruthy();
  });

  it("mantém o aviso quando uma GPU existe mas o sensor está indisponível", () => {
    const peer = fakeSlicePeer("thermal-unknown", 0, 15, {
      node: fakeNode({
        node_id: "thermal-unknown",
        short_id: "thermal-unknown",
        hostname: "node-thermal-unknown",
        system: fakeSystem({
          gpu_kind: "nvidia",
          gpus: [
            { index: 0, name: "RTX A", vendor: "nvidia", vram_total_mb: 1024, vram_used_mb: 512, vram_used_pct: 50, utilization_pct: 20, temperature_c: null },
          ],
        }),
      }),
    });
    mount(peer);
    expect(screen.getByText("temperatura desconhecida · sensor indisponível")).toBeTruthy();
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
