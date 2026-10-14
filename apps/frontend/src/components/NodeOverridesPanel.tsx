/**
 * One machine's own parameters inside the version properties of a live deployment.
 *
 * Each field shows its value and where it comes from: the version (inherited) or the machine (its own). Saving sends only
 * the fields the operator filled; the node reloads that machine alone and records the change in the active version. A
 * refusal from the machine is shown and nothing changes.
 */
import { useState } from "react";
import { api } from "../lib/api";
import type { NodeOverrides, RuntimeParams } from "../lib/types";
import { useI18n } from "../i18n/provider";
import { Button } from "./ui";

type OffloadChoice = "inherit" | "gpu" | "cpu";

function draftOf(overrides: NodeOverrides): {
  ctx: string;
  sessions: string;
  gpuLayers: string;
  offload: OffloadChoice;
} {
  return {
    ctx: overrides.n_ctx != null ? String(overrides.n_ctx) : "",
    sessions: overrides.max_sessions != null ? String(overrides.max_sessions) : "",
    gpuLayers: overrides.n_gpu_layers != null ? String(overrides.n_gpu_layers) : "",
    offload: overrides.offload_kqv == null ? "inherit" : overrides.offload_kqv ? "gpu" : "cpu",
  };
}

/** The body the endpoint takes: only the fields the operator set. */
export function overridesBody(draft: ReturnType<typeof draftOf>): NodeOverrides {
  const body: NodeOverrides = {};
  if (draft.ctx.trim() !== "") body.n_ctx = Number(draft.ctx);
  if (draft.sessions.trim() !== "") body.max_sessions = Number(draft.sessions);
  if (draft.gpuLayers.trim() !== "") body.n_gpu_layers = Number(draft.gpuLayers);
  if (draft.offload !== "inherit") body.offload_kqv = draft.offload === "gpu";
  return body;
}

export function NodeOverridesPanel({
  deployId,
  nodeId,
  hostname,
  versionParams,
  overrides,
  onSaved,
}: {
  deployId: string;
  nodeId: string;
  hostname: string;
  /** The version's parameters: what a field shows when the machine does not set it. */
  versionParams: RuntimeParams;
  overrides: NodeOverrides;
  onSaved: () => void;
}) {
  const { t } = useI18n();
  const [draft, setDraft] = useState(() => draftOf(overrides));
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<{ tone: "ok" | "error"; text: string } | null>(null);

  async function send(body: NodeOverrides) {
    setBusy(true);
    setMessage(null);
    try {
      await api.post(
        `/api/ui/deployments/${encodeURIComponent(deployId)}/nodes/${encodeURIComponent(nodeId)}/overrides`,
        body,
      );
      setMessage({ tone: "ok", text: t("deployments.nodeOverrideDone") });
      onSaved();
    } catch (reason) {
      setMessage({
        tone: "error",
        text: t("deployments.nodeOverrideFailed", {
          error: reason instanceof Error ? reason.message : String(reason),
        }),
      });
    } finally {
      setBusy(false);
    }
  }

  const rows: { key: keyof typeof draft; label: string; own: boolean; inherited: string }[] = [
    {
      key: "ctx",
      label: t("deployments.nodeOverrideCtx"),
      own: overrides.n_ctx != null,
      inherited: String(versionParams.n_ctx),
    },
    {
      key: "sessions",
      label: t("deployments.nodeOverrideSessions"),
      own: overrides.max_sessions != null,
      inherited: String(versionParams.max_sessions),
    },
    {
      key: "gpuLayers",
      label: t("deployments.nodeOverrideGpuLayers"),
      own: overrides.n_gpu_layers != null,
      inherited: String(versionParams.n_gpu_layers),
    },
  ];

  return (
    <div className="grid gap-3 p-2 text-xs">
      <p className="text-muted">{t("deployments.nodeOverridesHint", { machine: hostname || nodeId.slice(0, 12) })}</p>
      {rows.map((row) => (
        <label key={row.key} className="grid gap-1">
          <span className="flex items-center justify-between gap-2 text-muted">
            <span>{row.label}</span>
            <span className="font-mono text-[0.6rem] text-faint">
              {row.own ? t("deployments.nodeOwn") : `${t("deployments.nodeInherited")}: ${row.inherited}`}
            </span>
          </span>
          <input
            type="number"
            min={0}
            className="rounded-md border border-line bg-surface px-2 py-1 font-mono text-xs text-fg"
            placeholder={row.inherited}
            value={draft[row.key] as string}
            onChange={(event) => setDraft({ ...draft, [row.key]: event.target.value })}
          />
        </label>
      ))}
      <label className="grid gap-1">
        <span className="flex items-center justify-between gap-2 text-muted">
          <span>{t("deployments.nodeOverrideKv")}</span>
          <span className="font-mono text-[0.6rem] text-faint">
            {overrides.offload_kqv != null ? t("deployments.nodeOwn") : t("deployments.nodeInherited")}
          </span>
        </span>
        <select
          className="rounded-md border border-line bg-surface px-2 py-1 text-xs text-fg"
          value={draft.offload}
          onChange={(event) => setDraft({ ...draft, offload: event.target.value as OffloadChoice })}
        >
          <option value="inherit">{t("deployments.nodeInheritChoice")}</option>
          <option value="gpu">{t("deploy.kvOnGpu")}</option>
          <option value="cpu">{t("deploy.kvInRam")}</option>
        </select>
      </label>
      {message && (
        <p role="status" className={message.tone === "error" ? "text-rose" : "text-lime"}>
          {message.text}
        </p>
      )}
      <div className="flex flex-wrap justify-end gap-2">
        <Button size="sm" variant="outline" disabled={busy} onClick={() => void send({})}>
          {t("deployments.nodeOverrideReset")}
        </Button>
        <Button size="sm" variant="primary" disabled={busy} onClick={() => void send(overridesBody(draft))}>
          {t("deployments.nodeOverrideSave")}
        </Button>
      </div>
    </div>
  );
}
