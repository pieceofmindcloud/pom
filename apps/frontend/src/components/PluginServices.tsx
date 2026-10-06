/**
 * O que os plugins de uma máquina servem para a rede, desenhado junto dela.
 *
 * Um plugin é local ao nó, mas o que ele roda pode ser de todos: o Qdrant de
 * uma máquina é o banco de RAG da pipeline inteira. O nó publica esses
 * serviços em `node.plugin_services`, e aqui eles viram selos - um por
 * serviço, com a cor do estado.
 *
 * Dois lugares usam o mesmo selo: o card de detalhes da máquina (`inline`) e o
 * Fluxo, onde `PluginServiceTether` os pendura sob o card da máquina com um
 * traço de ligação, para a tela dizer **onde** o serviço roda.
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

/** Distância entre a borda de baixo do card e os selos, no Fluxo. */
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
  // Um tipo que esta versão da tela não conhece aparece como o plugin o chamou.
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
 * Os selos de uma máquina no Fluxo: pendurados sob o card dela, ligados por um
 * traço. Seguem o card - a posição vem dele, então arrastar a máquina leva os
 * serviços junto.
 */
export function PluginServiceTether({
  services,
  position,
  size,
}: {
  services: PluginService[];
  /**
   * Onde o card está e o tamanho dele. Sem eles, o selo é filho do próprio
   * card e fica logo abaixo da borda dele: o card expandido cresce com o
   * conteúdo, e só ele sabe a altura que tem.
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
