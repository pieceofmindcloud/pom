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
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
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

const withoutSlice = fakePeer({
  node_id: "z",
  node: fakeNode({ node_id: "z", short_id: "z", hostname: "atena" }),
});

describe("PeerDetailCard without a slice", () => {
  afterEach(cleanup);

  it("writes no layer range at all", () => {
    mount(withoutSlice);
    expect(screen.queryByText(/^L?0\s*[-–]\s*0$/)).toBeNull();
    expect(screen.queryByText(/NaN/)).toBeNull();
    expect(screen.getByText("sem fatia")).toBeTruthy();
  });

  /**
   * A frase saiu da faixa que ocupava uma linha inteira do card e entrou no
   * `[i]` do próprio distintivo. O que se afirma continua sendo o mesmo: quem
   * perguntar "por que sem fatia?" recebe resposta, sem que o card gaste uma
   * linha com ela em toda máquina ociosa da rede.
   */
  it("says the machine is on the network and serves no layer", () => {
    mount(withoutSlice);
    fireEvent.click(screen.getByRole("button", { name: "O que é sem fatia" }));
    expect(screen.getByText(/não serve nenhuma faixa de camadas/i)).toBeTruthy();
  });

  it("does not offer to remove from the model a machine that is in no model", () => {
    mount(withoutSlice);
    expect(screen.queryByRole("button", { name: /remover/i })).toBeNull();
    // Sair da rede continua valendo: é do cluster que ela sai.
    expect(screen.getByRole("button", { name: /sair da rede/i })).toBeTruthy();
  });

  it("keeps showing what the machine actually reports", () => {
    mount(withoutSlice);
    expect(screen.getByText("atena")).toBeTruthy();
    expect(screen.getByText("RAM")).toBeTruthy();
  });
});

describe("PeerDetailCard with a slice", () => {
  afterEach(cleanup);

  it("shows the range the peer announces", () => {
    mount(fakeSlicePeer("a", 0, 15));
    expect(screen.getByText(/0\s*[-–]\s*15/)).toBeTruthy();
  });

  it("warns when MTP was requested but did not become available", () => {
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

  it("shows when MTP became active in RAM", () => {
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

describe("PeerDetailCard · context reserve", () => {
  afterEach(cleanup);

  it("exposes when several sessions multiply the KV on the GPU", () => {
    mount(fakeSlicePeer("context", 0, 31), {
      loaded: true,
      health: {
        mtp_ready: true,
        max_sessions: 2,
        n_ctx_total: 500224,
        n_ctx_per_session: 250112,
        fit_memory_applied: true,
        context_memory: {
          buffers: [
            { buffer: "CUDA0", device: "CUDA0", model: 0, context: 2_000_000_000, compute: 0 },
          ],
        },
        placement: null,
      } as never,
    });

    expect(screen.getByRole("status").textContent).toMatch(
      /500\.224.*2 sessões.*250\.112.*KV.*GPU/i,
    );
  });

  it("creates no alert for a single-session configuration", () => {
    mount(fakeSlicePeer("context-one", 0, 31), {
      loaded: true,
      health: {
        mtp_ready: true,
        max_sessions: 1,
        n_ctx_total: 250112,
        n_ctx_per_session: 250112,
        context_memory: {
          buffers: [
            { buffer: "CUDA0", device: "CUDA0", model: 0, context: 1_000_000_000, compute: 0 },
          ],
        },
        placement: null,
      } as never,
    });

    expect(screen.queryByRole("status")).toBeNull();
  });
});

describe("PeerDetailCard · load parameters", () => {
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

  function mountWith(load_params: unknown) {
    mount(fakeSlicePeer("lp", 0, 31), {
      loaded: true,
      health: { mtp_ready: false, placement: null, load_params } as never,
    });
  }

  afterEach(cleanup);

  it("shows the batch and the microbatch the context started with", () => {
    mountWith(params);
    expect(screen.getByText("Parâmetros de carga do llama.cpp")).toBeTruthy();
    // O microlote é o que governa o prefill, e ele não é o n_batch: com o fit
    // ligado o llama.cpp o corta. Mostrar os dois é o ponto da seção.
    expect(screen.getByText("512 / 128")).toBeTruthy();
  });

  it("says Flash Attention came from the probe, not from the operator request", () => {
    mountWith(params);
    expect(screen.getByText("ligado (auto)")).toBeTruthy();
  });

  it("distinguishes requested Flash Attention from the automatically resolved one", () => {
    mountWith({ ...params, flash_attn: false, flash_attn_auto: false });
    expect(screen.getByText("desligado (pedido)")).toBeTruthy();
  });

  it("says it does not know when the diagnostic bridge did not answer", () => {
    // O caminho que não pode existir é o `null` virar "desligado": isso faria
    // o operador caçar um problema de Flash Attention que ninguém reportou.
    mountWith({ ...params, flash_attn: null, flash_attn_auto: null });
    expect(screen.getByText("não sei")).toBeTruthy();
  });

  /** O aviso do fit virou o `[i]` do próprio título da seção. */
  it("warns that the fit changed the requested values", () => {
    mountWith(params);
    fireEvent.click(
      screen.getByRole("button", { name: "O que é Parâmetros de carga do llama.cpp" }),
    );
    expect(screen.getByText(/O fit do llama.cpp ajustou este contexto/)).toBeTruthy();
  });

  it("omits the whole section on a node whose /health carries no parameters", () => {
    mountWith(undefined);
    expect(screen.queryByText("Parâmetros de carga do llama.cpp")).toBeNull();
  });
});

describe("PeerDetailCard without GPU (Linux node, e.g. in a Docker container)", () => {
  afterEach(cleanup);

  /// `wired_mb` só é populado no macOS (`sysmon.rs`); um nó Linux sem NVIDIA
  /// caía no mesmo ramo que mostrava "Wired memory (Metal buffers)" com
  /// `value={null}`, que trava o `Progress` no modo indeterminado para
  /// sempre — a barra "fica saltando e nunca se resolve", já que nada nesse
  /// nó jamais vai preencher `wired_mb`.
  const withoutGpu = fakePeer({
    node_id: "cpu-only",
    node: fakeNode({
      node_id: "cpu-only",
      short_id: "cpu-only",
      hostname: "node-cpu-1",
      system: fakeSystem({
        hostname: "node-cpu-1",
        os: "linux",
        gpu_kind: "none",
        wired_mb: null,
        // A stale device entry must not make a `none` machine render thermal UI.
        gpus: [{
          index: 0,
          name: "stale device",
          vendor: "nvidia",
          vram_total_mb: null,
          vram_used_mb: null,
          vram_used_pct: null,
          utilization_pct: null,
          power_watts: null,
          temperature_c: null,
        }],
      }),
    }),
  });

  it("does not show the Metal wired memory gauge", () => {
    mount(withoutGpu);
    expect(screen.queryByText(/wired/i)).toBeNull();
    expect(screen.queryByText(/metal/i)).toBeNull();
  });

  it("shows a bar in a defined state, not indeterminate forever", () => {
    mount(withoutGpu);
    const bars = screen.getAllByRole("progressbar");
    // Toda barra desta máquina tem que ter um valor definido — nenhuma pode
    // ficar sem `aria-valuenow`, que é como o indeterminado se denuncia.
    for (const bar of bars) {
      expect(bar.getAttribute("aria-valuenow")).not.toBeNull();
    }
  });

  it("omits the temperature section when the machine has no GPU", () => {
    mount(withoutGpu);
    expect(screen.queryByText(/temperatura da gpu/i)).toBeNull();
    expect(screen.queryByLabelText(/temperatura da gpu/i)).toBeNull();
  });
});

describe("GPU temperature", () => {
  afterEach(cleanup);

  it("shows a reading and the thermal band for each GPU", () => {
    const peer = fakeSlicePeer("thermal", 0, 15, {
      node: fakeNode({
        node_id: "thermal",
        short_id: "thermal",
        hostname: "node-thermal",
        system: fakeSystem({
          gpu_kind: "nvidia",
          gpus: [
            { index: 0, name: "RTX A", vendor: "nvidia", vram_total_mb: 1024, vram_used_mb: 512, vram_used_pct: 50, utilization_pct: 20, power_watts: null, temperature_c: 72 },
            { index: 1, name: "RTX B", vendor: "nvidia", vram_total_mb: 1024, vram_used_mb: 512, vram_used_pct: 50, utilization_pct: 20, power_watts: null, temperature_c: 90 },
          ],
        }),
      }),
    });
    mount(peer);
    expect(screen.getByText(/72,0 °C/)).toBeTruthy();
    expect(screen.getByText(/90,0 °C/)).toBeTruthy();
    expect(screen.getByText(/crítica/)).toBeTruthy();
  });

  it("keeps the warning when a GPU exists but the sensor is unavailable", () => {
    const peer = fakeSlicePeer("thermal-unknown", 0, 15, {
      node: fakeNode({
        node_id: "thermal-unknown",
        short_id: "thermal-unknown",
        hostname: "node-thermal-unknown",
        system: fakeSystem({
          gpu_kind: "nvidia",
          gpus: [
            { index: 0, name: "RTX A", vendor: "nvidia", vram_total_mb: 1024, vram_used_mb: 512, vram_used_pct: 50, utilization_pct: 20, power_watts: null, temperature_c: null },
          ],
        }),
      }),
    });
    mount(peer);
    expect(screen.getByText("sensor indisponível")).toBeTruthy();
    expect(screen.queryByText("temperatura desconhecida")).toBeNull();
  });
});

/**
 * O card é lido numa tela, e a máquina reporta muito. O que se afirma aqui é a
 * arrumação: o que se **faz** com a máquina fica no topo, e o que ela **tem**
 * aparece inteiro - todas as partições, não só a do projeto.
 */
describe("PeerDetailCard · card layout", () => {
  afterEach(cleanup);

  it("puts the actions at the top, before what the machine reports", () => {
    mount(fakeSlicePeer("a", 0, 15));
    const leaveButton = screen.getByRole("button", { name: /sair da rede/i });
    const ram = screen.getByText("RAM");
    // `DOCUMENT_POSITION_FOLLOWING` = a RAM vem depois do botão no documento.
    expect(leaveButton.compareDocumentPosition(ram) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  });

  /**
   * Uma máquina com NVMe e HDD, ou com `/` e `/dados` separados, aparecia como
   * um volume só - e quem decide onde cabe um GGUF de 40 GB não via o disco
   * onde ele caberia.
   */
  it("shows one bar per mounted partition", () => {
    const peer = fakeSlicePeer("discos", 0, 15, {
      node: fakeNode({
        node_id: "discos",
        short_id: "discos",
        hostname: "node-discos",
        system: fakeSystem({
          disk: { total_mb: 500000, free_mb: 200000, mount_point: "/", device: "/dev/nvme0n1p2" },
          disks: [
            { total_mb: 500000, free_mb: 200000, mount_point: "/", device: "/dev/nvme0n1p2" },
            { total_mb: 2000000, free_mb: 1500000, mount_point: "/dados", device: "/dev/sda1" },
          ],
        }),
      }),
    });
    mount(peer);
    expect(screen.getByText("Disco · /")).toBeTruthy();
    expect(screen.getByText("Disco · /dados")).toBeTruthy();
  });

  /** Um nó que ainda não publica a lista não perde o disco que já publicava. */
  it("falls back to the project volume when the node does not list the partitions", () => {
    const peer = fakeSlicePeer("antigo", 0, 15, {
      node: fakeNode({
        node_id: "antigo",
        short_id: "antigo",
        hostname: "node-antigo",
        system: fakeSystem({ disks: [] }),
      }),
    });
    mount(peer);
    expect(screen.getByText("Disco · /")).toBeTruthy();
  });

  it("keeps the load parameters in a block that opens", () => {
    mount(fakeSlicePeer("lp2", 0, 31), {
      loaded: true,
      health: {
        mtp_ready: false,
        placement: null,
        load_params: {
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
          fit_memory_applied: false,
        },
      } as never,
    });
    const title = screen.getByText("Parâmetros de carga do llama.cpp");
    expect(title.closest("details")).toBeTruthy();
    expect(title.closest("details")!.open).toBe(false);
  });

  it("shows GPUs, RAM, load and disks in that sequence", () => {
    const peer = fakeSlicePeer("telemetria", 0, 15, {
      node: fakeNode({
        node_id: "telemetria",
        short_id: "telemetria",
        hostname: "node-telemetria",
        system: fakeSystem({
          gpu_kind: "nvidia",
          gpus: [{
            index: 0,
            name: "RTX A",
            vendor: "nvidia",
            vram_total_mb: 8192,
            vram_used_mb: 2048,
            vram_used_pct: 25,
            utilization_pct: 30,
            temperature_c: 72,
            power_watts: 250,
          } as never],
        }),
      }),
    });
    mount(peer);

    const gpu = screen.getByTestId("gpu-telemetry-section");
    const ram = screen.getByText("RAM");
    const load = screen.getByText(/Carga ·/);
    const disk = screen.getByText("Disco · /");
    expect(gpu.compareDocumentPosition(ram) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(ram.compareDocumentPosition(load) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(load.compareDocumentPosition(disk) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(screen.getByTestId("gpu-telemetry-row").textContent).toMatch(/30%.*72,0 °C.*250 W/);
  });

  it("does not offer update and restart on the card", () => {
    mount(fakeSlicePeer("sem-update", 0, 15, {
      node: fakeNode({ dev_update: true }),
    }));
    expect(screen.queryByRole("button", { name: /atualizar e reiniciar/i })).toBeNull();
  });
});

/**
 * Um endereço que morre não pode virar um aviso permanente.
 *
 * Endereços IPv6 temporários rodam sozinhos (privacy extensions), então o
 * endereço que um par anunciou ontem pode não existir em máquina nenhuma hoje.
 * O nó esquece esse endereço, pergunta o atual e recupera o par sozinho - e é
 * isso que a tela tem de dizer enquanto acontece, em vez de "nada abaixo pode
 * ser lido", que manda o utilizador procurar um defeito que se conserta só.
 */
describe("PeerDetailCard with the address being rediscovered", () => {
  afterEach(cleanup);

  it("says the node is being rediscovered, not that nothing can be read", () => {
    mount(fakePeer({ node_id: "z", rediscovering: true }));
    expect(screen.getByText(/Reencontrando este nó/)).toBeTruthy();
    expect(screen.queryByText(/a leitura falhou/)).toBeNull();
  });

  it("keeps explaining the failure when the peer answered and the reading is what broke", () => {
    mount(fakePeer({ node_id: "z", unreachable: "HTTP 500" }));
    expect(screen.getByText(/a leitura falhou: HTTP 500/)).toBeTruthy();
    expect(screen.queryByText(/Reencontrando este nó/)).toBeNull();
  });

  /**
   * A cobertura sai do gossip, não da leitura: o par ilegível continua
   * contribuindo camadas. A tela tem de dizer que a cobertura não está
   * confirmada em vez de ficar calada.
   */
  it("warns that the coverage is not confirmed", () => {
    mount(fakePeer({ node_id: "z", rediscovering: true }));
    expect(screen.getByText(/continuam contando para a cobertura/)).toBeTruthy();
  });
});

describe("cardRange", () => {
  it("returns null when there is no range anywhere, instead of 0-0", () => {
    expect(cardRange(withoutSlice)).toBeNull();
  });

  it("prefers the peer announce", () => {
    expect(cardRange(fakeSlicePeer("a", 4, 9))).toEqual([4, 9]);
  });
});
