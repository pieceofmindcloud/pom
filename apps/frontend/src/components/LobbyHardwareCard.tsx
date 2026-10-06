import { Cpu, MemoryStick, Monitor, Waypoints } from "lucide-react";
import { useI18n } from "../i18n/provider";
import type { LobbyCard } from "../lib/types";

function gigabytes(mb: number | null | undefined): string | null {
  if (typeof mb !== "number" || !Number.isFinite(mb) || mb <= 0) return null;
  return `${(mb / 1024).toFixed(mb >= 10 * 1024 ? 0 : 1)} GB`;
}

/**
 * The hardware a lobby candidate declares: the same facts the Dashboard card
 * opens with (machine, CPU, memory, GPUs), without live usage - the candidate
 * is not a member, so nobody is measuring it. The values are self-declared.
 */
export function LobbyHardwareCard({ nodeId, card, self = false }: { nodeId: string; card: LobbyCard; self?: boolean }) {
  const { t } = useI18n();
  const unknown = t("lobby.unknown");
  const platform = [card.os, card.arch].filter(Boolean).join(" / ") || unknown;
  return (
    <div className="panel min-w-0 overflow-hidden" data-testid={`lobby-card-${nodeId}`}>
      <div className="flex items-start gap-2 border-b border-line px-3 py-2.5">
        <div className="mt-0.5 flex h-7 w-7 shrink-0 items-center justify-center rounded-lg border border-line bg-raised text-muted">
          <Monitor className="h-3.5 w-3.5" />
        </div>
        <div className="min-w-0">
          <h3 className="truncate font-display text-[0.86rem] font-semibold text-fg">
            {card.hostname || nodeId.slice(0, 12)}
            {self && <span className="ml-1.5 text-[0.62rem] font-normal text-faint">{t("lobby.you")}</span>}
          </h3>
          <p className="truncate text-[0.66rem] text-muted">{platform}</p>
          <p className="truncate font-mono text-[0.6rem] text-faint">{nodeId.slice(0, 12)}</p>
        </div>
      </div>
      <dl className="space-y-1.5 px-3 py-2.5 text-[0.72rem]">
        <div className="flex items-center gap-2 text-muted">
          <Cpu className="h-3.5 w-3.5 shrink-0 text-faint" />
          <dt className="sr-only">{t("lobby.cpu")}</dt>
          <dd>{card.cpu_count > 0 ? t("lobby.cpuCount", { count: card.cpu_count }) : unknown}</dd>
        </div>
        <div className="flex items-center gap-2 text-muted">
          <MemoryStick className="h-3.5 w-3.5 shrink-0 text-faint" />
          <dt className="sr-only">{t("lobby.ram")}</dt>
          <dd>{gigabytes(card.ram_total_mb) ? t("lobby.ramTotal", { value: gigabytes(card.ram_total_mb) ?? "" }) : unknown}</dd>
        </div>
        {(card.relayed != null || card.rtt_ms != null) && (
          <div className="flex items-center gap-2 text-muted" data-testid={`lobby-link-${nodeId}`}>
            <Waypoints className="h-3.5 w-3.5 shrink-0 text-faint" />
            <dt className="sr-only">{t("lobby.connection")}</dt>
            <dd>
              {[
                card.relayed == null ? null : t(card.relayed ? "lobby.connectionRelayed" : "lobby.connectionDirect"),
                card.rtt_ms == null ? null : t("lobby.latency", { value: card.rtt_ms }),
              ].filter(Boolean).join(" - ")}
            </dd>
          </div>
        )}
        {card.gpus.length === 0 ? (
          <p className="text-faint">{t("lobby.noGpu")}</p>
        ) : card.gpus.map((gpu, index) => (
          <div key={`${gpu.name}-${index}`} className="flex min-w-0 items-baseline justify-between gap-2 rounded-md border border-line bg-raised/50 px-2 py-1">
            <span className="min-w-0 truncate text-fg">{gpu.name || unknown}</span>
            <span className="shrink-0 font-mono text-[0.66rem] text-muted">{gigabytes(gpu.vram_total_mb) ?? t("lobby.unifiedMemory")}</span>
          </div>
        ))}
      </dl>
    </div>
  );
}
