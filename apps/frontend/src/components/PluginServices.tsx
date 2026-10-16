/**
 * What a machine's plugins serve to the network, drawn alongside it.
 *
 * A plugin is local to the node, but what it runs may belong to everyone: one
 * machine's Qdrant is the RAG database of the whole pipeline. The node publishes these
 * services in `node.plugin_services`, and here they become badges - one per
 * service, with the color of the state.
 *
 * Two places use the same badge: the machine's details card (`inline`) and the
 * Flow, where `PluginServiceTether` hangs them under the machine's card with a
 * connecting line, so the screen says **where** the service runs.
 */
import clsx from "clsx";
import { Database, Puzzle } from "lucide-react";
import type { CanvasPosition, CardSize } from "../lib/networkLayout";
import type { PluginService } from "../lib/types";
import { useI18n } from "../i18n/provider";

const TONE: Record<PluginService["status"], string> = {
  ready: "border-aqua/50 bg-aqua/15 text-aqua",
  starting: "border-sky/50 bg-sky/15 text-sky",
  installing: "border-sky/50 bg-sky/15 text-sky",
  waiting: "border-amber/50 bg-amber/15 text-amber",
  error: "border-rose/50 bg-rose/15 text-rose",
};

const LINE: Record<PluginService["status"], string> = {
  ready: "bg-aqua",
  starting: "bg-sky",
  installing: "bg-sky",
  waiting: "bg-amber",
  error: "bg-rose",
};

/** Distance between the bottom edge of the card and the badges, in the Flow. */
export const SERVICE_TETHER_GAP = 8;

export function PluginServiceBadge({ service }: { service: PluginService }) {
  const { t } = useI18n();
  const status = {
    ready: t("dashboard.serviceStatusReady"),
    starting: t("dashboard.serviceStatusStarting"),
    installing: t("dashboard.serviceStatusInstalling"),
    waiting: t("dashboard.serviceStatusWaiting"),
    error: t("dashboard.serviceStatusError"),
  }[service.status];
  // A type that this version of the screen does not know shows up as the plugin called it.
  const kind = service.kind === "rag" ? t("dashboard.serviceKindRag") : service.kind;
  const Icon = service.kind === "rag" ? Database : Puzzle;
  const title = [`${service.name} · ${kind}`, status, service.version, service.detail]
    .filter(Boolean)
    .join(" · ");
  return (
    <span
      data-testid="plugin-service"
      data-status={service.status}
      title={title}
      aria-label={title}
      className={clsx(
        "inline-flex h-[26px] items-center gap-1.5 whitespace-nowrap rounded-md border px-2 text-[0.68rem] font-medium",
        TONE[service.status],
      )}
    >
      <Icon aria-hidden="true" className="h-3.5 w-3.5 shrink-0" />
      <span>{service.name}</span>
      <span className="font-normal opacity-80">{kind}</span>
      {service.status !== "ready" && <span className="font-normal opacity-80">· {status}</span>}
    </span>
  );
}

/**
 * A machine's badges in the Flow: hanging under its card, linked by a
 * line. They follow the card - the position comes from it, so dragging the machine takes the
 * services along.
 */
export function PluginServiceTether({
  services,
  position,
  size,
}: {
  services: PluginService[];
  /**
   * Where the card is and its size. Without them, the badge is a child of the card
   * itself and sits right below its edge: the expanded card grows with the
   * content, and only it knows the height it has.
   */
  position?: CanvasPosition;
  size?: CardSize;
}) {
  if (services.length === 0) return null;
  const worst = services.find((service) => service.status === "error") ?? services[0];
  return (
    <div
      data-testid="plugin-service-tether"
      className="pointer-events-none absolute z-10"
      style={position && size ? { left: position.left + 16, top: position.top + size.height } : { left: 16, top: "100%" }}
    >
      <span
        aria-hidden="true"
        className={clsx("ml-3 block w-0.5", LINE[worst.status])}
        style={{ height: SERVICE_TETHER_GAP }}
      />
      <div className="pointer-events-auto flex gap-1.5">
        {services.map((service) => (
          <PluginServiceBadge key={`${service.plugin_code}:${service.id}`} service={service} />
        ))}
      </div>
    </div>
  );
}
