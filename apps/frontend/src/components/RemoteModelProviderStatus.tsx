import { CheckCircle2, KeyRound, TerminalSquare } from "lucide-react";
import type { RemoteModelProviderStatus as RemoteModelProviderStatusData } from "../lib/types";
import { useI18n } from "../i18n/provider";
import { Badge, Panel } from "./ui";

export function RemoteModelProviderStatus({
  status,
}: {
  status: RemoteModelProviderStatusData;
}) {
  const { t } = useI18n();
  const cli = status.cli;
  const missing = !cli.installed;
  const unauthenticated = cli.installed && !cli.authenticated;
  const tone = missing || unauthenticated ? "amber" : "aqua";
  const icon = missing ? (
    <TerminalSquare className="h-4 w-4" />
  ) : unauthenticated ? (
    <KeyRound className="h-4 w-4" />
  ) : (
    <CheckCircle2 className="h-4 w-4" />
  );
  const detail = missing
    ? t("remoteModels.cliMissingStatus")
    : unauthenticated
      ? t("remoteModels.authRequiredStatus")
      : t("remoteModels.authenticatedStatus", { username: cli.username || "Hugging Face" });

  return (
    <Panel className="flex flex-wrap items-start justify-between gap-3 border-line-strong/60 px-4 py-3.5 sm:px-5">
      <div className="flex min-w-0 items-start gap-3">
        <span className="mt-0.5 text-muted">{icon}</span>
        <div className="min-w-0">
          <div className="flex flex-wrap items-center gap-2">
            <span className="eyebrow">{t("remoteModels.providerStatus")}</span>
            <Badge tone={tone}>{missing ? t("remoteModels.cliUnavailable") : unauthenticated ? t("remoteModels.authNeeded") : t("remoteModels.authenticated")}</Badge>
          </div>
          <p className="mt-1 text-sm text-muted">{detail}</p>
        </div>
      </div>
      {cli.installed && (cli.version || cli.username) && (
        <span className="shrink-0 font-mono text-xs text-faint">
          {[cli.version, cli.username].filter(Boolean).join(" · ")}
        </span>
      )}
    </Panel>
  );
}
