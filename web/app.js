/* rdma-p2p — painel de operação.
   Sem framework e sem build: o binário serve este arquivo direto.

   Regra de cor: cada peer recebe um índice estável (ordenado por camada
   inicial) e esse índice vira a hue usada na fita, no cartão e nos medidores.
   A cor sempre quer dizer "esta máquina", nunca outra coisa. */

const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];

const PEER_HUES = 5;
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

async function postJSON(url, body) {
  const r = await fetch(url, {
    method: "POST",
    headers: { "content-type": "application/json" },
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
    // Update role badge
    const roleBadge = document.getElementById('role-badge');
    if (roleBadge && cluster.node) {
      const role = cluster.node.role || 'node';
      roleBadge.textContent = role.charAt(0).toUpperCase() + role.slice(1);
      roleBadge.dataset.role = role.toLowerCase();
      state.role = role.toLowerCase();
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
      <p class="peer-sub">${esc(peer.addr)} · ${esc(sys?.os || "?")}/${esc(sys?.arch || "?")} · camadas ${peer.start_layer}–${peer.end_layer}</p>
      <div class="tagrow">${tags.join("")}</div>

      ${meter("RAM", ram?.used_mb, ram?.total_mb, ram?.used_pct)}

      ${gpuSection(sys, gpu)}

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

function renderConnectInfo(cluster) {
  $("#base-url").textContent = `${location.origin}/v1`;
  $("#base-model").textContent = cluster.active_model || "—";
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
  } catch (e) {
    $("#model-table tbody").innerHTML =
      `<tr><td colspan="7" class="empty">Falha ao ler as bibliotecas: ${esc(e.message)}</td></tr>`;
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
      const name = esc(peerName(p.node_id));
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
    const resp = await fetch("/v1/chat/completions", {
      method: "POST",
      headers: { "content-type": "application/json" },
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
    const r = await fetch('/admin/transfer-offers', { headers: authHeaders() });
    if (!r.ok) return;
    const data = await r.json();
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
    dropdown.innerHTML = offers.map(o => `
      <div class="notif-item">
        <strong>Transferencia pendente</strong><br>
        ${o.shard_name} (${o.total_size_mb} MB)<br>
        Camadas ${o.start_layer}-${o.end_layer}<br>
        Espaco disponivel: ${o.disk_free_mb} MB
        <div class="notif-actions">
          <button onclick="respondTransfer('${o.id}', true)">Aceitar</button>
          <button onclick="respondTransfer('${o.id}', false)" style="background:#999">Recusar</button>
        </div>
      </div>
    `).join('');
  } else {
    count.classList.add('hidden');
    dropdown.innerHTML = '<div class="notif-item" style="color:var(--muted)">Nenhuma notificacao</div>';
  }
}

async function respondTransfer(id, accept) {
  try {
    await fetch('/admin/transfer-offer/' + id, {
      method: 'POST',
      headers: { ...authHeaders(), 'Content-Type': 'application/json' },
      body: JSON.stringify({ accept })
    });
    pollNotifications();
  } catch(e) {}
}

function authHeaders() {
  return { 'Authorization': 'Bearer ' + (state.connectionKey || '') };
}

document.getElementById('notif-bell')?.addEventListener('click', () => {
  document.getElementById('notif-dropdown').classList.toggle('hidden');
});

pollNotifications();
setInterval(pollNotifications, 5000);
