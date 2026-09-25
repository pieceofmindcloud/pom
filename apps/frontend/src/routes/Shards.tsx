/**
 * The slices on this machine, and who else in the network is missing them.
 *
 * Every local slice is listed, always — the panel on the Nodes screen is a
 * *difference* list and goes empty when both machines already agree, which is
 * exactly when it looks like sending a slice is impossible. Here the slice is
 * always there and the destination picker is the one that narrows.
 */
import { Layers, PackageOpen, RefreshCw, Trash2 } from "lucide-react";
import { useState } from "react";
import {
  Badge,
  Button,
  Empty,
  ErrorState,
  Footnote,
  FullBleedHeader,
  Panel,
  PanelHeader,
  Select,
  Skeleton,
} from "../components/ui";
import { api } from "../lib/api";
import { bytes, layerRange, num, optionalBasename } from "../lib/format";
import { deleteModel } from "../lib/deleteModel";
import { uniquePeerName } from "../lib/peerName";
import { useClusterCatalog, useCluster, useLocalCatalog } from "../lib/queries";
import { useToast } from "../lib/toast";
import { tpPartsOf, type DiscoveredModel, type PeerCatalog, type ShardSet } from "../lib/types";
import { useI18n } from "../i18n/provider";

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

function PartRows({
  parts,
  tensorParallel,
  heading,
  peersMissing,
  aloneInNetwork,
  onSend,
  onDelete,
}: {
  parts: DiscoveredModel[];
  tensorParallel: boolean;
  heading: string;
  peersMissing: (shard: DiscoveredModel) => PeerCatalog[];
  aloneInNetwork: boolean;
  onSend: (shard: DiscoveredModel, nodeId: string) => void;
  onDelete: (shard: DiscoveredModel) => void;
}) {
  const { t } = useI18n();
  if (parts.length === 0) return null;
  return (
    <section>
      <div className="border-t border-line bg-raised/50 px-5 py-2 text-[0.7rem] font-medium uppercase tracking-[0.12em] text-muted">
        {heading}
      </div>
      <div className="divide-y divide-line">
        {parts.map((s) => {
          const missing = peersMissing(s);
          const tensorLabel = tensorParallel && s.tp_rank != null
            ? `${s.tp_rank + 1}of${s.tp_size ?? "?"}`
            : null;
          const label = tensorLabel ?? (s.shard_range
            ? layerRange(s.shard_range[0], s.shard_range[1])
            : t("shards.unknownRange"));
          return (
            <div
              key={s.id}
              className="flex flex-wrap items-center gap-x-4 gap-y-2 px-5 py-3"
            >
              <div className="min-w-[12rem] flex-1">
                <div className="flex min-w-0 items-center gap-2">
                  <span className="shrink-0 font-mono text-sm text-iris">
                    {label}
                    {tensorLabel && <span className="ml-2 text-[0.7rem] text-faint">rank {s.tp_rank}</span>}
                  </span>
                  <span className="truncate text-[0.82rem] text-fg">{s.name}</span>
                  {s.sha256 && (
                    <span
                      className="shrink-0 font-mono text-[0.7rem] text-faint"
                      title={s.sha256}
                    >
                      {s.sha256.slice(0, 12)}
                    </span>
                  )}
                </div>
                <p className="break-path mt-0.5 font-mono text-[0.7rem] text-faint">
                  {s.path}
                </p>
              </div>
              <span className="shrink-0 font-mono text-[0.82rem] text-muted">
                {bytes(s.size_bytes)}
              </span>
              <Button
                size="sm"
                variant="ghost"
                icon={<Trash2 className="h-3.5 w-3.5" />}
                aria-label={t("models.deleteAria", { name: s.name })}
                onClick={() => onDelete(s)}
              >
                {t("models.deleteButton")}
              </Button>
              <div className="flex shrink-0 items-center gap-2">
                {missing.length === 0 ? (
                  <span className="text-[0.78rem] text-faint">
                    {aloneInNetwork ? t("shards.noOtherMachine") : t("shards.everyoneHas")}
                  </span>
                ) : (
                  <Select
                    className="w-52"
                    value=""
                    aria-label={t("shards.sendAria", { name: s.name })}
                    onChange={(e) => e.target.value && onSend(s, e.target.value)}
                  >
                    <option value="">{t("shards.sendTo")}</option>
                    {missing.map((p) => (
                      <option key={p.node_id} value={p.node_id}>
                        {uniquePeerName(p, missing)}
                      </option>
                    ))}
                  </Select>
                )}
              </div>
            </div>
          );
        })}
      </div>
    </section>
  );
}

function ShardCard({
  set,
  peersMissing,
  aloneInNetwork,
  onSend,
  onDelete,
}: {
  set: ShardSet;
  peersMissing: (shard: DiscoveredModel) => PeerCatalog[];
  /** No other machine is on the network at all — different from "everyone
      already has this slice", and the label has to say so. */
  aloneInNetwork: boolean;
  onSend: (shard: DiscoveredModel, nodeId: string) => void;
  onDelete: (shard: DiscoveredModel) => void;
}) {
  const { t } = useI18n();
  const tpParts = tpPartsOf(set);
  const tpGroups = Array.from(
    tpParts.reduce((groups, part) => {
      const key = part.tp_size ?? 0;
      const group = groups.get(key) ?? [];
      group.push(part);
      groups.set(key, group);
      return groups;
    }, new Map<number, DiscoveredModel[]>()),
  ).sort(([a], [b]) => a - b);
  const mmprojName = optionalBasename(set.mmproj_path);
  return (
    <Panel>
      <PanelHeader
        eyebrow={t("shards.set")}
        title={set.model_label || set.group}
        subtitle={
          set.total_layers != null
            ? t("shards.setDetail", { count: set.shards.length + tpParts.length, total: num(set.total_layers) })
            : t("shards.setUnknown", { count: set.shards.length + tpParts.length })
        }
        actions={
          <Badge tone={set.complete ? "aqua" : "amber"}>
            {set.complete ? t("shards.complete") : t("shards.incomplete")}
          </Badge>
        }
      />
      <div className="flex flex-wrap items-center gap-1.5 border-b border-line px-5 py-2.5 text-[0.78rem]">
        <span className="text-faint">{t("models.mmproj")}:</span>
        {mmprojName ? (
          <span className="break-path font-mono text-fg" title={set.mmproj_path ?? undefined}>
            {mmprojName}
          </span>
        ) : (
          <span className="text-faint">{t("models.mmprojNone")}</span>
        )}
      </div>
      {set.shards.length > 0 && (
        <div className="px-5 py-3">
          <CoverageBar set={set} />
        </div>
      )}
      <PartRows
        parts={set.shards}
        tensorParallel={false}
        heading={t("shards.parts")}
        peersMissing={peersMissing}
        aloneInNetwork={aloneInNetwork}
        onSend={onSend}
        onDelete={onDelete}
      />
      {tpGroups.map(([size, parts]) => (
        <PartRows
          key={size}
          parts={parts}
          tensorParallel
          heading={size > 0 ? t("shards.tpGroup", { size }) : t("shards.tpParts")}
          peersMissing={peersMissing}
          aloneInNetwork={aloneInNetwork}
          onSend={onSend}
          onDelete={onDelete}
        />
      ))}
    </Panel>
  );
}

export function Shards() {
  const { t } = useI18n();
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
      const known = [
        ...(p.models ?? []),
        ...(p.shard_sets ?? []).flatMap((s) => [...s.shards, ...tpPartsOf(s)]),
      ];
      return !known.some((m) => m.name === shard.name);
    });

  async function remove(shard: DiscoveredModel) {
    try {
      const out = await deleteModel(shard.path, shard.name, (m) => window.confirm(m));
      if (out.status === "deleted") {
        toast.success(t("models.deleted", { name: shard.name, bytes: bytes(out.freedBytes) }));
        void catalog.refetch();
      }
    } catch (e) {
      toast.fromError(t("models.deleteFailed"), e);
    }
  }

  async function send(shard: DiscoveredModel, nodeId: string) {
    setSending(shard.id);
    try {
      const filename = shard.path.split(/[\\/]/).pop() ?? shard.name;
      await api.post("/api/transfer/send", { to_node_id: nodeId, filename });
      toast.success(
        t("shards.offerSent"),
        t("shards.offerSentDetail"),
      );
    } catch (e) {
      toast.fromError(t("shards.offerFailed"), e);
    } finally {
      setSending(null);
    }
  }

  return (
    <div className="flex h-full min-h-0 min-w-0 flex-col bg-ink">
      <FullBleedHeader
        title={t("shards.title")}
        subtitle={selfId ? t("shards.subtitle") : undefined}
        actions={
          <Button
            icon={<RefreshCw className="h-3.5 w-3.5" />}
            loading={catalog.isFetching || clusterCatalog.isFetching}
            onClick={() => {
              void catalog.refetch();
              void clusterCatalog.refetch();
            }}
          >
            {t("shards.rescan")}
          </Button>
        }
      />

      <div className="min-h-0 min-w-0 flex-1 overflow-auto p-4 sm:p-5">
        {catalog.isError && (
          <ErrorState error={catalog.error} onRetry={() => void catalog.refetch()} />
        )}
        {catalog.isLoading && <Skeleton className="h-56 w-full" />}

        {catalog.data && sets.length === 0 && (
          <Panel>
            <Empty
              icon={<PackageOpen className="h-5 w-5" />}
              title={t("shards.empty")}
            >
              {t("shards.emptyDetail")}
            </Empty>
          </Panel>
        )}

        <div className="space-y-5">
          {sets.map((set) => (
            <ShardCard
              key={set.group}
              set={set}
              peersMissing={peersMissing}
              aloneInNetwork={others.length === 0}
              onSend={(shard, nodeId) => void send(shard, nodeId)}
              onDelete={(shard) => void remove(shard)}
            />
          ))}
        </div>
      </div>

      {sets.length > 0 && (
        <Footnote
          className="shrink-0 border-t border-line px-4 py-2.5"
          icon={<Layers className="h-3.5 w-3.5" />}
        >
          {sending
            ? t("shards.sending")
            : t("shards.offerDetail")}
        </Footnote>
      )}
    </div>
  );
}
