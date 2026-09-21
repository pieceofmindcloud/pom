/**
 * Informational stand-in for the interactive Logs/Statistics screens.
 *
 * The plugin `enterprise` (Request Logs/Statistics) was extracted to
 * https://github.com/pereirajair/pom-plugin-logs. Its interactive UI moved
 * with it (kept there as a reference, not buildable standalone - it depends
 * on this app's shared design system and hooks). This screen replaces it in
 * the POM bundle: always visible for product discovery, with a preview
 * illustration, a short feature list and a link to buy/install. It renders
 * regardless of activation state - see
 * `docs/subsystems/licenciamento-enterprise.md`, "Extração para
 * pom-plugin-logs".
 */
import { ExternalLink } from "lucide-react";
import { Badge, Button, Panel, PanelHeader } from "./ui";
import { useEnterpriseStatus } from "../lib/queries";
import { useI18n } from "../i18n/provider";

const ENTERPRISE_SITE_URL = "https://pieceofmind.cloud/enterprise/index.html";

function LogsPreview() {
  const rows = [0, 1, 2, 3, 4];
  return (
    <svg viewBox="0 0 320 160" className="h-full w-full" role="presentation" aria-hidden="true">
      <rect x="0" y="0" width="320" height="160" rx="6" fill="rgb(var(--surface))" />
      <rect x="0" y="0" width="320" height="28" rx="6" fill="rgb(var(--raised))" />
      {["Hora", "Chave", "Modelo", "Tokens", "Status"].map((_, index) => (
        <rect key={index} x={10 + index * 62} y="11" width="46" height="6" rx="3" fill="rgb(var(--faint))" />
      ))}
      {rows.map((row) => (
        <g key={row}>
          <rect x="0" y={30 + row * 26} width="320" height="24" fill={row % 2 === 0 ? "rgb(var(--surface))" : "rgb(var(--raised) / 0.4)"} />
          <rect x="10" y={40 + row * 26} width="34" height="6" rx="3" fill="rgb(var(--muted))" />
          <rect x="72" y={40 + row * 26} width="50" height="6" rx="3" fill="rgb(var(--muted))" />
          <rect x="134" y={40 + row * 26} width="42" height="6" rx="3" fill="rgb(var(--muted))" />
          <rect x="196" y={40 + row * 26} width="38" height="6" rx="3" fill="rgb(var(--muted))" />
          <rect x="258" y={40 + row * 26} width="44" height="10" rx="5" fill={row === 2 ? "#e783a4" : "#58d5c5"} opacity="0.55" />
        </g>
      ))}
    </svg>
  );
}

function StatisticsPreview() {
  const bars = [38, 62, 44, 78, 54, 30];
  return (
    <svg viewBox="0 0 320 160" className="h-full w-full" role="presentation" aria-hidden="true">
      <rect x="0" y="0" width="320" height="160" rx="6" fill="rgb(var(--surface))" />
      {bars.map((height, index) => (
        <rect
          key={index}
          x={16 + index * 34}
          y={140 - height}
          width="20"
          height={height}
          rx="3"
          fill={["#8b7dff", "#58d5c5", "#f3b85f", "#e783a4", "#7eb4ff", "#8b7dff"][index]}
          opacity="0.75"
        />
      ))}
      <line x1="8" y1="140" x2="312" y2="140" stroke="rgb(var(--line))" strokeWidth="1" />
      <circle cx="262" cy="40" r="26" fill="none" stroke="#7eb4ff" strokeWidth="10" opacity="0.75" />
      <circle cx="262" cy="40" r="26" fill="none" stroke="#8b7dff" strokeWidth="10" strokeDasharray="55 110" opacity="0.85" />
    </svg>
  );
}

type ShowcaseKind = "logs" | "statistics";

export function EnterpriseFeatureShowcase({ kind }: { kind: ShowcaseKind }) {
  const { t } = useI18n();
  const enterpriseStatus = useEnterpriseStatus();
  const active = Boolean(enterpriseStatus.data?.active && enterpriseStatus.data.features.includes(kind === "logs" ? "request_logs" : "statistics"));

  return (
    <main className="flex min-h-full items-center justify-center bg-ink px-5 py-10">
      <Panel className="w-full max-w-3xl">
        <PanelHeader
          eyebrow={t("enterprise.eyebrow")}
          title={t(`enterprise.showcase.${kind}.title`)}
          subtitle={t(`enterprise.showcase.${kind}.description`)}
          actions={active ? <Badge tone="aqua">{t("enterprise.showcase.activeNote")}</Badge> : undefined}
        />
        <div className="grid gap-5 p-5 sm:grid-cols-[minmax(0,1fr)_minmax(0,1fr)]">
          <div
            className="overflow-hidden rounded-md border border-line"
            aria-label={t(`enterprise.showcase.${kind}.screenshotAlt`)}
          >
            {kind === "logs" ? <LogsPreview /> : <StatisticsPreview />}
          </div>
          <div className="min-w-0">
            <ul className="space-y-2 text-sm text-muted">
              {[1, 2, 3].map((index) => (
                <li key={index} className="flex gap-2">
                  <span className="mt-1.5 h-1.5 w-1.5 shrink-0 rounded-full bg-iris" />
                  <span>{t(`enterprise.showcase.${kind}.bullet${index}`)}</span>
                </li>
              ))}
            </ul>
            <p className="mt-4 text-[0.7rem] text-faint">{t("enterprise.showcase.pluginNote")}</p>
            <a
              href={ENTERPRISE_SITE_URL}
              target="_blank"
              rel="noreferrer"
              className="mt-4 inline-flex"
            >
              <Button variant="primary" icon={<ExternalLink className="h-3.5 w-3.5" />}>
                {t("enterprise.showcase.cta")}
              </Button>
            </a>
          </div>
        </div>
      </Panel>
    </main>
  );
}
