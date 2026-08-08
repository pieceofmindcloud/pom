/**
 * The slices on this machine, and who else in the network is missing them.
 *
 * Every local slice is listed, always — the panel on the Nodes screen is a
 * *difference* list and goes empty when both machines already agree, which is
 * exactly when it looks like sending a slice is impossible. Here the slice is
 * always there and the destination picker is the one that narrows.
 */
import { Layers, PackageOpen, RefreshCw } from "lucide-react";
import { useState } from "react";
import {
  Badge,
  Button,
  Empty,
  ErrorState,
  Footnote,
  PageHeader,
  Panel,
  PanelHeader,
  Select,
  Skeleton,
} from "../components/ui";
import { api } from "../lib/api";
import { bytes, layerRange, num } from "../lib/format";
import { useClusterCatalog, useCluster, useLocalCatalog } from "../lib/queries";
import { useToast } from "../lib/toast";
import type { DiscoveredModel, PeerCatalog, ShardSet } from "../lib/types";

/** Peers of the same machine share a hostname; the short id disambiguates. */
function uniqueName(peer: { hostname?: string | null; node_id: string }, all: PeerCatalog[]) {
  const name = peer.hostname || peer.node_id.slice(0, 12);
  const repeated = all.filter((p) => (p.hostname || p.node_id.slice(0, 12)) === name);
  return repeated.length > 1 ? `${name} · ${peer.node_id.slice(0, 6)}` : name;
}

function CoverageBar({ set }: { set: ShardSet }) {
  const total = set.total_layers ?? 0;
  if (total <= 0) return null;
  const owned = new Array<boolean>(total).fill(false);
  for (const s of set.shards) {
    if (!s.shard_range) continue;
    for (let l = s.shard_range[0]; l <= Math.min(s.shard_range[1], total - 1); l++) {
      owned[l] = true;
    }
  }
  return (
    <div className="flex h-2.5 gap-[2px]" aria-hidden="true">
      {owned.map((o, i) => (
        <span
          key={i}
          className={`min-w-0 flex-1 rounded-[1px] ${o ? "bg-iris/80" : "bg-line"}`}
        />
      ))}
    </div>
  );
}

function ShardCard({
  set,
  peersMissing,
  onSend,
}: {
  set: ShardSet;
  peersMissing: (shard: DiscoveredModel) => PeerCatalog[];
  onSend: (shard: DiscoveredModel, nodeId: string) => void;
}) {
  return (
    <Panel>
      <PanelHeader
        eyebrow="Conjunto de fatias"
        title={set.group}
        subtitle={
          set.total_layers != null
            ? `${set.shards.length} ${set.shards.length === 1 ? "fatia" : "fatias"} de um modelo de ${num(set.total_layers)} camadas${
                set.total_layers_inferred
                  ? " (contagem deduzida das próprias fatias — estas não trazem o metadado rdmap2p)"
                  : ""
              }`
            : `${set.shards.length} fatias`
        }
        actions={
          <Badge tone={set.complete ? "aqua" : "amber"}>
            {set.complete ? "cobre o modelo" : "cobertura incompleta"}
          </Badge>
        }
      />
      <div className="px-5 py-3">
        <CoverageBar set={set} />
      </div>
      <div className="divide-y divide-line border-t border-line">
        {set.shards.map((s) => {
          const missing = peersMissing(s);
          return (
            <div
              key={s.id}
              className="flex flex-wrap items-center gap-x-4 gap-y-2 px-5 py-3"
            >
              <div className="min-w-[12rem] flex-1">
                <div className="flex items-center gap-2">
                  <span className="font-mono text-sm text-iris">
                    {s.shard_range
                      ? layerRange(s.shard_range[0], s.shard_range[1])
                      : "faixa desconhecida"}
                  </span>
                  <span className="truncate text-[0.82rem] text-fg">{s.name}</span>
                </div>
                <p className="break-path mt-0.5 font-mono text-[0.7rem] text-faint">
                  {s.path}
                </p>
              </div>
              <span className="shrink-0 font-mono text-[0.82rem] text-muted">
                {bytes(s.size_bytes)}
              </span>
              <div className="flex shrink-0 items-center gap-2">
                {missing.length === 0 ? (
                  <span className="text-[0.78rem] text-faint">todas já têm</span>
                ) : (
                  <Select
                    className="w-52"
                    value=""
                    aria-label={`Enviar ${s.name} para`}
                    onChange={(e) => e.target.value && onSend(s, e.target.value)}
                  >
                    <option value="">Enviar para…</option>
                    {missing.map((p) => (
                      <option key={p.node_id} value={p.node_id}>
                        {uniqueName(p, missing)}
                      </option>
                    ))}
                  </Select>
                )}
              </div>
            </div>
          );
        })}
      </div>
    </Panel>
  );
}

export function Shards() {
  const catalog = useLocalCatalog();
  const cluster = useCluster();
  const clusterCatalog = useClusterCatalog();
  const toast = useToast();
  const [sending, setSending] = useState<string | null>(null);

  const sets = catalog.data?.shard_sets ?? [];
  const selfId = cluster.data?.self_id;
  const others = (clusterCatalog.data?.peers ?? []).filter((p) => !p.is_self);

  /** A peer is missing a slice when nothing it reports has that filename. */
  const peersMissing = (shard: DiscoveredModel) =>
    others.filter((p) => {
      const known = [...(p.models ?? []), ...(p.shard_sets ?? []).flatMap((s) => s.shards)];
      return !known.some((m) => m.name === shard.name);
    });

  async function send(shard: DiscoveredModel, nodeId: string) {
    setSending(shard.id);
    try {
      await api.post("/api/transfer/send", { to_node_id: nodeId, path: shard.path });
      toast.success(
        "Oferta enviada",
        "O outro lado precisa aceitar antes de a transferência começar.",
      );
    } catch (e) {
      toast.fromError("A oferta não saiu", e);
    } finally {
      setSending(null);
    }
  }

  return (
    <>
      <PageHeader
        title="Fatias nesta máquina"
        subtitle={
          selfId
            ? "Toda fatia local aparece aqui, mesmo quando todo mundo já tem uma cópia. O destino é que se restringe a quem ainda não tem."
            : undefined
        }
        actions={
          <Button
            icon={<RefreshCw className="h-3.5 w-3.5" />}
            loading={catalog.isFetching || clusterCatalog.isFetching}
            onClick={() => {
              void catalog.refetch();
              void clusterCatalog.refetch();
            }}
          >
            Varrer de novo
          </Button>
        }
      />

      {catalog.isError && (
        <ErrorState error={catalog.error} onRetry={() => void catalog.refetch()} />
      )}
      {catalog.isLoading && <Skeleton className="h-56 w-full" />}

      {catalog.data && sets.length === 0 && (
        <Panel>
          <Empty
            icon={<PackageOpen className="h-5 w-5" />}
            title="Nenhuma fatia nesta máquina"
          >
            Fatias aparecem aqui depois de um fatiamento local ou de uma transferência
            aceita. Elas são gravadas na pasta de fatias configurada.
          </Empty>
        </Panel>
      )}

      <div className="space-y-5">
        {sets.map((set) => (
          <ShardCard
            key={set.group}
            set={set}
            peersMissing={peersMissing}
            onSend={(shard, nodeId) => void send(shard, nodeId)}
          />
        ))}
      </div>

      {sets.length > 0 && (
        <Footnote
          className="mt-4"
          icon={<Layers className="h-3.5 w-3.5" />}
        >
          {sending
            ? "Enviando oferta…"
            : "Enviar cria uma oferta: o dono da outra máquina aceita, e só então os bytes saem daqui."}
        </Footnote>
      )}
    </>
  );
}
