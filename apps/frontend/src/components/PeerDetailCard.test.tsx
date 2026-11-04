// @vitest-environment jsdom
/**
 * A machine without a slice does not become a machine with the slice "0-0".
 *
 * The card was written for whoever serves a range, and the Details tab now shows
 * also whoever is on the network without serving anything. What is asserted here is that in that
 * case it writes no range at all, says why, and does not offer the button that
 * takes out of the model a machine that is in no model.
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
   * The sentence left the strip that took up a whole line of the card and went into the
   * `[i]` of the badge itself. What is asserted remains the same: whoever
   * asks "why no slice?" gets an answer, without the card spending a
   * line on it for every idle machine on the network.
   */
  it("says the machine is on the network and serves no layer", () => {
    mount(withoutSlice);
    fireEvent.click(screen.getByRole("button", { name: "O que é sem fatia" }));
    expect(screen.getByText(/não serve nenhuma faixa de camadas/i)).toBeTruthy();
  });

  it("does not offer to remove from the model a machine that is in no model", () => {
    mount(withoutSlice);
    expect(screen.queryByRole("button", { name: /remover/i })).toBeNull();
    // Leaving the network is not offered from the card either.
    expect(screen.queryByRole("button", { name: /sair da rede/i })).toBeNull();
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
    // The microbatch is what governs prefill, and it is not n_batch: with fit
    // on, llama.cpp cuts it. Showing both is the point of the section.
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
    // The path that must not exist is `null` turning into "off": that would make
    // the operator hunt for a Flash Attention problem that nobody reported.
    mountWith({ ...params, flash_attn: null, flash_attn_auto: null });
    expect(screen.getByText("não sei")).toBeTruthy();
  });

  /** The fit warning became the `[i]` of the section's own title. */
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

  /// `wired_mb` is only populated on macOS (`sysmon.rs`); a Linux node without NVIDIA
  /// fell into the same branch that showed "Wired memory (Metal buffers)" with
  /// `value={null}`, which locks the `Progress` in indeterminate mode
  /// forever - the bar "keeps jumping and never resolves", since nothing on that
  /// node will ever fill `wired_mb`.
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
    // Every bar of this machine must have a defined value - none may
    // be left without `aria-valuenow`, which is how the indeterminate gives itself away.
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
 * The card is read on a screen, and the machine reports a lot. What is asserted here is the
 * arrangement: what you **do** with the machine stays at the top, and what it **has**
 * appears in full - all the partitions, not only the project's.
 */
describe("PeerDetailCard · card layout", () => {
  afterEach(cleanup);

  it("puts the actions at the top, before what the machine reports", () => {
    mount(fakeSlicePeer("a", 0, 15));
    const removeButton = screen.getByRole("button", { name: /remover/i });
    expect(screen.queryByRole("button", { name: /sair da rede/i })).toBeNull();
    const ram = screen.getByText("RAM");
    // `DOCUMENT_POSITION_FOLLOWING` = the RAM comes after the button in the document.
    expect(removeButton.compareDocumentPosition(ram) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  });

  /**
   * A machine with NVMe and HDD, or with `/` and `/data` separate, showed up as
   * a single volume - and whoever decides where a 40 GB GGUF fits did not see the disk
   * where it would fit.
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

  /** A node that does not publish the list yet does not lose the disk it already published. */
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
 * An address that dies must not become a permanent warning.
 *
 * Temporary IPv6 addresses rotate on their own (privacy extensions), so the
 * address that a peer announced yesterday may not exist on any machine today.
 * The node forgets that address, asks for the current one and recovers the peer on its own - and
 * that is what the screen must say while it happens, instead of "nothing below can
 * be read", which sends the user hunting for a defect that fixes itself.
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
   * The coverage comes from gossip, not from reading: the unreadable peer keeps
   * contributing layers. The screen must say that the coverage is not
   * confirmed instead of staying silent.
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

describe("a machine's system, as its node shares it", () => {
  afterEach(cleanup);

  const shared = fakePeer({
    node_id: "m",
    node: fakeNode({
      node_id: "m",
      short_id: "m",
      hostname: "membro",
      system: fakeSystem({
        disk: { total_mb: 1000, free_mb: 400, mount_point: "", device: "" },
        disks: [{ total_mb: 1000, free_mb: 400, mount_point: "", device: "" }],
        gpu_processes: [
          { pid: 7, name: "pom", used_mb: 2048 },
          { pid: 0, name: "", used_mb: 512 },
        ],
      }),
    }),
  });

  it("names the one volume as model storage, with no mount point", () => {
    mount(shared);
    expect(screen.getByText("Armazenamento de modelos")).toBeTruthy();
    expect(screen.queryByText(/Disco ·/)).toBeNull();
  });

  it("shows the other processes as one total, with no pid", () => {
    mount(shared);
    fireEvent.click(screen.getByText("Processos na GPU"));
    expect(screen.getByText("Outros processos")).toBeTruthy();
    expect(screen.getByText("#7")).toBeTruthy();
    expect(screen.queryByText("#0")).toBeNull();
  });
});
