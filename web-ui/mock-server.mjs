/**
 * Sample backend — a stand-in node for working on the interface.
 *
 * It answers the same paths a real `rdma-p2p` gateway does, with a three
 * machine cluster made up so the screens have something to show: a MacBook that
 * holds the first layers, an NVIDIA box in the middle, and a Linux server that
 * samples the tokens. Nothing here joins any network: it is a plain HTTP server
 * with no UDP, no discovery and no peers.
 *
 *   node mock-server.mjs                  # listens on 5198
 *   VITE_API_TARGET=http://127.0.0.1:5198 npm run dev
 */
import { createServer } from "node:http";

const PORT = Number(process.env.MOCK_PORT ?? 5198);
const started = Date.now();
const KEY = "3f7c1c0e9a5b4d2e8f6a0b1c2d3e4f50617283945a6b7c8d9e0f1a2b3c4d5e6f";

const mem = (total, usedPct) => ({
  total_mb: total,
  used_mb: Math.round(total * usedPct),
  available_mb: Math.round(total * (1 - usedPct)),
  used_pct: usedPct * 100,
});

/** A little drift so the meters are not frozen. */
const wobble = (base, spread = 0.03) =>
  Math.min(0.97, Math.max(0.05, base + Math.sin(Date.now() / 9000) * spread));

const machines = [
  {
    node_id: "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90",
    hostname: "macbook-captain",
    role: "Principal",
    gateway_port: 8080,
    inference_port: 8181,
    addr: "192.168.100.42:42777",
    start_layer: 0,
    end_layer: 11,
    os: "macos",
    arch: "aarch64",
    cpu_count: 12,
    ram: () => mem(32768, wobble(0.62)),
    gpu_kind: "unified",
    gpu_note:
      "Memória unificada: a GPU usa a mesma RAM, então não existe VRAM dedicada para reportar. O número abaixo é a memória wired, onde ficam os buffers do Metal.",
    gpus: [],
    gpu_processes: [],
    wired_mb: 9420,
    disk: { total_mb: 994_000, free_mb: 214_500, mount_point: "/" },
  },
  {
    node_id: "b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90a1",
    hostname: "ksia",
    role: "Nó",
    gateway_port: 8090,
    inference_port: 8181,
    addr: "192.168.100.138:42777",
    start_layer: 12,
    end_layer: 25,
    os: "linux",
    arch: "x86_64",
    cpu_count: 16,
    ram: () => mem(65536, wobble(0.41)),
    gpu_kind: "nvidia",
    gpu_note: null,
    gpus: [
      {
        index: 0,
        name: "NVIDIA GeForce RTX 5060 Ti",
        vram_total_mb: 16311,
        vram_used_mb: 7636,
        vram_used_pct: 46.8,
        utilization_pct: 71,
        temperature_c: 62,
      },
    ],
    gpu_processes: [{ pid: 48213, name: "python3", used_mb: 7580 }],
    wired_mb: null,
    disk: { total_mb: 1_907_000, free_mb: 803_200, mount_point: "/" },
  },
  {
    node_id: "c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2",
    hostname: "forno",
    role: "Nó",
    gateway_port: 8090,
    inference_port: 8181,
    addr: "192.168.100.77:42777",
    start_layer: 26,
    end_layer: 31,
    os: "linux",
    arch: "x86_64",
    cpu_count: 8,
    ram: () => mem(16384, wobble(0.55)),
    gpu_kind: "none",
    gpu_note: "Nenhuma GPU detectada nesta máquina: as camadas rodam na CPU.",
    gpus: [],
    gpu_processes: [],
    wired_mb: null,
    disk: { total_mb: 480_000, free_mb: 41_800, mount_point: "/" },
  },
];

const MODEL = "qwen35";
const TOTAL_LAYERS = 32;

function nodeInfo(m) {
  return {
    node_id: m.node_id,
    short_id: m.node_id.slice(0, 12),
    hostname: m.hostname,
    role: m.role,
    connection_key: KEY,
    p2p_port: 42777,
    gateway_port: m.gateway_port,
    inference_port: m.inference_port,
    config: {
      model_path: `/Users/captain/models/qwen/qwen_${m.start_layer}_${m.end_layer}.gguf`,
      model_label: MODEL,
      start_layer: m.start_layer,
      end_layer: m.end_layer,
      total_layers: TOTAL_LAYERS,
      peer_count: machines.length,
      peer_index: machines.indexOf(m),
      primary: m.role === "Principal",
      inference_port: m.inference_port,
      n_gpu_layers: 99,
      n_ctx: 32768,
      n_batch: 3072,
      max_sessions: 1,
      prefill_chunk_tokens: 512,
      max_new_tokens: 256,
      temperature: 0.7,
      top_p: 0.9,
      top_k: 40,
      repeat_penalty: 1.15,
      repeat_last_n: 64,
      no_thinking: true,
      single_peer: false,
      no_embeddings: m.end_layer + 1 >= TOTAL_LAYERS,
    },
    status: {
      state: "running",
      detail: `servindo camadas ${m.start_layer}-${m.end_layer}`,
      pid: 40000 + machines.indexOf(m),
      restarts: 0,
      log_path: `inference_server_${m.inference_port}.log`,
      reported_total_layers: TOTAL_LAYERS,
      changed_at: new Date(started).toISOString(),
    },
    system: {
      hostname: m.hostname,
      os: m.os,
      arch: m.arch,
      cpu_count: m.cpu_count,
      load_avg: [1.24, 1.08, 0.92],
      ram: m.ram(),
      gpu_kind: m.gpu_kind,
      gpu_note: m.gpu_note,
      gpus: m.gpus,
      gpu_processes: m.gpu_processes,
      wired_mb: m.wired_mb,
      disk: m.disk,
      sampled_at: new Date().toISOString(),
    },
    peer_count: machines.length,
    load: 0,
    active_model: MODEL,
  };
}

const shard = (m) => ({
  id: `shard-${m.start_layer}`,
  name: `qwen_${m.start_layer}_${m.end_layer}.gguf`,
  source: "folder",
  source_label: "Pasta",
  path: `/Users/captain/models/qwen/qwen_${m.start_layer}_${m.end_layer}.gguf`,
  size_bytes: 1_400_000_000 + m.start_layer * 40_000_000,
  architecture: "qwen2",
  total_layers: TOTAL_LAYERS,
  quant: "Q4_K_M",
  shard_range: [m.start_layer, m.end_layer],
  shard_group: "qwen",
});

const wholeModels = [
  {
    id: "lmstudio-qwen",
    name: "Qwen/Qwen3.5-9B-Instruct",
    source: "lmstudio",
    source_label: "LM Studio",
    path: "/Users/captain/.lmstudio/models/Qwen/Qwen3.5-9B-Instruct-Q4_K_M.gguf",
    size_bytes: 5_368_709_120,
    architecture: "qwen2",
    total_layers: 32,
    quant: "Q4_K_M",
    shard_range: null,
    shard_group: null,
  },
  {
    id: "ollama-llama3",
    name: "llama3:8b",
    source: "ollama",
    source_label: "Ollama",
    path: "/Users/captain/.ollama/models/blobs/sha256-6a0746a1ec1a",
    size_bytes: 4_661_211_808,
    architecture: "llama",
    total_layers: 32,
    quant: "Q4_0",
    shard_range: null,
    shard_group: null,
  },
];

const catalog = () => ({
  hostname: machines[0].hostname,
  node_id: machines[0].node_id,
  models: wholeModels,
  shard_sets: [
    {
      group: "qwen",
      total_layers: TOTAL_LAYERS,
      total_layers_inferred: false,
      shards: machines.map(shard),
      complete: true,
    },
  ],
  scanned: [
    { source: "lmstudio", path: "/Users/captain/.lmstudio/models", exists: true, found: 1 },
    { source: "ollama", path: "/Users/captain/.ollama/models", exists: true, found: 1 },
    { source: "folder", path: "/Users/captain/models", exists: true, found: 3 },
  ],
});

/** A slicing run that walks from 0 to 100 and then restarts, so the bar moves. */
function liveJob() {
  const elapsed = (Date.now() - started) / 1000;
  const pct = (elapsed * 2.4) % 100;
  return {
    id: "job-demo",
    state: "running",
    step: "gravando",
    kind: "slice",
    title: "Fatiando Qwen3.5-9B",
    message: `fatia 2/3 · camadas 12–25 de 32 · gravando`,
    progress: Number(pct.toFixed(1)),
    outputs: ["/Users/captain/models/qwen/qwen_0_11.gguf"],
    started_at: new Date(started).toISOString(),
    finished_at: null,
  };
}

function liveTransfer() {
  const elapsed = (Date.now() - started) / 1000;
  const total = 10_737_418_240;
  const done = Math.min(total, elapsed * 11_500_000);
  return {
    id: "tx-demo",
    name: "qwen_26_31.gguf",
    from_host: "ksia",
    state: "downloading",
    bytes_done: Math.round(done),
    bytes_total: total,
    bytes_per_sec: 11_500_000,
    eta_secs: Math.max(0, Math.round((total - done) / 11_500_000)),
    detail: "baixando com retomada por Range",
  };
}

const requestLog = () =>
  Array.from({ length: 24 }, (_, i) => {
    const at = new Date(Date.now() - i * 47_000).toISOString();
    const failed = i % 9 === 5;
    return {
      id: `req-${i}`,
      at,
      route: i % 4 === 0 ? "/api/generate" : "/v1/chat/completions",
      model: MODEL,
      token_label: i % 3 === 0 ? "opencode" : i % 3 === 1 ? "painel" : "chave de conexão",
      token_hint: i % 3 === 2 ? "…5e6f" : i % 3 === 0 ? "…a41c" : "…9d02",
      token_node_id: i % 3 === 0 ? machines[1].node_id : null,
      entry_node_id: machines[0].node_id,
      hops: machines.map((m) => m.node_id),
      source: `pipeline:${machines.map((m) => m.node_id.slice(0, 8)).join("→")}`,
      prompt_tokens: 35 + (i % 7) * 120,
      completion_tokens: failed ? null : 34 + (i % 5) * 18,
      duration_ms: failed ? 412 : 2237 + (i % 6) * 310,
      status: failed ? "error" : "ok",
      error: failed ? "prompt de 3.640 tokens excede o orçamento de 3.072" : null,
    };
  });

const routes = {
  "GET /api/ui/node": () => nodeInfo(machines[0]),
  "GET /api/ui/cluster": () => ({
    self_id: machines[0].node_id,
    active_model: MODEL,
    total_layers: TOTAL_LAYERS,
    covered_layers: TOTAL_LAYERS,
    coverage: Array(TOTAL_LAYERS).fill(true),
    complete: true,
    peers: machines.map((m) => ({
      node_id: m.node_id,
      short_id: m.node_id.slice(0, 12),
      addr: m.addr,
      model: MODEL,
      start_layer: m.start_layer,
      end_layer: m.end_layer,
      total_layers: TOTAL_LAYERS,
      inference_url: `http://${m.addr.split(":")[0]}:${m.inference_port}`,
      gateway_url: `http://${m.addr.split(":")[0]}:${m.gateway_port}`,
      load: 0,
      is_self: m === machines[0],
      node: nodeInfo(m),
      unreachable: null,
    })),
    node: nodeInfo(machines[0]),
  }),
  "GET /api/ui/models": catalog,
  "GET /api/ui/cluster/models": () => ({
    peers: machines.map((m, i) => ({
      node_id: m.node_id,
      hostname: m.hostname,
      is_self: i === 0,
      models: i === 0 ? wholeModels : [wholeModels[0]],
      shard_sets: catalog().shard_sets,
      scanned: catalog().scanned,
    })),
  }),
  "GET /api/ui/paths": () => ({
    model_dirs: [
      { path: "/Users/captain/.lmstudio/models", exists: true, kind: "lmstudio" },
      { path: "/Users/captain/.ollama/models", exists: true, kind: "ollama" },
      { path: "/Volumes/sdcard/models", exists: false, kind: "folder" },
    ],
    shard_dir: { path: "/Users/captain/models", exists: true, kind: "folder" },
    using_defaults: false,
    detected: [
      { path: "/Users/captain/.lmstudio/models", exists: true, kind: "lmstudio" },
      { path: "/Users/captain/.ollama/models", exists: true, kind: "ollama" },
    ],
    default_shard_dir: "/Users/captain/rdma-p2p/models",
  }),
  "GET /api/ui/jobs": () => ({ jobs: [liveJob()] }),
  "GET /api/transfer/jobs": () => ({ jobs: [liveTransfer()] }),
  "GET /api/transfer/offers": () => ({
    offers: [
      {
        id: "offer-1",
        from_node_id: machines[1].node_id,
        from_host: "ksia",
        from_gateway: "http://192.168.100.138:8090",
        remote_path: "/home/captain/models/qwen/qwen_26_31.gguf",
        grant: "grant-1",
        name: "qwen_26_31.gguf",
        size_bytes: 1_610_612_736,
        md5: "d41d8cd98f00b204e9800998ecf8427e",
        start_layer: 26,
        end_layer: 31,
      },
    ],
  }),
  "GET /admin/nodes": () =>
    machines.map((m, i) => ({
      node_id: m.node_id,
      short_id: m.node_id.slice(0, 12),
      hostname: m.hostname,
      addr: m.addr,
      role: m.role,
      model: MODEL,
      start_layer: m.start_layer,
      end_layer: m.end_layer,
      total_layers: TOTAL_LAYERS,
      is_self: i === 0,
      connected: true,
      blocked: false,
      verified: true,
    })),
  "GET /admin/api-keys": () => [
    {
      key: "sk-4f2c9a70-1b3d-4e58-9c07-2a6f8b1d4e30",
      label: "opencode",
      node_id: machines[1].node_id,
      active: true,
      created_at: new Date(Date.now() - 86_400_000 * 3).toISOString(),
    },
    {
      key: "sk-9d81b402-77aa-4c19-bb6e-51e0f3a2c8d1",
      label: "painel",
      node_id: "",
      active: true,
      created_at: new Date(Date.now() - 86_400_000 * 11).toISOString(),
    },
  ],
  "GET /api/ui/requests": () => {
    const entries = requestLog();
    return { entries, total: entries.length };
  },
  "GET /v1/models": () => ({
    object: "list",
    data: [{ id: MODEL, object: "model", created: 0, owned_by: "rdma-p2p" }],
  }),
  "GET /health": () => ({ status: "ok", node_id: machines[0].node_id }),
};

/** A slow, believable stream so the chat can be worked on offline. */
function streamChat(res) {
  res.writeHead(200, {
    "Content-Type": "text/event-stream",
    "Cache-Control": "no-cache",
    Connection: "keep-alive",
  });
  const words = `Esta resposta vem do **backend de exemplo**, não de um modelo.

O pipeline real encadeia as máquinas por camada:

\`\`\`bash
curl -s localhost:8080/v1/chat/completions \\
  -H "Authorization: Bearer $CHAVE" \\
  -d '{"model":"qwen35","messages":[{"role":"user","content":"oi"}],"stream":true}'
\`\`\`

| máquina | camadas | papel |
|---|---|---|
| macbook-captain | 0–11 | processa o prompt |
| ksia | 12–25 | intermediária |
| forno | 26–31 | gera os tokens |

Cada salto carrega o estado oculto em f16.`.split(/(\s+)/);

  let i = 0;
  const timer = setInterval(() => {
    if (i >= words.length) {
      clearInterval(timer);
      res.write("data: [DONE]\n\n");
      res.end();
      return;
    }
    const chunk = {
      id: "chatcmpl-mock",
      object: "chat.completion.chunk",
      created: Math.floor(Date.now() / 1000),
      model: MODEL,
      choices: [{ index: 0, delta: { content: words[i] }, finish_reason: null }],
    };
    res.write(`data: ${JSON.stringify(chunk)}\n\n`);
    i++;
  }, 22);
  res.on("close", () => clearInterval(timer));
}

createServer((req, res) => {
  const url = new URL(req.url, "http://localhost");
  const key = `${req.method} ${url.pathname}`;
  res.setHeader("Access-Control-Allow-Origin", "*");

  if (key === "POST /v1/chat/completions") {
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", () => {
      const parsed = JSON.parse(body || "{}");
      if (parsed.stream) return streamChat(res);
      res.writeHead(200, { "Content-Type": "application/json" });
      res.end(
        JSON.stringify({
          id: "chatcmpl-mock",
          object: "chat.completion",
          model: MODEL,
          choices: [
            {
              index: 0,
              message: { role: "assistant", content: "Resposta de exemplo." },
              finish_reason: "stop",
            },
          ],
          x_rdma_p2p: { source: "pipeline:mock" },
        }),
      );
    });
    return;
  }

  const handler = routes[key];
  if (!handler) {
    // Mutations are answered, never performed: this server has nothing to change.
    if (req.method === "POST" || req.method === "DELETE") {
      res.writeHead(200, { "Content-Type": "application/json" });
      res.end(JSON.stringify({ ok: true, mock: true }));
      return;
    }
    res.writeHead(404, { "Content-Type": "application/json" });
    res.end(JSON.stringify({ error: `sem rota de exemplo para ${key}` }));
    return;
  }

  res.writeHead(200, { "Content-Type": "application/json" });
  res.end(JSON.stringify(handler()));
}).listen(PORT, () => {
  console.log(`backend de exemplo em http://127.0.0.1:${PORT}`);
  console.log(`use com: VITE_API_TARGET=http://127.0.0.1:${PORT} npm run dev`);
});
