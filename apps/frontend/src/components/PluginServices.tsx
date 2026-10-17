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
 * connecting line, so the screen says **where** the service runs. In the Flow
 * the badge is only its icon and opens to the full text on hover or focus: a
 * row of named badges under each card was wider than the card itself.
 *
 * The engine shown is the one the machine's model runs on (`servicesInUse`): a
 * machine on llama.cpp gets a llama.cpp badge and loses the Strata one, even
 * when the Strata plugin is installed there, because it is not what serves now.
 */
import clsx from "clsx";
import { Cpu, Database, Puzzle } from "lucide-react";
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
/** Height of a badge, so the Flow's arrangement can leave room for the tether. */
export const SERVICE_BADGE_HEIGHT = 26;
/** What the tether adds under a card: the line plus the badge row. */
export const SERVICE_TETHER_HEIGHT = SERVICE_TETHER_GAP + SERVICE_BADGE_HEIGHT;

/** The plugin code of the built-in engine's pseudo service. */
export const LLAMA_CPP_SERVICE = "llama.cpp";

/**
 * The services worth drawing for a machine.
 *
 * With a model loaded, the only inference service shown is the engine that runs
 * it (`PeerView.engine`, null for llama.cpp, which is not a plugin and gets a
 * badge of its own); other inference plugins installed on the machine are not
 * in use by that deployment. Without a model, nothing runs, so the services are
 * shown as published.
 */
export function servicesInUse(
  services: PluginService[] | undefined,
  engine: string | null | undefined,
  servesModel: boolean,
): PluginService[] {
  const list = services ?? [];
  if (!servesModel) return list;
  const kept = list.filter((service) => service.kind !== "inference" || service.plugin_code === engine);
  if (engine) return kept;
  return [
    { plugin_code: LLAMA_CPP_SERVICE, id: LLAMA_CPP_SERVICE, kind: "inference", name: "llama.cpp", status: "ready" },
    ...kept,
  ];
}

export function PluginServiceBadge({ service, compact = false }: { service: PluginService; compact?: boolean }) {
  const { t } = useI18n();
  const status = {
    ready: t("dashboard.serviceStatusReady"),
    starting: t("dashboard.serviceStatusStarting"),
    installing: t("dashboard.serviceStatusInstalling"),
    waiting: t("dashboard.serviceStatusWaiting"),
    error: t("dashboard.serviceStatusError"),
  }[service.status];
  // A type that this version of the screen does not know shows up as the plugin called it.
  const kind = service.kind === "rag"
    ? t("dashboard.serviceKindRag")
    : service.kind === "inference"
      ? t("dashboard.serviceKindInference")
      : service.kind;
  const Icon = service.kind === "rag" ? Database : service.kind === "inference" ? Cpu : Puzzle;
  const title = [`${service.name} · ${kind}`, status, service.version, service.detail]
    .filter(Boolean)
    .join(" · ");
  if (compact) {
    // Icon only; hover or keyboard focus slides the name and kind out.
    return (
      <span
        data-testid="plugin-service"
        data-status={service.status}
        data-compact="true"
        tabIndex={0}
        title={title}
        aria-label={title}
        className={clsx(
          "group/service inline-flex h-[26px] items-center whitespace-nowrap rounded-md border px-1.5 text-[0.68rem] font-medium outline-none focus-visible:ring-1 focus-visible:ring-iris",
          TONE[service.status],
        )}
      >
        <Icon aria-hidden="true" className="h-3.5 w-3.5 shrink-0" />
        <span
          aria-hidden="true"
          className="max-w-0 overflow-hidden opacity-0 transition-[max-width,opacity,margin] duration-200 group-hover/service:ml-1.5 group-hover/service:max-w-[16rem] group-hover/service:opacity-100 group-focus/service:ml-1.5 group-focus/service:max-w-[16rem] group-focus/service:opacity-100"
        >
          {service.name} <span className="font-normal opacity-80">{kind}</span>
          {service.status !== "ready" && <span className="font-normal opacity-80"> · {status}</span>}
        </span>
      </span>
    );
  }
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
        className={clsx("ml-2.5 block w-0.5", LINE[worst.status])}
        style={{ height: SERVICE_TETHER_GAP }}
      />
      <div className="pointer-events-auto flex gap-1.5">
        {services.map((service) => (
          <PluginServiceBadge key={`${service.plugin_code}:${service.id}`} service={service} compact />
        ))}
      </div>
    </div>
  );
}
