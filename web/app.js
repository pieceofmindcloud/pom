/* rdma-p2p — painel de operação.
   Sem framework e sem build: o binário serve este arquivo direto.

   Regra de cor: cada peer recebe um índice estável (ordenado por camada
   inicial) e esse índice vira a hue usada na fita, no cartão e nos medidores.
   A cor sempre quer dizer "esta máquina", nunca outra coisa. */

const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];

const PEER_HUES = 5;
/** Banda medida entre as máquinas do captain (ksia -> MacBook), por dois
 *  métodos independentes. Usada para estimar tempo antes de o usuário decidir. */
const MEDIDA_BYTES_POR_S = 10.24 * 1024 * 1024;
const POLL_MS = 2000;
/** Quantas amostras de carga a linha de tendência guarda. */
const SPARK_POINTS = 40;

const state = {
  role: 'node',
  cluster: null,
  clusterModels: null,
  /** node_id -> [{ram, vram}] */
  history: new Map(),
  /** node_id -> índice de cor */
  hue: new Map(),
  selection: null,
  plan: null,
  streaming: false,
};

// ── Utilidades ───────────────────────────────────────────────────────────────

const hueVar = (i) => `var(--peer-${i % PEER_HUES})`;

function hueFor(nodeId) {
  if (!state.hue.has(nodeId)) state.hue.set(nodeId, state.hue.size);
  return hueVar(state.hue.get(nodeId));
}

function fmtBytes(n) {
  if (!n && n !== 0) return "—";
  const gb = n / 1073741824;
  if (gb >= 1) return `${gb.toFixed(gb >= 10 ? 0 : 1)} GB`;
  return `${(n / 1048576).toFixed(0)} MB`;
}

function fmtMB(mb) {
  if (mb == null) return "—";
  return mb >= 1024 ? `${(mb / 1024).toFixed(1)} GB` : `${Math.round(mb)} MB`;
}

const esc = (s) =>
  String(s ?? "").replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));

async function getJSON(url) {
  const r = await fetch(url, { headers: { accept: "application/json" } });
  if (!r.ok) throw new Error(`${r.status} ${r.statusText}`);
  return r.json();
}

/** A chave de conexão deste nó, buscada uma vez e reusada. */
async function ensureConnectionKey() {
  if (state.connectionKey) return state.connectionKey;
  try {
    const node = await getJSON("/api/ui/node");
    state.connectionKey = node.connection_key || "";
  } catch (e) {
    state.connectionKey = "";
  }
  return state.connectionKey;
}

async function postJSON(url, body) {
  // /api/ui/plan, /apply e /launch exigem a chave de conexão. Sem este header
  // toda a aba Configurar respondia 401, inclusive no próprio nó principal.
  await ensureConnectionKey();
  const r = await fetch(url, {
    method: "POST",
    headers: { "content-type": "application/json", ...authHeaders() },
    body: JSON.stringify(body),
  });
  const data = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(data.error || `${r.status} ${r.statusText}`);
  return data;
}

// ── Abas ─────────────────────────────────────────────────────────────────────

$$(".tab").forEach((tab) => {
  tab.addEventListener("click", () => {
    $$(".tab").forEach((t) => {
      const on = t === tab;
      t.classList.toggle("is-active", on);
      t.setAttribute("aria-selected", String(on));
    });
    $$(".view").forEach((v) =>
      v.classList.toggle("is-active", v.id === `view-${tab.dataset.view}`));
    if (tab.dataset.view === "configurar") loadClusterModels();
    if (tab.dataset.view === "nos") loadNodesTab();
    if (tab.dataset.view === "modelos") loadModelsTab();
  });
});

// ── Painel ───────────────────────────────────────────────────────────────────

async function pollCluster() {
  try {
    const cluster = await getJSON("/api/ui/cluster");
    state.cluster = cluster;
    setConn(true, `${cluster.peers.length} nó${cluster.peers.length === 1 ? "" : "s"}`);
    renderTop(cluster);
    renderRibbon($("#ribbon"), $("#ribbon-axis"), $("#ribbon-legend"), cluster);
    renderCoverage(cluster);
    renderPeers(cluster);
    renderConnectInfo(cluster);
    // Update role badge and connection key
    const roleBadge = document.getElementById('role-badge');
    if (roleBadge && cluster.node) {
      const role = cluster.node.role || 'node';
      roleBadge.textContent = role.charAt(0).toUpperCase() + role.slice(1);
      roleBadge.dataset.role = role.toLowerCase();
      state.role = role.toLowerCase();
    }
    if (cluster.node && cluster.node.connection_key) {
      state.connectionKey = cluster.node.connection_key;
    }
  } catch (e) {
    setConn(false, "sem resposta");
  } finally {
    setTimeout(pollCluster, POLL_MS);
  }
}

function setConn(live, text) {
  const el = $("#conn");
  el.classList.toggle("is-live", live);
  el.classList.toggle("is-down", !live);
  $("#conn-text").textContent = text;
}

function renderTop(cluster) {
  const me = cluster.peers.find((p) => p.is_self);
  $("#stat-node").textContent =
    me?.node?.hostname ? shortHost(me.node.hostname) : me?.short_id || "—";
  $("#stat-peers").textContent = String(cluster.peers.length);
  $("#stat-model").textContent = cluster.active_model || "—";
}

/** Ordena por camada inicial e fixa a hue de cada peer. */
function orderedPeers(cluster) {
  const peers = [...cluster.peers].sort((a, b) => a.start_layer - b.start_layer);
  peers.forEach((p) => hueFor(p.node_id));
  return peers;
}

/** Desenha a fita de camadas: uma célula por camada, colorida pelo dono. */
function renderRibbon(ribbonEl, axisEl, legendEl, cluster) {
  const total = cluster.total_layers || 0;
  if (!total) {
    ribbonEl.innerHTML = "";
    axisEl.textContent = "";
    legendEl.innerHTML = "";
    return;
  }

  const owners = new Array(total).fill(null);
  const peers = orderedPeers(cluster).filter((p) => p.model === cluster.active_model);
  for (const p of peers) {
    for (let l = p.start_layer; l <= Math.min(p.end_layer, total - 1); l++) {
      if (owners[l] === null) owners[l] = p;
    }
  }

  ribbonEl.innerHTML = owners
    .map((owner, i) => {
      if (!owner) return `<span class="cell is-gap" style="--i:${i}" title="camada ${i} — sem dono"></span>`;
      const label = `${owner.node?.hostname || owner.short_id} · camada ${i}`;
      return `<span class="cell" style="--peer-c:${hueFor(owner.node_id)};--i:${i}" title="${esc(label)}"></span>`;
    })
    .join("");

  axisEl.innerHTML = `<span>0</span><span>${total - 1}</span>`;

  legendEl.innerHTML = peers
    .map((p) => `
      <span class="legend-item">
        <span class="legend-swatch" style="--peer-c:${hueFor(p.node_id)}"></span>
        <span>${esc(p.node?.hostname ? shortHost(p.node.hostname) : p.short_id)}</span>
        <span class="range">${p.start_layer}–${p.end_layer}</span>
      </span>`)
    .join("");
}

function renderCoverage(cluster) {
  const meta = $("#coverage-meta");
  const warn = $("#coverage-warn");
  const total = cluster.total_layers || 0;

  if (!total) {
    meta.textContent = "nenhum peer anunciou um modelo ainda";
    warn.hidden = true;
    return;
  }

  meta.textContent = `${cluster.covered_layers} de ${total} camadas · ${esc(cluster.active_model)}`;

  if (cluster.complete) {
    warn.hidden = true;
  } else {
    const faltam = total - cluster.covered_layers;
    warn.hidden = false;
    warn.className = "notice notice-warn";
    warn.textContent =
      `${faltam} camada${faltam === 1 ? "" : "s"} sem dono — o planejador não fecha o pipeline ` +
      `e a inferência distribuída não roda. Ajuste os orçamentos em Configurar ou suba outro peer.`;
  }
}

function renderPeers(cluster) {
  const grid = $("#peer-grid");
  const peers = orderedPeers(cluster);

  if (!peers.length) {
    grid.innerHTML = `<p class="empty">Procurando peers na rede…</p>`;
    return;
  }

  grid.innerHTML = peers.map((p) => peerCard(p, cluster)).join("");
  peers.forEach((p) => drawSpark(p));
}

function peerCard(peer, cluster) {
  const hue = hueFor(peer.node_id);
  const node = peer.node;
  const sys = node?.system;
  const cfg = node?.config;
  const status = node?.status;

  if (!node) {
    return `
      <article class="peer-card" style="--peer-c:${hue}">
        <h3>${esc(peer.short_id)}</h3>
        <p class="peer-sub">${esc(peer.addr)}</p>
        <p class="peer-error">Sem resposta do painel deste peer${
          peer.unreachable ? ` — ${esc(peer.unreachable)}` : ""}.</p>
        <p class="meter-note">Camadas anunciadas: ${peer.start_layer}–${peer.end_layer}</p>
      </article>`;
  }

  const tags = [];
  if (peer.is_self) tags.push(`<span class="tag is-primary">este nó</span>`);
  if (cfg?.primary) tags.push(`<span class="tag is-primary">principal</span>`);
  if (status) {
    const cls = { ready: "is-ready", starting: "is-starting", failed: "is-failed" }[status.state] || "";
    tags.push(`<span class="tag ${cls}">${esc(status.state)}</span>`);
  }
  if (peer.load > 0) tags.push(`<span class="tag">${peer.load} em voo</span>`);

  const ram = sys?.ram;
  const gpu = sys?.gpus?.[0];

  return `
    <article class="peer-card" style="--peer-c:${hue}">
      <h3>${esc(sys?.hostname ? shortHost(sys.hostname) : peer.short_id)}</h3>
      <p class="peer-sub">${esc(peer.addr)} · ${esc(sys?.os || "?")}/${esc(sys?.arch || "?")} · ${
        peer.total_layers > 0
          ? `camadas ${peer.start_layer}–${peer.end_layer}`
          : "sem camadas atribuídas"
      }</p>
      <div class="tagrow">${tags.join("")}</div>

      ${meter("RAM", ram?.used_mb, ram?.total_mb, ram?.used_pct)}

      ${gpuSection(sys, gpu)}

      ${diskSection(sys?.disk)}

      <canvas class="spark" id="spark-${esc(peer.node_id)}" data-node="${esc(peer.node_id)}"></canvas>
      <p class="spark-key">${sys?.gpu_kind === "nvidia"
        ? "— RAM · - - VRAM · últimos 80 s"
        : "— RAM · últimos 80 s"}</p>

      ${status && status.state !== "ready"
        ? `<p class="meter-note">${esc(status.detail)}</p>` : ""}
      ${cfg ? `<p class="meter-note">${esc(cfg.model_path.split("/").pop())} · ctx ${cfg.n_ctx} · gpu ${cfg.n_gpu_layers}</p>` : ""}
    </article>`;
}

function meter(label, usedMb, totalMb, pct) {
  if (usedMb == null || totalMb == null) return "";
  const p = Math.max(0, Math.min(100, pct ?? 0));
  return `
    <div class="meter">
      <div class="meter-head">
        <span class="label">${esc(label)}</span>
        <span class="value">${fmtMB(usedMb)} / ${fmtMB(totalMb)} · ${p.toFixed(0)}%</span>
      </div>
      <div class="meter-track">
        <div class="meter-fill ${p > 90 ? "is-high" : ""}" style="width:${p}%"></div>
      </div>
    </div>`;
}

/** Espaço em disco onde ficam as fatias — é o que decide se cabe baixar outra. */
function diskSection(disk) {
  if (!disk || !disk.total_mb) return "";
  const used = disk.total_mb - disk.free_mb;
  const pct = (used / disk.total_mb) * 100;
  return `
    ${meter("Disco", used, disk.total_mb, pct)}
    <p class="meter-note">${fmtMB(disk.free_mb)} livres em ${esc(disk.mount_point || "—")}</p>`;
}

/** VRAM só é desenhada como medidor quando existe VRAM dedicada de verdade. */
function gpuSection(sys, gpu) {
  if (!gpu) return `<p class="meter-note">Nenhuma GPU detectada.</p>`;

  if (sys.gpu_kind === "nvidia" && gpu.vram_total_mb != null) {
    const util = gpu.utilization_pct != null ? ` · uso ${gpu.utilization_pct.toFixed(0)}%` : "";
    const temp = gpu.temperature_c != null ? ` · ${gpu.temperature_c.toFixed(0)}°C` : "";
    const procs = (sys.gpu_processes || [])
      .map((p) => `${esc(p.name)} ${fmtMB(p.used_mb)}`)
      .join(" · ");
    return `
      ${meter(`VRAM · ${gpu.name}`, gpu.vram_used_mb, gpu.vram_total_mb, gpu.vram_used_pct)}
      <p class="meter-note">${esc(gpu.name)}${util}${temp}${procs ? `<br>${procs}` : ""}</p>`;
  }

  // Memória unificada: não há um segundo pool para medir, então dizemos isso.
  const wired = sys.wired_mb != null ? `Wired ${fmtMB(sys.wired_mb)}. ` : "";
  return `
    <div class="meter">
      <div class="meter-head">
        <span class="label">VRAM</span>
        <span class="value">memória unificada</span>
      </div>
    </div>
    <p class="meter-note">${wired}${esc(sys.gpu_note || `${gpu.name}: a GPU divide a RAM acima.`)}</p>`;
}

/** Linha de tendência de RAM e VRAM, desenhada no canvas do cartão. */
function drawSpark(peer) {
  const canvas = document.getElementById(`spark-${peer.node_id}`);
  if (!canvas) return;

  const sys = peer.node?.system;
  if (!sys) return;

  const gpu = sys.gpus?.[0];
  const series = state.history.get(peer.node_id) || [];
  series.push({
    ram: sys.ram?.used_pct ?? 0,
    vram: sys.gpu_kind === "nvidia" ? gpu?.vram_used_pct ?? null : null,
  });
  while (series.length > SPARK_POINTS) series.shift();
  state.history.set(peer.node_id, series);

  const dpr = window.devicePixelRatio || 1;
  const w = canvas.clientWidth || 260;
  const h = canvas.clientHeight || 26;
  canvas.width = w * dpr;
  canvas.height = h * dpr;

  const ctx = canvas.getContext("2d");
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, w, h);

  const css = getComputedStyle(canvas);
  const hue = css.getPropertyValue("--peer-c").trim() ||
    getComputedStyle(canvas.closest(".peer-card")).getPropertyValue("--peer-c").trim();
  const rule = getComputedStyle(document.body).getPropertyValue("--rule").trim();

  // Régua de 50% — dá escala à linha sem precisar de eixo.
  ctx.strokeStyle = rule;
  ctx.lineWidth = 1;
  ctx.beginPath();
  ctx.moveTo(0, h / 2 + 0.5);
  ctx.lineTo(w, h / 2 + 0.5);
  ctx.stroke();

  const line = (key, color, dash) => {
    const pts = series.map((s, i) => [i, s[key]]).filter(([, v]) => v != null);
    if (pts.length < 2) return;
    ctx.strokeStyle = color;
    ctx.lineWidth = 1.5;
    ctx.setLineDash(dash);
    ctx.beginPath();
    pts.forEach(([i, v], k) => {
      const x = (i / Math.max(1, SPARK_POINTS - 1)) * w;
      const y = h - (v / 100) * h;
      k === 0 ? ctx.moveTo(x, y) : ctx.lineTo(x, y);
    });
    ctx.stroke();
    ctx.setLineDash([]);
  };

  line("ram", hue || "#888", []);
  line("vram", hue || "#888", [3, 3]);
}

/** A janela que o usuário vai realmente ter com os ajustes atuais.
 *
 *  Mesma conta de `gateway::prompt_budget`: o cache é dividido entre as
 *  sessões, então errar isso e só descobrir na hora da requisição foi
 *  exatamente o que irritou o captain.
 */
function usableWindow() {
  const nCtx = Number($("#p-ctx")?.value || 0);
  const nBatch = Number($("#p-batch")?.value || 0);
  const maxNew = Number($("#p-maxnew")?.value || 0);
  const sessions = Math.max(1, Number($("#p-sessions")?.value || 1));
  const perSession = Math.floor(nCtx / sessions);
  // Sem `nBatch` de propósito: com o prompt em pedaços o lote limita o pedaço,
  // não o prompt. Ver `gateway::prompt_budget`.
  const prompt = Math.max(0, perSession - maxNew);
  return { nCtx, nBatch, maxNew, sessions, perSession, prompt };
}

/** Bytes por token de prompt que atravessam cada hop, com a precisão de fio
 *  em uso. hidden_dim 4096 × 2 bytes (f16) = 8 KiB. */
const BYTES_POR_TOKEN_POR_HOP = 4096 * 2;

function renderWindowNote() {
  const el = $("#window-note");
  if (!el) return;
  const w = usableWindow();
  // Com o prompt em pedaços a janela deixa de ser limitada pelo lote: o lote
  // passa a valer por pedaço, não pelo prompt inteiro.
  const janela = Math.max(0, w.perSession - w.maxNew);
  const hops = Math.max(1, (state.plan?.plan?.shards?.length || state.cluster?.peers?.length || 2) - 1);
  const segundos = (janela * BYTES_POR_TOKEN_POR_HOP * hops) / MEDIDA_BYTES_POR_S;

  el.className = janela > 0 ? "notice notice-ok" : "notice notice-error";
  el.innerHTML = janela > 0
    ? `Cada requisição aceita até <b>${janela.toLocaleString("pt-BR")} tokens</b> de prompt `
      + `(${w.perSession.toLocaleString("pt-BR")} de janela por sessão, menos ${w.maxNew} de resposta). `
      + `O prompt entra em pedaços de ${Number($("#p-chunk")?.value || 512)} tokens, então o lote `
      + `(n_batch ${w.nBatch.toLocaleString("pt-BR")}) limita o pedaço, não o prompt.<br>`
      + `Um prompt desse tamanho leva <b>~${fmtDuration(segundos)}</b> até a primeira palavra, `
      + `porque ${fmtBytes(janela * BYTES_POR_TOKEN_POR_HOP * hops)} de estado oculto atravessam `
      + `${hops} hop(s) a ${fmtBytes(MEDIDA_BYTES_POR_S)}/s medidos. Cada peer precisa de memória para esse cache.`
    : `Estes ajustes não deixam espaço para prompt nenhum: a janela por sessão `
      + `(${w.perSession.toLocaleString("pt-BR")}) não cobre os ${w.maxNew} tokens de resposta.`;
}

["#p-ctx", "#p-batch", "#p-maxnew", "#p-sessions", "#p-chunk"].forEach((sel) =>
  document.querySelector(sel)?.addEventListener("input", renderWindowNote));
renderWindowNote();

function renderConnectInfo(cluster) {
  $("#base-url").textContent = `${location.origin}/v1`;
  $("#base-model").textContent = cluster.active_model || "—";
  // As rotas /v1 pedem chave: a de conexão vale, ou uma chave de API criada
  // na aba Nós. Dizer "não é verificada" deixou de ser verdade.
  const keyEl = $("#connect-api-key");
  if (keyEl) {
    keyEl.textContent = state.connectionKey
      ? "a chave de conexão, ou uma chave de API criada na aba Nós"
      : "gere uma chave na aba Nós";
  }
  $("#chat-hint").textContent = cluster.complete
    ? `pipeline pronto · ${cluster.total_layers} camadas`
    : "cobertura incompleta — a resposta deve falhar";
}

// ── Configurar ───────────────────────────────────────────────────────────────

$$('input[name="mode"]').forEach((r) =>
  r.addEventListener("change", () => {
    const full = r.value === "full" && r.checked;
    $("#mode-full").hidden = !full;
    $("#mode-shards").hidden = full;
    state.selection = null;
    state.plan = null;
    $("#plan-panel").hidden = true;
    renderBudgets();
  }));

async function loadClusterModels() {
  try {
    state.clusterModels = await getJSON("/api/ui/cluster/models");
    renderModelTable();
    renderShardTable();
    // A aba Modelos lê o mesmo catálogo; uma busca só serve as duas.
    renderWholeModels();
    renderLocalShards();
  } catch (e) {
    $("#model-table tbody").innerHTML =
      `<tr><td colspan="7" class="empty">Falha ao ler as bibliotecas: ${esc(e.message)}</td></tr>`;
    $("#whole-table tbody").innerHTML =
      `<tr><td colspan="7" class="empty">Falha ao ler as bibliotecas: ${esc(e.message)}</td></tr>`;
    $("#local-shards").innerHTML =
      `<p class="notice notice-error">Falha ao ler as bibliotecas: ${esc(e.message)}</p>`;
  }
}

/** Nome do peer, priorizando o hostname que ele mesmo reportou. */
function peerName(nodeId) {
  const entry = state.clusterModels?.peers.find((p) => p.node_id === nodeId);
  const raw = entry?.hostname
    || state.cluster?.peers.find((p) => p.node_id === nodeId)?.node?.hostname;
  if (raw) return shortHost(raw);
  const inCluster = state.cluster?.peers.find((p) => p.node_id === nodeId);
  return inCluster?.short_id || nodeId.slice(0, 8);
}

/** `Jairs-MacBook-Pro.local` → `Jairs-MacBook-Pro`: o sufixo mDNS só ocupa espaço. */
const shortHost = (h) => String(h).replace(/\.local$/i, "");

/** Nome do peer, com o começo do node_id quando dois peers da mesma máquina
 *  compartilham o hostname — aí o nome sozinho não diz qual é qual. */
function uniqueName(nodeId, peers) {
  const nome = peerName(nodeId);
  const repetido = (peers || []).filter((p) => peerName(p.node_id) === nome).length > 1;
  return repetido ? `${nome} · ${String(nodeId).slice(0, 8)}` : nome;
}

/**
 * Linha de apoio do modelo. Para o Ollama o caminho é um blob endereçado por
 * digest, que não diz nada a quem lê — nesses casos mostramos a arquitetura.
 */
function modelSubtitle(m) {
  const file = m.path.split("/").pop();
  const bits = [];
  if (m.architecture) bits.push(m.architecture);
  if (!file.startsWith("sha256-") && file !== m.name) bits.push(file);
  return bits.join(" · ");
}

/** Une as bibliotecas dos peers por nome, guardando o caminho local de cada um. */
function unionModels(predicate) {
  const union = new Map();
  for (const peer of state.clusterModels?.peers || []) {
    for (const m of peer.models || []) {
      if (!predicate(m)) continue;
      if (!union.has(m.name)) union.set(m.name, { name: m.name, sample: m, byPeer: new Map() });
      union.get(m.name).byPeer.set(peer.node_id, m);
    }
  }
  return [...union.values()].sort((a, b) => a.name.localeCompare(b.name));
}

function availabilityChips(byPeer) {
  const all = state.clusterModels?.peers || [];
  return `<span class="avail">${all
    .map((p) => {
      const m = byPeer.get(p.node_id);
      const name = esc(uniqueName(p.node_id, all));
      return m
        ? `<span class="chip has" title="${esc(m.path)}">${name}${m.quant ? ` ${esc(m.quant)}` : ""}</span>`
        : `<span class="chip hasnt" title="não instalado neste peer">${name}</span>`;
    })
    .join("")}</span>`;
}

function renderModelTable() {
  const rows = unionModels((m) => !m.shard_range);
  const tbody = $("#model-table tbody");

  if (!rows.length) {
    tbody.innerHTML = `<tr><td colspan="7" class="empty">Nenhum modelo inteiro encontrado. Veja os diretórios varridos em <code>/api/ui/models</code>.</td></tr>`;
    return;
  }

  tbody.innerHTML = rows
    .map((row, i) => {
      const m = row.sample;
      const missing = (state.clusterModels?.peers || []).some((p) => !row.byPeer.has(p.node_id));
      return `
      <tr class="${missing ? "is-unavailable" : ""}" data-kind="full" data-index="${i}">
        <td><input type="radio" name="model-pick" value="${i}" aria-label="${esc(row.name)}"></td>
        <td class="name">${esc(row.name)}
          <span class="sub">${esc(modelSubtitle(m))}</span></td>
        <td>${esc(m.source_label)}</td>
        <td>${esc(m.quant || "—")}</td>
        <td class="num">${m.total_layers ?? "—"}</td>
        <td class="num">${fmtBytes(m.size_bytes)}</td>
        <td>${availabilityChips(row.byPeer)}</td>
      </tr>`;
    })
    .join("");

  tbody.querySelectorAll('input[name="model-pick"]').forEach((input) =>
    input.addEventListener("change", () => {
      const row = rows[Number(input.value)];
      state.selection = { kind: "full", row };
      $$("#model-table tbody tr").forEach((tr) =>
        tr.classList.toggle("is-picked", tr.contains(input)));
      renderBudgets();
    }));
}

function renderShardTable() {
  const shards = unionModels((m) => !!m.shard_range);
  const groups = new Map();
  for (const row of shards) {
    const g = row.sample.shard_group || row.name;
    if (!groups.has(g)) groups.set(g, []);
    groups.get(g).push(row);
  }

  const tbody = $("#shard-table tbody");
  const list = [...groups.entries()];
  if (!list.length) {
    tbody.innerHTML = `<tr><td colspan="5" class="empty">Nenhum conjunto fatiado em <code>models/</code>.</td></tr>`;
    return;
  }

  list.forEach(([, rows]) =>
    rows.sort((a, b) => a.sample.shard_range[0] - b.sample.shard_range[0]));

  tbody.innerHTML = list
    .map(([group, rows], i) => {
      const declared = rows.find((r) => r.sample.total_layers)?.sample.total_layers;
      const total = shardSetTotal(rows);
      const chips = `<span class="avail">${(state.clusterModels?.peers || [])
        .map((p) => {
          const n = rows.filter((r) => r.byPeer.has(p.node_id)).length;
          return `<span class="chip ${n ? "has" : "hasnt"}">${esc(peerName(p.node_id))} ${n}/${rows.length}</span>`;
        })
        .join("")}</span>`;
      return `
      <tr data-kind="shards" data-index="${i}">
        <td><input type="radio" name="model-pick" value="s${i}" aria-label="${esc(group)}"></td>
        <td class="name">${esc(group)}</td>
        <td class="num">${rows.map((r) => `${r.sample.shard_range[0]}–${r.sample.shard_range[1]}`).join(" · ")}</td>
        <td class="num" ${declared ? "" : 'title="deduzido da cobertura: fatias antigas não gravam rdmap2p.original_block_count"'}>${
          total ?? "—"}${declared ? "" : "*"}</td>
        <td>${chips}</td>
      </tr>`;
    })
    .join("");

  tbody.querySelectorAll('input[name="model-pick"]').forEach((input) =>
    input.addEventListener("change", () => {
      const idx = Number(input.value.slice(1));
      const [group, rows] = list[idx];
      state.selection = { kind: "shards", group, rows };
      $$("#shard-table tbody tr").forEach((tr) =>
        tr.classList.toggle("is-picked", tr.contains(input)));
      renderBudgets();
    }));
}

/** Palpite inicial de orçamento a partir do que a máquina tem livre agora. */
function suggestedBudget(peer) {
  const sys = peer.node?.system;
  if (!sys) return 4096;
  const gpu = sys.gpus?.[0];
  if (sys.gpu_kind === "nvidia" && gpu?.vram_total_mb) {
    const free = gpu.vram_total_mb - (gpu.vram_used_mb || 0);
    return Math.max(1024, Math.round(free * 0.9 / 256) * 256);
  }
  // Unificada: metade do que está disponível, para não sufocar o sistema.
  return Math.max(1024, Math.round((sys.ram?.available_mb || 8192) * 0.5 / 256) * 256);
}

/** Os dois modos preenchem a mesma tabela com coisas diferentes. */
function setBudgetHeader(mode) {
  const shards = mode === "shards";
  $("#budget-title").textContent = shards
    ? "2 · Quem serve cada fatia"
    : "2 · Orçamento de memória por peer";
  $("#budget-meta").textContent = shards
    ? "As camadas já estão decididas pelas fatias; escolha a máquina de cada uma"
    : "Quantas camadas cabem sai do tamanho real de cada bloco do GGUF";
  $("#budget-head").innerHTML = shards
    ? `<tr><th>Fatia</th><th>Papel</th><th class="num">Instalada?</th><th class="num">Peer</th>
       <th class="num">Camadas</th><th class="num">Peso</th></tr>`
    : `<tr><th>Peer</th><th>Papel</th><th class="num">Memória livre</th>
       <th class="num">Orçamento (MB)</th><th class="num">Camadas</th><th class="num">Peso da fatia</th></tr>`;
  // A reserva só faz sentido quando ainda há camadas a distribuir.
  $(".reserve").hidden = shards;
}

function renderBudgets() {
  const tbody = $("#budget-table tbody");
  const sel = state.selection;
  const peers = state.cluster ? orderedPeers(state.cluster) : [];
  setBudgetHeader(sel?.kind ?? "full");

  if (!sel || !peers.length) {
    tbody.innerHTML = `<tr><td colspan="6" class="empty">Escolha um modelo acima.</td></tr>`;
    $("#budget-panel").hidden = false;
    return;
  }

  if (sel.kind === "shards") {
    // Uma linha por fatia, cada uma escolhendo o peer que vai servi-la.
    // Quem fica com a fatia inicial processa o prompt inteiro de cada
    // requisicao, entao vale colocar ali a maquina com mais memoria.
    tbody.innerHTML = sel.rows
      .map((row, i) => {
        const [start, end] = row.sample.shard_range;
        const options = peers
          .map((p, k) => {
            const has = row.byPeer.has(p.node_id);
            const chosen = k === i ? " selected" : "";
            return `<option value="${esc(p.node_id)}"${chosen}${has ? "" : " disabled"}>${
              esc(peerName(p.node_id))}${has ? "" : " (não tem a fatia)"}</option>`;
          })
          .join("");
        const missing = peers.filter((p) => !row.byPeer.has(p.node_id)).length;
        return `
        <tr data-shard="${i}">
          <td class="name">fatia ${start}–${end}</td>
          <td>${start === 0 ? "processa o prompt" : i === sel.rows.length - 1 ? "gera os tokens" : "meio"}</td>
          <td class="num">${missing === 0
            ? `<span class="chip has">em todos</span>`
            : `<span class="chip hasnt">falta em ${missing}</span>`}</td>
          <td class="num"><select class="shard-peer">${options}</select></td>
          <td class="num">${end - start + 1} camadas</td>
          <td class="num">${fmtBytes(row.sample.size_bytes)}</td>
        </tr>`;
      })
      .join("");
    // Trocar o peer de uma fatia invalida a divisão calculada.
    $$("#budget-table .shard-peer").forEach((s) =>
      s.addEventListener("change", () => buildShardPlan(sel)));
    $("#btn-plan").textContent = "Conferir fatias";
    return;
  }

  $("#btn-plan").textContent = "Calcular divisão";
  tbody.innerHTML = peers
    .map((p) => {
      const sys = p.node?.system;
      const has = sel.row.byPeer.has(p.node_id);
      const free = sys?.gpu_kind === "nvidia"
        ? fmtMB((sys.gpus[0].vram_total_mb || 0) - (sys.gpus[0].vram_used_mb || 0)) + " VRAM"
        : fmtMB(sys?.ram?.available_mb) + " RAM";
      return `
      <tr data-node="${esc(p.node_id)}" class="${has ? "" : "is-unavailable"}">
        <td class="name">${esc(peerName(p.node_id))}
          ${has ? "" : `<span class="sub">modelo não instalado aqui</span>`}</td>
        <td><label class="hint"><input type="checkbox" class="use-peer" ${has ? "checked" : ""} ${has ? "" : "disabled"}> usar</label></td>
        <td class="num">${free}</td>
        <td class="num"><input type="number" class="budget" value="${suggestedBudget(p)}" min="128" step="256" ${has ? "" : "disabled"}></td>
        <td class="num plan-layers">—</td>
        <td class="num plan-weight">—</td>
      </tr>`;
    })
    .join("");
}

$("#btn-plan").addEventListener("click", async () => {
  const sel = state.selection;
  if (!sel) return;

  if (sel.kind === "shards") {
    buildShardPlan(sel);
    return;
  }

  const budgets = [];
  let layoutNode = null;
  for (const tr of $$("#budget-table tbody tr[data-node]")) {
    if (!$(".use-peer", tr)?.checked) continue;
    const nodeId = tr.dataset.node;
    budgets.push({
      node_id: nodeId,
      label: peerName(nodeId),
      budget_mb: Number($(".budget", tr).value) || 0,
      has_model: sel.row.byPeer.has(nodeId),
    });
    if (!layoutNode && sel.row.byPeer.has(nodeId)) layoutNode = nodeId;
  }

  if (!budgets.length) {
    showPlanError("Marque ao menos um peer.");
    return;
  }
  if (!layoutNode) {
    showPlanError("Nenhum dos peers escolhidos tem esse modelo instalado.");
    return;
  }

  const btn = $("#btn-plan");
  btn.disabled = true;
  btn.textContent = "Calculando…";
  try {
    const plan = await postJSON("/api/ui/plan", {
      model_path: sel.row.byPeer.get(layoutNode).path,
      layout_node_id: layoutNode,
      budgets,
      reserve_pct: Number($("#reserve").value) || 0,
    });
    state.plan = { kind: "full", plan, selection: sel };
    renderPlan(plan);
  } catch (e) {
    showPlanError(e.message);
  } finally {
    btn.disabled = false;
    btn.textContent = "Calcular divisão";
  }
});

/** Conjunto já fatiado: monta um plano equivalente sem calcular nada. */
function buildShardPlan(sel) {
  const total = shardSetTotal(sel.rows);
  const shards = sel.rows.map((row, i) => {
    const chosen = $(`#budget-table tbody tr[data-shard="${i}"] .shard-peer`)?.value;
    const peer = state.cluster?.peers.find((p) => p.node_id === chosen);
    const [start, end] = row.sample.shard_range;
    return {
      node_id: peer?.node_id,
      label: peer ? peerName(peer.node_id) : "sem peer",
      start_layer: start,
      end_layer: end,
      layers: end - start + 1,
      weight_mb: (peer && row.byPeer.get(peer.node_id)?.size_bytes || 0) / 1048576,
      has_model: !!(peer && row.byPeer.has(peer.node_id)),
      is_first: start === 0,
      is_last: total ? end + 1 >= total : i === sel.rows.length - 1,
      shard_filename: peer ? row.byPeer.get(peer.node_id)?.path : null,
      budget_mb: 0,
    };
  });

  const warnings = [];
  shards.filter((s) => !s.node_id).forEach((s) =>
    warnings.push(`A fatia ${s.start_layer}–${s.end_layer} ficou sem peer.`));
  shards.filter((s) => s.node_id && !s.has_model).forEach((s) =>
    warnings.push(`${s.label} não tem a fatia ${s.start_layer}–${s.end_layer} instalada.`));
  // Duas fatias no mesmo peer significam carregar dois modelos no mesmo
  // processo, o que o servidor de inferência não faz.
  const seen = new Set();
  shards.filter((s) => s.node_id).forEach((s) => {
    if (seen.has(s.node_id)) {
      warnings.push(`${s.label} recebeu mais de uma fatia — cada peer serve uma só.`);
    }
    seen.add(s.node_id);
  });

  const covered = shards.reduce((max, s) => Math.max(max, s.end_layer + 1), 0);
  const plan = {
    base_name: sel.group,
    total_layers: total || covered,
    covered_layers: covered,
    complete: !total || covered >= total,
    shards,
    warnings,
    reserve_pct: 0,
    model_weight_mb: shards.reduce((a, s) => a + s.weight_mb, 0),
    missing_range: null,
  };
  state.plan = { kind: "shards", plan, selection: sel };
  renderPlan(plan);
}

function showPlanError(message) {
  $("#plan-panel").hidden = false;
  $("#plan-meta").textContent = "";
  $("#plan-ribbon").innerHTML = "";
  $("#plan-axis").innerHTML = "";
  $("#plan-legend").innerHTML = "";
  $("#plan-warnings").innerHTML = `<p class="notice notice-error">${esc(message)}</p>`;
  $("#btn-launch").disabled = true;
}

function renderPlan(plan) {
  $("#plan-panel").hidden = false;
  $("#plan-meta").textContent =
    `${plan.total_layers} camadas · modelo ${fmtMB(plan.model_weight_mb)}` +
    (plan.reserve_pct ? ` · reserva ${plan.reserve_pct}%` : "");

  // Mesma fita da tela inicial, agora como proposta.
  const total = plan.total_layers;
  const owners = new Array(total).fill(null);
  plan.shards.forEach((s, i) => {
    for (let l = s.start_layer; l <= Math.min(s.end_layer, total - 1); l++) {
      if (owners[l] === null) owners[l] = { shard: s, index: i };
    }
  });

  $("#plan-ribbon").innerHTML = owners
    .map((o, i) =>
      o
        ? `<span class="cell" style="--peer-c:${o.shard.node_id ? hueFor(o.shard.node_id) : hueVar(o.index)};--i:${i}" title="${esc(o.shard.label)} · camada ${i}"></span>`
        : `<span class="cell is-gap" style="--i:${i}" title="camada ${i} — sem dono"></span>`)
    .join("");
  $("#plan-axis").innerHTML = `<span>0</span><span>${total - 1}</span>`;
  $("#plan-legend").innerHTML = plan.shards
    .map((s, i) => `
      <span class="legend-item">
        <span class="legend-swatch" style="--peer-c:${s.node_id ? hueFor(s.node_id) : hueVar(i)}"></span>
        <span>${esc(s.label)}</span>
        <span class="range">${s.start_layer}–${s.end_layer} · ${s.layers} camadas · ${fmtMB(s.weight_mb)}</span>
      </span>`)
    .join("");

  // Espelha o resultado de volta na tabela de orçamentos.
  plan.shards.forEach((s) => {
    const tr = $(`#budget-table tbody tr[data-node="${s.node_id}"]`);
    if (!tr) return;
    $(".plan-layers", tr).textContent = `${s.start_layer}–${s.end_layer}`;
    $(".plan-weight", tr).textContent = fmtMB(s.weight_mb);
  });

  const notes = [];
  if (plan.complete) {
    notes.push(`<p class="notice notice-ok">Os peers cobrem as ${plan.total_layers} camadas do modelo.</p>`);
  } else {
    const [a, b] = plan.missing_range || [plan.covered_layers, plan.total_layers - 1];
    notes.push(
      `<p class="notice notice-error">Faltam as camadas ${a}–${b}. Sem elas o pipeline não fecha e nenhuma resposta sai.</p>`);
  }
  plan.warnings.forEach((w) => notes.push(`<p class="notice notice-warn">${esc(w)}</p>`));
  $("#plan-warnings").innerHTML = notes.join("");

  const sel = $("#primary-select");
  sel.innerHTML = plan.shards
    .filter((s) => s.node_id)
    .map((s, i) => `<option value="${esc(s.node_id)}" ${i === 0 ? "selected" : ""}>${esc(s.label)}</option>`)
    .join("");

  $("#btn-launch").disabled = !plan.shards.some((s) => s.node_id && s.has_model);
}

$("#btn-launch").addEventListener("click", async () => {
  const wrapper = state.plan;
  if (!wrapper) return;
  const { plan, selection } = wrapper;

  const targets = plan.shards
    .filter((s) => s.node_id && s.has_model)
    .map((s) => {
      if (wrapper.kind === "shards") {
        const row = selection.rows.find((r) => r.sample.shard_range[0] === s.start_layer);
        return {
          node_id: s.node_id,
          source_model_path: row.byPeer.get(s.node_id).path,
          start_layer: s.start_layer,
          end_layer: s.end_layer,
          skip_split: true,
        };
      }
      return {
        node_id: s.node_id,
        source_model_path: selection.row.byPeer.get(s.node_id).path,
        start_layer: s.start_layer,
        end_layer: s.end_layer,
        skip_split: false,
      };
    });

  const body = {
    base_name: wrapper.kind === "shards" ? plan.base_name : baseNameOf(selection.row.sample.path),
    total_layers: plan.total_layers,
    targets,
    primary_node_id: $("#primary-select").value,
    params: {
      n_gpu_layers: Number($("#p-gpu").value),
      n_ctx: Number($("#p-ctx").value),
      n_batch: Number($("#p-batch").value),
      max_new_tokens: Number($("#p-maxnew").value),
      max_sessions: Number($("#p-sessions").value),
      prefill_chunk_tokens: Number($("#p-chunk").value),
      temperature: Number($("#p-temp").value),
      top_p: Number($("#p-topp").value),
      top_k: Number($("#p-topk").value),
      repeat_penalty: Number($("#p-rep").value),
      repeat_last_n: Number($("#p-repn").value),
      no_thinking: $("#p-nothink").checked,
    },
  };

  const btn = $("#btn-launch");
  btn.disabled = true;
  btn.textContent = "Subindo…";
  const log = $("#launch-log");
  try {
    const res = await postJSON("/api/ui/launch", body);
    log.innerHTML = res.results
      .map((r) =>
        r.dispatched
          ? `<p class="notice notice-ok">${esc(peerName(r.node_id))}: pedido aceito${r.local ? " (este nó)" : ""}. Fatiar um modelo grande leva minutos — acompanhe no Painel.</p>`
          : `<p class="notice notice-error">${esc(peerName(r.node_id))}: ${esc(r.error || "falhou")}</p>`)
      .join("");
  } catch (e) {
    log.innerHTML = `<p class="notice notice-error">${esc(e.message)}</p>`;
  } finally {
    btn.disabled = false;
    btn.textContent = "Subir a rede";
  }
});

/**
 * Camadas do modelo de origem de um conjunto de fatias.
 *
 * Fatias geradas por `split_model.py` gravam `rdmap2p.original_block_count`.
 * As antigas não gravam nada, e aí o único número honesto é até onde as fatias
 * chegam — que é como `catalog::build_shard_sets` também resolve.
 */
function shardSetTotal(rows) {
  const declared = rows.find((r) => r.sample.total_layers)?.sample.total_layers;
  if (declared) return declared;
  const reach = rows.reduce((max, r) => Math.max(max, r.sample.shard_range[1] + 1), 0);
  return reach || undefined;
}

/** Espelha `plan::base_name_of` para nomear as fatias igual em todos os peers. */
function baseNameOf(path) {
  const stem = path.split("/").pop().replace(/\.gguf$/i, "");
  const parts = stem.split("_");
  if (parts.length >= 3 && /^\d+$/.test(parts.at(-1)) && /^\d+$/.test(parts.at(-2))) {
    return parts.slice(0, -2).join("_");
  }
  return stem;
}

// ── Modelos ──────────────────────────────────────────────────────────────────
//
// Três coisas que só o dono da máquina pode decidir: onde os arquivos estão,
// em quantas fatias cortar um modelo, e para quem mandar uma fatia.

const modelos = {
  /** Resposta de /api/ui/paths. */
  paths: null,
  /** Lista em edição das pastas de modelos inteiros. */
  dirs: [],
  shardDir: "",
  /** "local" (só desta máquina) ou "all" (todos os nós). */
  scope: "local",
  /** Fatiamento em curso. */
  job: null,
  jobTimer: null,
  jobModel: "",
  /** Lista de fatias em montagem, quando o painel está aberto. */
  builder: null,
  builderTimer: null,
};

async function loadModelsTab() {
  await Promise.all([loadPaths(), loadClusterModels()]);
}

// ── Pastas ───────────────────────────────────────────────────────────────────

async function loadPaths() {
  try {
    modelos.paths = await getJSON("/api/ui/paths");
    modelos.dirs = modelos.paths.model_dirs.map((d) => d.path);
    modelos.shardDir = modelos.paths.shard_dir.path;
    renderPaths();
  } catch (e) {
    $("#path-list").innerHTML =
      `<p class="notice notice-error">Não deu para ler a configuração: ${esc(e.message)}</p>`;
  }
}

const KIND_LABEL = { ollama: "Ollama", lmstudio: "LM Studio", folder: "Pasta" };

/** O que a varredura vai fazer com um caminho, dito antes de salvar. */
function kindOf(path) {
  const known = modelos.paths?.model_dirs.concat(modelos.paths.detected || [])
    .find((d) => d.path === path);
  if (known) return known.kind;
  const t = String(path).toLowerCase();
  if (t.includes(".ollama") || t.endsWith("ollama/models")) return "ollama";
  if (t.includes("lmstudio") || t.includes("lm-studio")) return "lmstudio";
  return "folder";
}

function renderPaths() {
  const box = $("#path-list");
  if (!box) return;

  box.innerHTML = modelos.dirs.length
    ? modelos.dirs.map((dir, i) => {
        const known = modelos.paths?.model_dirs.find((d) => d.path === dir);
        const falta = known && !known.exists;
        return `
        <div class="path-row">
          <input class="secret-field path-field ${falta ? "is-bad" : ""}" data-index="${i}"
                 value="${esc(dir)}" spellcheck="false" aria-label="Pasta ${i + 1}">
          <span class="path-kind">${esc(KIND_LABEL[kindOf(dir)] || "pasta")}</span>
          <button class="btn-slim" data-remove="${i}" type="button" title="Remover da lista">Remover</button>
        </div>`;
      }).join("")
    : `<p class="empty">Nenhuma pasta de modelos inteiros. Só as fatias serão encontradas.</p>`;

  // Só reescreve o campo quando ele realmente ficou para trás: redesenhar a
  // lista (acrescentar ou remover uma pasta) apagava o destino que a pessoa
  // tinha acabado de digitar.
  const shardField = $("#path-shard");
  if (shardField.value !== modelos.shardDir) shardField.value = modelos.shardDir;

  const meta = $("#paths-state");
  if (meta) {
    meta.textContent = modelos.paths?.using_defaults
      ? "hoje: bibliotecas detectadas automaticamente — salvar fixa esta lista"
      : "lista escolhida por você";
  }

  $$("#path-list .path-field").forEach((input) =>
    input.addEventListener("input", () => {
      modelos.dirs[Number(input.dataset.index)] = input.value;
    }));
  $$("#path-list [data-remove]").forEach((btn) =>
    btn.addEventListener("click", () => {
      modelos.dirs.splice(Number(btn.dataset.remove), 1);
      renderPaths();
    }));
}

$("#path-shard")?.addEventListener("input", (ev) => {
  modelos.shardDir = ev.target.value;
});

$("#btn-path-add")?.addEventListener("click", () => {
  const field = $("#path-new");
  const value = field.value.trim();
  if (!value) return;
  if (modelos.dirs.includes(value)) {
    pathsMessage("Essa pasta já está na lista.", "warn");
    return;
  }
  modelos.dirs.push(value);
  field.value = "";
  renderPaths();
  pathsMessage("Pasta acrescentada — falta salvar.", "");
});

function pathsMessage(text, kind) {
  const el = $("#paths-msg");
  if (!el) return;
  el.className = kind === "error" ? "notice notice-error"
    : kind === "ok" ? "notice notice-ok"
    : kind === "warn" ? "notice notice-warn"
    : "muted-note";
  el.textContent = text;
}

$("#btn-paths-save")?.addEventListener("click", async () => {
  const btn = $("#btn-paths-save");
  btn.disabled = true;
  pathsMessage("Conferindo as pastas…", "");
  try {
    const saved = await postJSON("/api/ui/paths", {
      model_dirs: modelos.dirs.map((d) => d.trim()).filter(Boolean),
      shard_dir: modelos.shardDir.trim(),
    });
    modelos.paths = saved;
    modelos.dirs = saved.model_dirs.map((d) => d.path);
    modelos.shardDir = saved.shard_dir.path;
    renderPaths();
    pathsMessage("Pastas salvas. A varredura já usa estas.", "ok");
    loadClusterModels();
  } catch (e) {
    pathsMessage(e.message, "error");
  } finally {
    btn.disabled = false;
  }
});

// ── Modelos inteiros: filtro e fatiamento ────────────────────────────────────

$$('input[name="whole-scope"]').forEach((r) =>
  r.addEventListener("change", () => {
    if (!r.checked) return;
    modelos.scope = r.value;
    renderWholeModels();
  }));

/** O peer que somos, dentro do catálogo do cluster. */
const selfPeer = () => state.clusterModels?.peers.find((p) => p.is_self);

/** Diretório onde um arquivo está, que é o que interessa quando a lista é local. */
const dirOf = (path) => String(path).split("/").slice(0, -1).join("/") || "/";

function renderWholeModels() {
  const tbody = $("#whole-table tbody");
  if (!tbody) return;
  const meta = $("#whole-meta");
  const cm = state.clusterModels;

  if (!cm) {
    tbody.innerHTML = `<tr><td colspan="7" class="empty">Lendo as bibliotecas…</td></tr>`;
    return;
  }

  const local = modelos.scope === "local";
  const me = selfPeer();
  const linhas = local
    ? (me?.models || [])
        .filter((m) => !m.shard_range)
        .map((m) => ({ name: m.name, sample: m, byPeer: new Map([[me.node_id, m]]) }))
        .sort((a, b) => a.name.localeCompare(b.name))
    : unionModels((m) => !m.shard_range);

  if (meta) {
    meta.textContent = local
      ? `${linhas.length} nesta máquina (${esc(peerName(me?.node_id || ""))}) · fatiar só acontece aqui`
      : `${linhas.length} somando todos os nós — só os desta máquina podem ser fatiados daqui`;
  }

  if (!linhas.length) {
    tbody.innerHTML = `<tr><td colspan="7" class="empty">${
      local
        ? "Nenhum modelo inteiro nas pastas configuradas acima."
        : "Nenhum modelo inteiro em nenhum nó da rede."
    }</td></tr>`;
    return;
  }

  tbody.innerHTML = linhas.map((row) => {
    const m = row.sample;
    const aqui = me ? row.byPeer.get(me.node_id) : null;
    const onde = local
      ? `<span class="path-cell" title="${esc(m.path)}">${esc(dirOf(m.path))}</span>`
      : availabilityChips(row.byPeer);
    const acao = aqui
      ? `<button class="btn-slim" type="button" data-slice="${esc(aqui.path)}"
                 data-name="${esc(row.name)}" data-size="${aqui.size_bytes}">Fatiar…</button>`
      : `<span class="muted-note">não está aqui</span>`;
    return `
      <tr>
        <td class="name">${esc(row.name)}
          <span class="sub">${esc(modelSubtitle(m))}</span></td>
        <td>${esc(m.source_label)}</td>
        <td>${esc(m.quant || "—")}</td>
        <td class="num">${m.total_layers ?? "—"}</td>
        <td class="num">${fmtBytes(m.size_bytes)}</td>
        <td>${onde}</td>
        <td class="num">${acao}</td>
      </tr>`;
  }).join("");

  tbody.querySelectorAll("[data-slice]").forEach((btn) =>
    btn.addEventListener("click", () =>
      openSliceBuilder(btn.dataset.slice, btn.dataset.name, Number(btn.dataset.size) || 0)));

  setSliceButtonsDisabled(!!modelos.job && modelos.job.state === "running");
}

function setSliceButtonsDisabled(disabled) {
  $$("#whole-table [data-slice]").forEach((b) => { b.disabled = disabled; });
}

// ── Montagem das fatias ──────────────────────────────────────────────────────
//
// As máquinas do cluster não são parecidas, então dividir em partes iguais é a
// resposta errada quase sempre: quem manda no tamanho de uma fatia é a memória
// da máquina que vai recebê-la. Aqui se monta a lista fatia a fatia, em GB, e a
// tela mostra em que faixa de camadas cada tamanho caiu — o arredondamento é
// para camada inteira e aparece, não fica escondido.

/** Uma fatia da lista em edição: alvo em GB, ou "o resto". */
const novaFatia = (gb) => ({ gb, resto: false });

function openSliceBuilder(path, name, sizeBytes) {
  modelos.builder = {
    path,
    name,
    sizeBytes,
    fatias: [novaFatia(""), { gb: "", resto: true }],
    plan: null,
    erro: "",
    calculando: false,
  };
  renderSliceBuilder();
  // Abre já com uma proposta completa em vez de dois campos vazios.
  fillEqualParts(2);
}

function closeSliceBuilder() {
  modelos.builder = null;
  clearTimeout(modelos.builderTimer);
  $("#slice-builder").hidden = true;
}

/** Pede ao nó a conversão GB → camadas. A conta é do Rust, e é a mesma que roda
 *  na hora de fatiar de verdade. */
function scheduleSlicePlan() {
  clearTimeout(modelos.builderTimer);
  modelos.builderTimer = setTimeout(refreshSlicePlan, 250);
}

async function refreshSlicePlan() {
  const b = modelos.builder;
  if (!b) return;
  const slices = b.fatias.map((f) => ({
    target_mb: f.resto ? null : Math.round((Number(f.gb) || 0) * 1024),
  }));
  if (!slices.length) {
    b.plan = null;
    b.erro = "Acrescente ao menos uma fatia.";
    renderSliceBuilder();
    return;
  }
  b.calculando = true;
  try {
    b.plan = await postJSON("/api/ui/slice/plan", { model_path: b.path, slices });
    b.erro = "";
  } catch (e) {
    b.plan = null;
    b.erro = e.message;
  } finally {
    b.calculando = false;
    renderSliceBuilder();
  }
}

/** Atalho: preenche a lista com N fatias de camadas iguais, que continuam
 *  editáveis — o alvo de cada uma já vem com o tamanho real daquela faixa. */
async function fillEqualParts(n) {
  const b = modelos.builder;
  if (!b) return;
  b.calculando = true;
  renderSliceBuilder();
  try {
    const plan = await postJSON("/api/ui/slice/plan", { model_path: b.path, equal_parts: n });
    b.fatias = plan.ranges.map((r) => novaFatia((r.bytes / 1073741824).toFixed(1)));
    b.plan = plan;
    b.erro = "";
  } catch (e) {
    b.erro = e.message;
  } finally {
    b.calculando = false;
    renderSliceBuilder();
  }
}

function renderSliceBuilder() {
  const box = $("#slice-builder");
  const b = modelos.builder;
  if (!box) return;
  if (!b) {
    box.hidden = true;
    return;
  }
  box.hidden = false;

  const plan = b.plan;
  const faixaDe = (i) => plan?.ranges?.[i];

  const linhas = b.fatias.map((f, i) => {
    const r = faixaDe(i);
    const faixa = !r || r.empty
      ? `<span class="muted-note">sem camadas</span>`
      : `camadas ${r.start_layer}–${r.end_layer} <span class="muted-note">(${r.layers})</span>`;
    const real = r && !r.empty ? fmtBytes(r.bytes) : "—";
    const alerta = r?.too_small
      ? `<span class="chip hasnt">não cabe uma camada</span>`
      : r?.empty
        ? `<span class="chip hasnt">nada sobrou</span>`
        : "";
    return `
      <tr data-fatia="${i}">
        <td class="num">${i + 1}</td>
        <td class="num">
          ${f.resto
            ? `<span class="chip has">o resto</span>`
            : `<input type="number" class="fatia-gb" min="0.1" step="0.1" value="${esc(f.gb)}"
                      aria-label="Tamanho da fatia ${i + 1} em GB"> GB`}
        </td>
        <td class="num">${faixa}</td>
        <td class="num">${real} ${alerta}</td>
        <td class="num">
          <label class="hint"><input type="checkbox" class="fatia-resto" ${f.resto ? "checked" : ""}> o resto</label>
          <button class="btn-slim" type="button" data-remove-fatia="${i}"
                  ${b.fatias.length === 1 ? "disabled" : ""}>Remover</button>
        </td>
      </tr>`;
  }).join("");

  const faltam = plan ? plan.total_layers - plan.covered_layers : 0;
  const cobertura = plan
    ? plan.complete
      ? `<p class="notice notice-ok">As fatias cobrem as ${plan.total_layers} camadas do modelo.</p>`
      : `<p class="notice notice-warn">Cobertas ${plan.covered_layers} de ${plan.total_layers} camadas: `
        + `faltam ${faltam} (${plan.missing_range[0]}–${plan.missing_range[1]}, ${fmtBytes(plan.missing_bytes)}). `
        + `Acrescente uma fatia ou marque a última como “o resto”.</p>`
    : "";

  const avisos = (plan?.warnings || [])
    .map((w) => `<p class="notice notice-warn">${esc(w)}</p>`).join("");
  const erro = b.erro ? `<p class="notice notice-error">${esc(b.erro)}</p>` : "";
  const impedido = !plan || plan.blocked || !!b.erro || b.calculando;

  box.innerHTML = `
    <div class="slice-builder-head">
      <strong>Fatiar ${esc(b.name)}</strong>
      <span class="muted-note">${b.sizeBytes ? fmtBytes(b.sizeBytes) : ""}${
        plan ? ` · ${plan.total_layers} camadas` : ""}</span>
      <button class="btn-slim" type="button" id="btn-slice-cancel">Cancelar</button>
    </div>

    <p class="sub-note">O tamanho de cada fatia é o que a máquina de destino aguenta, não uma
      divisão aritmética. O valor em GB vira uma faixa de camadas inteiras — o tamanho real ao
      lado é somado bloco a bloco da tabela de tensores, e por isso não bate exatamente com o alvo.</p>

    <div class="table-scroll">
      <table class="grid" id="fatia-table">
        <thead>
          <tr><th class="num">#</th><th class="num">Alvo</th><th class="num">Faixa</th>
              <th class="num">Tamanho real</th><th class="num"></th></tr>
        </thead>
        <tbody>${linhas}</tbody>
      </table>
    </div>

    ${cobertura}${avisos}${erro}

    <div class="launch-row">
      <span class="slice-builder-tools">
        <button class="btn-slim" type="button" id="btn-fatia-add">+ Acrescentar fatia</button>
        <label class="hint">dividir em
          <input type="number" id="equal-parts" min="1" max="64" step="1" value="2"> iguais</label>
        <button class="btn-slim" type="button" id="btn-fatia-equal">Preencher</button>
      </span>
      <button class="btn btn-go" id="btn-fatia-go" ${impedido ? "disabled" : ""}>
        ${b.calculando ? "Calculando…" : "Fatiar"}</button>
    </div>`;

  $$("#fatia-table .fatia-gb").forEach((input) =>
    input.addEventListener("input", () => {
      const i = Number(input.closest("[data-fatia]").dataset.fatia);
      modelos.builder.fatias[i].gb = input.value;
      scheduleSlicePlan();
    }));
  $$("#fatia-table .fatia-resto").forEach((chk) =>
    chk.addEventListener("change", () => {
      const i = Number(chk.closest("[data-fatia]").dataset.fatia);
      modelos.builder.fatias[i].resto = chk.checked;
      renderSliceBuilder();
      refreshSlicePlan();
    }));
  $$("#fatia-table [data-remove-fatia]").forEach((btn) =>
    btn.addEventListener("click", () => {
      modelos.builder.fatias.splice(Number(btn.dataset.removeFatia), 1);
      renderSliceBuilder();
      refreshSlicePlan();
    }));

  $("#btn-fatia-add").addEventListener("click", () => {
    modelos.builder.fatias.push(novaFatia(""));
    renderSliceBuilder();
    refreshSlicePlan();
  });
  $("#btn-fatia-equal").addEventListener("click", () =>
    fillEqualParts(Math.max(1, Math.min(64, Number($("#equal-parts").value) || 2))));
  $("#btn-slice-cancel").addEventListener("click", closeSliceBuilder);
  $("#btn-fatia-go").addEventListener("click", startSlice);
}

async function startSlice() {
  const b = modelos.builder;
  if (!b) return;
  const slices = b.fatias.map((f) => ({
    target_mb: f.resto ? null : Math.round((Number(f.gb) || 0) * 1024),
  }));

  modelos.jobModel = b.name;
  renderSliceJob({ state: "running", step: "preparando", progress: null, message: "" });
  setSliceButtonsDisabled(true);
  try {
    const { job_id } = await postJSON("/api/ui/slice", {
      model_path: b.path,
      name: b.name,
      slices,
    });
    closeSliceBuilder();
    modelos.job = { id: job_id, state: "running" };
    pollSliceJob(job_id);
  } catch (e) {
    modelos.job = null;
    setSliceButtonsDisabled(false);
    renderSliceJob({ state: "failed", step: "erro", progress: null, message: e.message });
  }
}

function pollSliceJob(id) {
  clearTimeout(modelos.jobTimer);
  const tick = async () => {
    try {
      const job = await getJSON(`/api/ui/jobs/${id}`);
      modelos.job = job;
      renderSliceJob(job);
      if (job.state === "running") {
        modelos.jobTimer = setTimeout(tick, 800);
        return;
      }
      setSliceButtonsDisabled(false);
      // As fatias novas entram no catálogo sem reiniciar nada. A varredura corre
      // atrás da resposta que o servidor acabou de dar, então insiste até as
      // fatias aparecerem — numa biblioteca em cartão SD isso leva um minuto.
      awaitCatalog(job.outputs || []);
    } catch (e) {
      setSliceButtonsDisabled(false);
      renderSliceJob({ state: "failed", step: "erro", progress: null, message: e.message });
    }
  };
  modelos.jobTimer = setTimeout(tick, 400);
}

/** Recarrega o catálogo até ele conhecer os arquivos recém-criados. */
async function awaitCatalog(outputs, tentativas = 8) {
  await loadClusterModels();
  const nomes = outputs.map((o) => o.split("/").pop());
  const conhecidos = new Set((selfPeer()?.models || []).map((m) => m.name));
  if (!nomes.length || nomes.every((n) => conhecidos.has(n)) || tentativas <= 0) return;
  setTimeout(() => awaitCatalog(outputs, tentativas - 1), 4000);
}

function renderSliceJob(job) {
  const box = $("#slice-job");
  if (!box) return;
  box.hidden = false;

  const falhou = job.state === "failed";
  const pronto = job.state === "done";
  const pct = job.progress;
  const indet = pct == null && !pronto && !falhou;
  const largura = pronto ? 100 : pct ?? 100;

  const tag = pronto ? "is-ready" : falhou ? "is-failed" : "is-starting";
  const estado = pronto ? "pronto" : falhou ? "falhou" : job.step || "fatiando";

  box.className = `slice-job ${falhou ? "is-failed" : ""}`;
  box.innerHTML = `
    <div class="slice-job-head">
      <strong>${esc(modelos.jobModel || "modelo")}</strong>
      <span class="tagrow"><span class="tag ${tag}">${esc(estado)}</span></span>
      <span class="slice-pct">${pronto ? "100%" : falhou ? "—" : pct != null ? `${pct.toFixed(0)}%` : "…"}</span>
    </div>
    <div class="meter-track">
      <div class="meter-fill ${falhou ? "is-high" : ""} ${indet ? "is-indeterminate" : ""}"
           style="width:${largura}%"></div>
    </div>
    ${job.message ? `<p class="${falhou ? "notice notice-error" : "meter-note"}">${esc(job.message)}</p>` : ""}
    ${(job.outputs || []).length
      ? `<ul class="slice-outputs">${job.outputs
          .map((o) => `<li title="${esc(o)}">${esc(o.split("/").pop())}</li>`)
          .join("")}</ul>`
      : ""}`;
}

// ── Fatias desta máquina: mandar uma para quem não tem ────────────────────────

/** Um arquivo com este nome existe naquele peer? É o mesmo critério do painel
 *  de trocas na aba Nós. */
function peerHasFile(peer, name) {
  const alvo = String(name).toLowerCase();
  return (peer.models || []).some((m) => (m.name || "").toLowerCase() === alvo);
}

function renderLocalShards() {
  const box = $("#local-shards");
  if (!box) return;
  const cm = state.clusterModels;
  const me = selfPeer();

  if (!cm || !me) {
    box.innerHTML = `<p class="empty">Lendo as bibliotecas…</p>`;
    return;
  }

  const fatias = (me.models || [])
    .filter((m) => m.shard_range)
    .sort((a, b) => a.name.localeCompare(b.name));

  if (!fatias.length) {
    box.innerHTML = `<p class="empty">Nenhuma fatia nesta máquina ainda. Fatie um modelo inteiro acima.</p>`;
    return;
  }

  const outros = cm.peers.filter((p) => !p.is_self);

  box.innerHTML = fatias.map((m, i) => {
    const [a, b] = m.shard_range;
    const faltam = outros.filter((p) => !peerHasFile(p, m.name));
    const tem = outros.filter((p) => peerHasFile(p, m.name));
    const tempo = m.size_bytes ? ` · ~${fmtDuration(m.size_bytes / MEDIDA_BYTES_POR_S)} no enlace medido` : "";

    const destino = !outros.length
      ? `<span class="muted-note">nenhuma outra máquina no cluster</span>`
      : !faltam.length
        ? `<span class="chip has">todas já têm</span>`
        : `<span class="send-cell">
             <select class="shard-dest" aria-label="Máquina de destino">${faltam
               .map((p) => `<option value="${esc(p.node_id)}">${esc(uniqueName(p.node_id, cm.peers))}</option>`)
               .join("")}</select>
             <button class="btn-slim" type="button" data-send="${i}">Enviar</button>
           </span>`;

    return `
      <div class="node-row" data-shard-row="${i}" data-path="${esc(m.path)}">
        <div class="node-row-main">
          <strong>${esc(m.name)}</strong>
          <span class="node-row-sub">camadas ${a}–${b} · ${fmtBytes(m.size_bytes)}${tempo}</span>
          <span class="node-row-sub" title="${esc(m.path)}">${esc(dirOf(m.path))}</span>
          <span class="send-msg muted-note" hidden></span>
        </div>
        <div class="tagrow">${
          // Quando ninguém está faltando, a própria ação já diz isso; repetir
          // máquina por máquina só ocupa a linha.
          faltam.length
            ? tem.map((p) => `<span class="tag is-ready">${esc(peerName(p.node_id))} já tem</span>`).join("")
            : ""}</div>
        <div class="node-row-actions">${destino}</div>
      </div>`;
  }).join("");

  box.querySelectorAll("[data-send]").forEach((btn) =>
    btn.addEventListener("click", () => sendShardFromRow(btn)));
}

async function sendShardFromRow(btn) {
  const row = btn.closest("[data-shard-row]");
  const destino = row.querySelector(".shard-dest")?.value;
  const msg = row.querySelector(".send-msg");
  const path = row.dataset.path;
  if (!destino || !path) return;

  btn.disabled = true;
  msg.hidden = false;
  msg.className = "send-msg muted-note";
  msg.textContent = "Calculando o MD5 da fatia…";
  try {
    await ensureConnectionKey();
    const r = await fetch("/api/transfer/send", {
      method: "POST",
      headers: { ...authHeaders(), "Content-Type": "application/json" },
      body: JSON.stringify({ to_node_id: destino, path }),
    });
    const d = await r.json().catch(() => ({}));
    msg.className = r.ok ? "send-msg notice notice-ok" : "send-msg notice notice-error";
    msg.textContent = r.ok
      ? "Oferta enviada. O outro nó aceita no sino, e o progresso aparece em Transferências (aba Nós)."
      : "Não deu para oferecer: " + (d.error || r.status);
    pollTransfers();
  } catch (e) {
    msg.className = "send-msg notice notice-error";
    msg.textContent = "Não deu para oferecer: " + e.message;
  } finally {
    btn.disabled = false;
  }
}

// ── Testar ───────────────────────────────────────────────────────────────────

$("#chat-form").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  if (state.streaming) return;

  const prompt = $("#chat-input").value.trim() || $("#chat-input").placeholder;
  const out = $("#chat-output");
  out.textContent = "";
  state.streaming = true;
  $("#btn-send").disabled = true;
  $("#ribbon").classList.add("is-running");

  try {
    // As rotas /v1 exigem chave. O painel usa a chave de conexão deste nó,
    // que o ApiKeyAuth aceita como credencial de operador.
    await ensureConnectionKey();
    const resp = await fetch("/v1/chat/completions", {
      method: "POST",
      headers: { "content-type": "application/json", ...authHeaders() },
      body: JSON.stringify({
        model: state.cluster?.active_model,
        messages: [{ role: "user", content: prompt }],
        stream: true,
      }),
    });

    if (!resp.ok || !resp.body) {
      out.textContent = `Falhou: HTTP ${resp.status}\n${await resp.text()}`;
      return;
    }

    const reader = resp.body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";

    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });

      // SSE: eventos separados por linha em branco.
      let sep;
      while ((sep = buffer.indexOf("\n\n")) !== -1) {
        const raw = buffer.slice(0, sep);
        buffer = buffer.slice(sep + 2);
        for (const line of raw.split("\n")) {
          if (!line.startsWith("data:")) continue;
          const payload = line.slice(5).trim();
          if (payload === "[DONE]") continue;
          try {
            const chunk = JSON.parse(payload);
            if (chunk.error) {
              out.textContent += `\n\n[erro] ${chunk.error.message}`;
              continue;
            }
            const delta = chunk.choices?.[0]?.delta?.content;
            if (delta) {
              out.textContent += delta;
              out.scrollTop = out.scrollHeight;
            }
          } catch { /* pedaço parcial: volta no próximo read */ }
        }
      }
    }
  } catch (e) {
    out.textContent += `\n\n[erro] ${e.message}`;
  } finally {
    state.streaming = false;
    $("#btn-send").disabled = false;
    $("#ribbon").classList.remove("is-running");
  }
});

// ── Início ───────────────────────────────────────────────────────────────────

pollCluster();

// Notification polling
async function pollNotifications() {
  try {
    await ensureConnectionKey();
    const r = await fetch('/api/transfer/offers', { headers: authHeaders() });
    if (!r.ok) return;
    const data = await r.json();
    state.diskFreeMb = data.disk_free_mb;
    updateNotificationBell(data.offers || []);
  } catch(e) {}
}

function updateNotificationBell(offers) {
  const count = document.getElementById('notif-count');
  const dropdown = document.getElementById('notif-dropdown');
  if (!count || !dropdown) return;

  if (offers.length > 0) {
    count.textContent = offers.length;
    count.classList.remove('hidden');
    dropdown.innerHTML = offers.map(o => {
      const cabe = state.diskFreeMb == null || state.diskFreeMb > (o.size_bytes / 1048576) + 512;
      const faixa = (o.start_layer != null && o.end_layer != null)
        ? `camadas ${o.start_layer}–${o.end_layer}` : 'arquivo inteiro';
      return `
      <div class="notif-item">
        <strong>${esc(o.from_host)} quer te enviar um arquivo</strong>
        <span class="notif-sub">${esc(o.name)} · ${fmtBytes(o.size_bytes)} · ${faixa}</span>
        <span class="notif-sub" title="${esc(o.remote_path || '')}">de <code>${
          esc(o.remote_path || '—')}</code></span>
        <span class="notif-sub">${cabe
          ? `Cabe no disco (${fmtMB(state.diskFreeMb)} livres)`
          : `<b>Espaço insuficiente</b> — só há ${fmtMB(state.diskFreeMb)} livres`}</span>
        <div class="notif-actions">
          <button class="btn-slim" onclick="respondTransfer('${esc(o.id)}', true)">Aceitar</button>
          <button class="btn-slim" onclick="respondTransfer('${esc(o.id)}', false)">Recusar</button>
        </div>
      </div>`;
    }).join('');
  } else {
    count.classList.add('hidden');
    dropdown.innerHTML = '<div class="notif-item" style="color:var(--muted)">Nenhuma notificacao</div>';
  }
}

async function respondTransfer(id, accept) {
  try {
    await ensureConnectionKey();
    const acao = accept ? 'accept' : 'reject';
    const r = await fetch(`/api/transfer/offers/${id}/${acao}`, {
      method: 'POST', headers: authHeaders(),
    });
    if (!r.ok) {
      const d = await r.json().catch(() => ({}));
      alert('Não deu para ' + (accept ? 'aceitar' : 'recusar') + ': ' + (d.error || r.status));
    }
    pollNotifications();
    pollTransfers();
  } catch(e) {}
}

// ── Transferências de fatias ────────────────────────────────────────────────

/** Progresso das transferências em curso, com velocidade e tempo restante.
 *  A 10 MB/s medidos nesta LAN uma fatia de 10 GB leva ~17 min, então isto
 *  não é enfeite: é como se sabe que a coisa está andando. */
async function pollTransfers() {
  const box = document.getElementById('transfer-jobs');
  if (!box) return;
  try {
    await ensureConnectionKey();
    const r = await fetch('/api/transfer/jobs', { headers: authHeaders() });
    if (!r.ok) return;
    const { jobs } = await r.json();
    box.innerHTML = jobs.length
      ? jobs.map(renderTransferJob).join('')
      : '<p class="muted-note">Nenhuma transferência em curso.</p>';
  } catch (e) {}
}

function fmtDuration(secs) {
  if (secs == null) return "—";
  if (secs < 60) return `${Math.round(secs)}s`;
  const m = Math.floor(secs / 60), s = Math.round(secs % 60);
  if (m < 60) return `${m}min ${s}s`;
  return `${Math.floor(m / 60)}h ${m % 60}min`;
}

function renderTransferJob(j) {
  const pct = j.bytes_total ? (j.bytes_done / j.bytes_total) * 100 : 0;
  const falhou = j.state === 'falhou';
  const pronto = j.state === 'concluída';
  return `
    <div class="node-row">
      <div class="node-row-main">
        <strong>${esc(j.name)}</strong>
        <span class="node-row-sub">
          de ${esc(j.from_host)} · ${fmtBytes(j.bytes_done)} de ${fmtBytes(j.bytes_total)}
          ${pronto || falhou ? '' : `· ${fmtBytes(j.bytes_per_sec)}/s · faltam ${fmtDuration(j.eta_secs)}`}
        </span>
        <div class="meter-track" style="margin-top:6px">
          <div class="meter-fill ${falhou ? 'is-high' : ''}" style="width:${pct.toFixed(1)}%"></div>
        </div>
        ${j.detail ? `<span class="node-row-sub">${esc(j.detail)}</span>` : ''}
      </div>
      <div class="tagrow">
        <span class="tag ${pronto ? 'is-ready' : falhou ? 'is-failed' : 'is-starting'}">${esc(j.state)}</span>
      </div>
      <div></div>
    </div>`;
}

/** Fatias que estão numa máquina e faltam na outra. */
function renderSliceExchange(cm) {
  const box = document.getElementById('slice-exchange');
  if (!box || !cm) return;
  const me = cm.peers.find(p => p.is_self);
  const outros = cm.peers.filter(p => !p.is_self);
  if (!me) return;

  const meus = new Set((me.models || []).map(m => (m.name || '').toLowerCase()));
  const linhas = [];

  for (const peer of outros) {
    const deles = new Set((peer.models || []).map(m => (m.name || '').toLowerCase()));

    // o que eles têm e eu não: posso puxar
    for (const m of (peer.models || [])) {
      if (meus.has((m.name || '').toLowerCase())) continue;
      linhas.push(sliceRow(m, peer, 'puxar'));
    }
    // o que eu tenho e eles não: posso oferecer
    for (const m of (me.models || [])) {
      if (deles.has((m.name || '').toLowerCase())) continue;
      linhas.push(sliceRow(m, peer, 'enviar'));
    }
  }

  box.innerHTML = linhas.length
    ? linhas.join('')
    : '<p class="muted-note">As duas máquinas têm exatamente os mesmos arquivos — nada a trocar.</p>';
}

/** Puxar é pedir a outra máquina um arquivo que ninguém de lá escolheu, então o
 *  guarda de caminho dela só libera o que está em `models/`. Modelos do Ollama e
 *  do LM Studio moram fora dali: aparecem na lista sem botão de baixar, com o
 *  motivo escrito e o caminho que resolve (pedir o envio do outro lado). */
function podePuxar(m) {
  return (m.source || '').toLowerCase() === 'local';
}

/** Enviar é o dono da máquina escolhendo o arquivo, então vale qualquer caminho
 *  legível: o nó registra uma concessão e o outro lado busca por ela, sem nunca
 *  nomear um caminho nosso. Ver `transfer::TransferState::grants`. */
function podeEnviar() {
  return true;
}

function sliceRow(m, peer, acao) {
  const tipo = m.shard_range
    ? `fatia · camadas ${m.shard_range[0]}–${m.shard_range[1]}`
    : 'modelo inteiro';
  const host = shortHost(peer.hostname || peer.node_id);
  const onde = acao === 'puxar'
    ? `está em ${esc(host)}, falta aqui`
    : `está aqui, falta em ${esc(host)}`;

  const podeIr = acao === 'puxar' ? podePuxar(m) : podeEnviar();
  const nome = m.path ? m.path.split('/').pop() : m.name;
  const tempo = m.size_bytes ? ` · ~${fmtDuration(m.size_bytes / MEDIDA_BYTES_POR_S)} no enlace medido` : '';

  const acaoHtml = podeIr
    ? (acao === 'puxar'
        ? `<button class="btn-slim" onclick="pullSlice('${esc(peer.node_id)}','${esc(m.path)}','${esc(nome)}')">Baixar</button>`
        : `<button class="btn-slim" onclick="sendSlice('${esc(peer.node_id)}','${esc(m.path)}')">Enviar</button>`)
    : `<span class="muted-note">peça o envio de lá</span>`;

  return `
    <div class="node-row">
      <div class="node-row-main">
        <strong>${esc(m.name)}</strong>
        <span class="node-row-sub">${fmtBytes(m.size_bytes)} · ${tipo} · ${onde}${podeIr ? tempo : ''}</span>
        ${podeIr ? '' : `<span class="node-row-sub">Este veio de
          ${esc(m.source_label || m.source || 'outra biblioteca')}, fora de <code>models/</code>, e
          baixar é este nó pedindo um caminho que ninguém de lá escolheu. Abra o painel de
          ${esc(host)} e use “Enviar um arquivo desta máquina”.</span>`}
      </div>
      <div class="tagrow"><span class="tag${podeIr ? '' : ' is-failed'}">${
        podeIr ? (acao === 'puxar' ? 'baixar' : 'enviar') : 'peça de lá'}</span></div>
      <div class="node-row-actions">${acaoHtml}</div>
    </div>`;
}

async function pullSlice(nodeId, remotePath, name) {
  await ensureConnectionKey();
  const r = await fetch('/api/transfer/pull', {
    method: 'POST',
    headers: { ...authHeaders(), 'Content-Type': 'application/json' },
    body: JSON.stringify({ node_id: nodeId, remote_path: remotePath, name }),
  });
  const d = await r.json().catch(() => ({}));
  if (!r.ok) alert('Não deu para baixar: ' + (d.error || r.status));
  pollTransfers();
}

async function sendSlice(nodeId, path) {
  await ensureConnectionKey();
  const r = await fetch('/api/transfer/send', {
    method: 'POST',
    headers: { ...authHeaders(), 'Content-Type': 'application/json' },
    body: JSON.stringify({ to_node_id: nodeId, path }),
  });
  const d = await r.json().catch(() => ({}));
  alert(r.ok
    ? 'Oferta enviada. O outro nó precisa aceitar no sino de notificações.'
    : 'Não deu para oferecer: ' + (d.error || r.status));
}

/** Envio por caminho digitado — o caso "está na máquina, tenho que poder mandar".
 *
 *  A lista de fatias só enxerga o que a varredura de bibliotecas achou; um GGUF
 *  solto numa pasta qualquer não aparece em lugar nenhum, e era exatamente o que
 *  não dava para transferir. Aqui o caminho vem do dono da máquina.
 */
async function renderSendPathPeers() {
  const sel = document.getElementById('send-path-peer');
  if (!sel) return;
  // Buscar o cluster aqui em vez de esperar `state.cluster`: aquele campo só é
  // preenchido pelo poller do painel, então abrir a aba Nós antes do primeiro
  // ciclo deixava a lista de destinos vazia.
  if (!state.cluster) {
    try {
      state.cluster = await getJSON('/api/ui/cluster');
    } catch (e) {
      return;
    }
  }
  // `is_self` é a marca que o cluster já traz; comparar node_id com um campo
  // que não existe deixava o próprio nó na lista de destinos.
  const peers = (state.cluster?.peers || []).filter((p) => !p.is_self);
  const anterior = sel.value;
  // Dois peers na mesma máquina compartilham o hostname, então o nome sozinho
  // não distingue um do outro — a faixa de camadas distingue.
  const rotulo = (p) => {
    const nome = peerName(p.node_id);
    const faixa = p.start_layer != null && p.end_layer != null
      ? ` · camadas ${p.start_layer}–${p.end_layer}`
      : '';
    return `${nome}${faixa}`;
  };
  sel.innerHTML = peers.length
    ? peers.map((p) => `<option value="${esc(p.node_id)}">${esc(rotulo(p))}</option>`).join('')
    : '<option value="">nenhuma outra máquina no cluster</option>';
  if (anterior && peers.some((p) => p.node_id === anterior)) sel.value = anterior;
  sel.disabled = !peers.length;
}

document.getElementById('send-path-form')?.addEventListener('submit', async (ev) => {
  ev.preventDefault();
  const msg = document.getElementById('send-path-msg');
  const caminho = document.getElementById('send-path').value.trim();
  const destino = document.getElementById('send-path-peer').value;
  if (!caminho || !destino) {
    msg.className = 'notice notice-error';
    msg.textContent = 'Informe o caminho do arquivo e a máquina de destino.';
    return;
  }

  msg.className = 'muted-note';
  msg.textContent = 'Calculando o MD5 do arquivo…';
  await ensureConnectionKey();
  const r = await fetch('/api/transfer/send', {
    method: 'POST',
    headers: { ...authHeaders(), 'Content-Type': 'application/json' },
    body: JSON.stringify({ to_node_id: destino, path: caminho }),
  });
  const d = await r.json().catch(() => ({}));
  msg.className = r.ok ? 'notice notice-ok' : 'notice notice-error';
  msg.textContent = r.ok
    ? 'Oferta enviada. O outro nó precisa aceitar no sino de notificações.'
    : 'Não deu para oferecer: ' + (d.error || r.status);
  if (r.ok) document.getElementById('send-path').value = '';
  pollTransfers();
});

setInterval(pollTransfers, 1000);
pollTransfers();

async function loadNodesTab() {
  const view = document.getElementById('view-nos');
  if (!view || !view.classList.contains('is-active')) return;

  try {
    // The connection key comes from this node itself, and every /admin call
    // needs it. Fetch it first, or the very first tab open sends an empty
    // bearer token and every panel stays stuck on "Carregando...".
    const localNodeR = await fetch('/api/ui/node');
    if (localNodeR.ok) {
      const node = await localNodeR.json();
      const connKey = node.connection_key || '';
      document.getElementById('display-conn-key').textContent = connKey || '***';
      state.connectionKey = connKey;
    }

    const nodesR = await fetch('/admin/nodes', { headers: authHeaders() });
    const nodesEl = document.getElementById('nodes-table');
    if (nodesR.ok) {
      const nodes = await nodesR.json();
      nodesEl.innerHTML = nodes.length
        ? nodes.map(renderNodeRow).join('')
        : '<p class="muted-note">Nenhum no conhecido ainda.</p>';
    } else {
      nodesEl.innerHTML =
        `<p class="notice notice-warn">Nao foi possivel listar os nos (HTTP ${nodesR.status}).</p>`;
    }

    // As fatias de cada máquina vêm do mesmo catálogo do cluster que a aba
    // Configurar usa.
    try {
      const cm = await getJSON('/api/ui/cluster/models');
      renderSliceExchange(cm);
    } catch (e) {}

    await renderSendPathPeers();

    const keysR = await fetch('/admin/api-keys', { headers: authHeaders() });

    if (keysR.ok) {
      const keys = await keysR.json();
      document.getElementById('api-keys-list').innerHTML = keys.map(k => `
        <div class="node-row">
          <div class="node-row-main">
            <strong>${esc(k.label)}</strong>
            <span class="node-row-sub">${esc((k.key || '').substring(0, 24))}…</span>
          </div>
          <div class="tagrow">${k.active
            ? '<span class="tag is-ready">ativa</span>'
            : '<span class="tag">inativa</span>'}</div>
          <div class="node-row-actions">
            <button class="btn-slim" onclick="revokeApiKey('${esc(k.key)}')">Revogar</button>
          </div>
        </div>
      `).join('') || '<p class="muted-note">Nenhuma chave</p>';
    }

  } catch(e) {
    console.error('Failed to load nodes tab', e);
  }
}

/** Uma linha da tabela "Nós conectados". */
function renderNodeRow(n) {
  const name = n.hostname ? shortHost(n.hostname) : (n.short_id || n.node_id);
  const tags = [];
  if (n.is_self) tags.push('<span class="tag is-primary">este nó</span>');
  if (n.role) tags.push(`<span class="tag">${esc(n.role)}</span>`);
  if (n.blocked) tags.push('<span class="tag is-failed">bloqueado</span>');
  if (!n.connected) tags.push('<span class="tag">desconectado</span>');
  const layers = n.total_layers > 0
    ? `camadas ${n.start_layer}–${n.end_layer}`
    : 'sem camadas atribuídas';

  const actions = n.is_self
    ? ''
    : n.blocked
      ? `<button class="btn-slim" onclick="unblockNode('${esc(n.node_id)}')">Desbloquear</button>`
      : `<button class="btn-slim" onclick="blockNode('${esc(n.node_id)}')">Bloquear</button>`;

  return `
    <div class="node-row">
      <div class="node-row-main">
        <strong>${esc(name)}</strong>
        <span class="node-row-sub">${esc(n.addr || '—')} · ${layers}</span>
      </div>
      <div class="tagrow">${tags.join('')}</div>
      <div class="node-row-actions">${actions}</div>
    </div>`;
}

async function revokeApiKey(key) {
  await fetch('/admin/api-keys/' + key, { method: 'DELETE', headers: authHeaders() });
  loadNodesTab();
}

async function blockNode(id) {
  await fetch('/admin/nodes/' + id + '/block', { method: 'POST', headers: authHeaders() });
  loadNodesTab();
}

async function unblockNode(id) {
  await fetch('/admin/nodes/' + id + '/unblock', { method: 'POST', headers: authHeaders() });
  loadNodesTab();
}

async function removeNode(id) {
  await fetch('/admin/nodes/' + id, { method: 'DELETE', headers: authHeaders() });
  loadNodesTab();
}

function authHeaders() {
  return { 'Authorization': 'Bearer ' + (state.connectionKey || '') };
}

document.getElementById('notif-bell')?.addEventListener('click', () => {
  document.getElementById('notif-dropdown').classList.toggle('hidden');
});

pollNotifications();
setInterval(pollNotifications, 5000);

/** Copia o conteudo de um campo e confirma na propria tecla. */
document.addEventListener('click', async (ev) => {
  const btn = ev.target.closest('[data-copy]');
  if (!btn) return;
  const field = document.getElementById(btn.dataset.copy);
  if (!field) return;
  const value = field.value || '';
  try {
    await navigator.clipboard.writeText(value);
  } catch (e) {
    // clipboard bloqueado (http em host remoto): a selecao ainda resolve
    field.select();
    document.execCommand('copy');
  }
  const before = btn.textContent;
  btn.textContent = 'Copiado';
  btn.classList.add('is-done');
  setTimeout(() => { btn.textContent = before; btn.classList.remove('is-done'); }, 1500);
});

document.getElementById('btn-regenerate-key')?.addEventListener('click', async () => {
  if (!confirm('Gerar uma chave nova invalida a atual: todos os nos terao de ser reconfigurados com ela. Continuar?')) return;
  await ensureConnectionKey();
  const r = await fetch('/admin/regenerate-key', { method: 'POST', headers: authHeaders() });
  const d = await r.json();
  if (d.connection_key) {
    document.getElementById('display-conn-key').value = d.connection_key;
    state.connectionKey = d.connection_key;
  }
});

document.getElementById('btn-generate-api-key')?.addEventListener('click', async () => {
  const label = prompt('Rotulo para a chave (ex: opencode):', 'default');
  if (!label) return;
  await ensureConnectionKey();
  const r = await fetch('/admin/api-keys', {
    method: 'POST',
    headers: { ...authHeaders(), 'Content-Type': 'application/json' },
    body: JSON.stringify({ label })
  });
  const d = await r.json();
  if (!r.ok || !d.key) {
    alert('Nao foi possivel gerar a chave: ' + (d.error || r.status));
    return;
  }
  // Mostrada uma unica vez, num campo selecionavel - um alert() nao deixa copiar.
  const box = document.getElementById('new-api-key');
  document.getElementById('new-api-key-value').value = d.key;
  box.hidden = false;
  loadNodesTab();
});
