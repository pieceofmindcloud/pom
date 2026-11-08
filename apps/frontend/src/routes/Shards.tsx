/**
 * The slices on this machine, kept for slice sets made before range loading
 * and for staged services. Files are no longer sent between machines, so
 * the screen lists and deletes them only.
 */
import { PackageOpen, RefreshCw, Trash2 } from "lucide-react";
import {
  Badge,
  Button,
  Empty,
  ErrorState,
  FullBleedHeader,
  Panel,
  PanelHeader,
  Skeleton,
} from "../components/ui";
import { bytes, layerRange, num } from "../lib/format";
import { deleteModel } from "../lib/deleteModel";
import { useClusterCatalog, useCluster, useLocalCatalog } from "../lib/queries";
import { useToast } from "../lib/toast";
import { tpPartsOf, type DiscoveredModel, type PeerCatalog, type ShardSet } from "../lib/types";
import { useI18n } from "../i18n/provider";
import { SidecarLinkEditor } from "../components/SidecarLinkEditor";

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
  onDelete,
}: {
  parts: DiscoveredModel[];
  tensorParallel: boolean;
  heading: string;
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
            </div>
          );
        })}
      </div>
    </section>
  );
}

function ShardCard({
  set,
  peers,
  onDelete,
}: {
  set: ShardSet;
  /** Every machine's catalog: where the sidecars that can be linked live. */
  peers: PeerCatalog[] | undefined;
  /** No other machine is on the network at all — different from "everyone
      already has this slice", and the label has to say so. */
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
  const shape = set.shards.find((shard) => shard.embedding_length != null) ?? set.shards[0];
  const embeddedMtp = set.shards.some((shard) => shard.capabilities?.mtp === true);
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
      <div className="border-b border-line px-5 py-3">
        <SidecarLinkEditor
          modelKey={set.model_label || set.group}
          model={shape}
          embeddedMtp={embeddedMtp}
          peers={peers}
        />
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
        onDelete={onDelete}
      />
      {tpGroups.map(([size, parts]) => (
        <PartRows
          key={size}
          parts={parts}
          tensorParallel
          heading={size > 0 ? t("shards.tpGroup", { size }) : t("shards.tpParts")}
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

  const sets = catalog.data?.shard_sets ?? [];
  const selfId = cluster.data?.self_id;

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
              peers={clusterCatalog.data?.peers}
              onDelete={(shard) => void remove(shard)}
            />
          ))}
        </div>
      </div>

    </div>
  );
}
