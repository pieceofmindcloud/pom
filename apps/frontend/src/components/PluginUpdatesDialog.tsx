import { AlertTriangle, CheckCircle2, Download } from "lucide-react";
import { useState } from "react";
import { Modal } from "./Confirm";
import { Badge, Button } from "./ui";
import { useI18n } from "../i18n/provider";

/**
 * One update the "Check for updates" action found: POM itself, a licensed
 * plugin (license server) or a plugin installed from a public GitHub
 * repository. `install` applies just this one.
 */
export interface UpdateItem {
  key: string;
  kind: "pom" | "licensed" | "github";
  name: string;
  from: string | null;
  to: string;
  changelog: string | null;
  /** Not made by the POM team: installing it is at the operator's risk. */
  unverified?: boolean;
  /** Why this node cannot install it itself, when it cannot. */
  blocked?: string | null;
  install: () => Promise<unknown>;
}

type ItemState = "idle" | "installing" | "done" | "failed";

/**
 * The updates found, each with its own Install and one Install all. POM goes
 * last: installing it restarts the node, which ends this page's session.
 */
export function PluginUpdatesDialog({
  open,
  items,
  error,
  onClose,
  onInstalled,
}: {
  open: boolean;
  items: UpdateItem[];
  error: string | null;
  onClose: () => void;
  onInstalled: () => Promise<unknown> | void;
}) {
  const { t } = useI18n();
  const [states, setStates] = useState<Record<string, ItemState>>({});
  const [failures, setFailures] = useState<Record<string, string>>({});
  const busy = Object.values(states).includes("installing");
  const ordered = [...items.filter((item) => item.kind !== "pom"), ...items.filter((item) => item.kind === "pom")];
  const pending = ordered.filter((item) => !item.blocked && states[item.key] !== "done");

  async function install(item: UpdateItem) {
    setStates((current) => ({ ...current, [item.key]: "installing" }));
    setFailures((current) => {
      const { [item.key]: _, ...rest } = current;
      return rest;
    });
    try {
      await item.install();
      setStates((current) => ({ ...current, [item.key]: "done" }));
      return true;
    } catch (failure) {
      setStates((current) => ({ ...current, [item.key]: "failed" }));
      setFailures((current) => ({ ...current, [item.key]: failure instanceof Error ? failure.message : "" }));
      return false;
    }
  }

  async function installOne(item: UpdateItem) {
    await install(item);
    await onInstalled();
  }

  async function installAll() {
    for (const item of pending) await install(item);
    await onInstalled();
  }

  return (
    <Modal
      open={open}
      size="lg"
      title={t("plugins.updatesTitle")}
      onClose={() => !busy && onClose()}
      footer={(
        <>
          <Button variant="ghost" disabled={busy} onClick={onClose}>{t("common.close")}</Button>
          {pending.length > 1 && (
            <Button variant="primary" loading={busy} icon={<Download className="h-3.5 w-3.5" />} onClick={() => void installAll()}>
              {t("plugins.updatesInstallAll", { count: pending.length })}
            </Button>
          )}
        </>
      )}
    >
      <div className="space-y-3">
        {error && <p role="alert" className="break-words text-xs text-rose">{error}</p>}
        {ordered.length === 0 ? (
          <p role="status" className="flex items-center gap-2 text-sm text-fg">
            <CheckCircle2 className="h-4 w-4 text-aqua" aria-hidden="true" />
            {t("plugins.updatesNone")}
          </p>
        ) : (
          <ul className="space-y-2" aria-label={t("plugins.updatesTitle")}>
            {ordered.map((item) => {
              const state = states[item.key] ?? "idle";
              return (
                <li key={item.key} className="space-y-1.5 rounded-lg border border-line bg-raised/30 px-3 py-2.5">
                  <div className="flex flex-wrap items-center justify-between gap-2">
                    <div className="min-w-0">
                      <p className="flex flex-wrap items-center gap-1.5 text-sm font-medium text-fg">
                        {item.name}
                        {item.kind === "pom" && <Badge tone="iris">{t("plugins.updatesRestarts")}</Badge>}
                        {item.unverified && <Badge tone="amber">{t("plugins.unverifiedTitle")}</Badge>}
                      </p>
                      <p className="font-mono text-[0.7rem] text-muted">
                        {item.from ? `${item.from} → ${item.to}` : item.to}
                      </p>
                    </div>
                    {state === "done" ? (
                      <Badge tone="aqua">{item.kind === "pom" ? t("plugins.updatesRestarting") : t("plugins.updatesInstalled")}</Badge>
                    ) : item.blocked ? null : (
                      <Button
                        variant="outline"
                        disabled={busy && state !== "installing"}
                        loading={state === "installing"}
                        icon={<Download className="h-3.5 w-3.5" />}
                        onClick={() => void installOne(item)}
                      >
                        {t("plugins.updatesInstall")}
                      </Button>
                    )}
                  </div>
                  {item.changelog && (
                    <p className="line-clamp-3 whitespace-pre-wrap break-words text-[0.7rem] leading-relaxed text-faint">{item.changelog}</p>
                  )}
                  {item.blocked && <p className="text-[0.7rem] text-muted">{item.blocked}</p>}
                  {state === "failed" && (
                    <p role="alert" className="flex items-start gap-1.5 break-words text-xs text-rose">
                      <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" aria-hidden="true" />
                      {t("plugins.updatesFailed")} {failures[item.key]}
                    </p>
                  )}
                </li>
              );
            })}
          </ul>
        )}
      </div>
    </Modal>
  );
}
