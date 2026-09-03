/**
 * Tudo que está implantado no cluster, e o que dá para mexer sem refazer.
 *
 * Uma implantação liga um conjunto de máquinas a um modelo, de uma forma
 * (fatias, modelo inteiro ou tensor parallelism) e com uma configuração por
 * máquina. O mesmo modelo pode estar no ar mais de uma vez — em máquinas
 * diferentes, ou com parâmetros diferentes —, e antes desta tela isso era
 * invisível: o painel mostrava uma pipeline só, deduzida do nome do modelo.
 *
 * Duas palavras que a tela **não** pode confundir, porque não são a mesma:
 *
 * * **no ar** — há máquina viva anunciando esta implantação.
 * * **padrão** — é esta que atende quem pede só `"qwen3:8b"`, sem o sufixo
 *   `@<deploy_id>`. Uma implantação pode estar no ar sem ser a padrão, e a
 *   padrão pode estar fora do ar (registrada, mas com as máquinas caídas).
 *
 * Reimplantar aqui é só mexer em parâmetro: as máquinas recarregam o mesmo
 * arquivo com outra configuração, mantendo o `deploy_id`. Trocar de máquina ou
 * de faixa continua sendo assunto da tela de implantar, que é onde há o que
 * planejar e o que copiar.
 */
import { useState } from "react";
import { Layers, RefreshCw, Star } from "lucide-react";
import {
  activateDeployment,
  redeployDeployment,
  type DeploymentRow,
} from "../lib/api";
import { useDeployments, useInvalidate, useJobs } from "../lib/queries";
import { useToast } from "../lib/toast";
import { layerRange } from "../lib/format";
import { Modal } from "../components/Confirm";
import { RuntimeParamsForm } from "../components/RuntimeParamsForm";
import { UndeployPipeline } from "../components/UndeployPipeline";
import {
  Badge,
  Button,
  Empty,
  Field,
  Input,
  Mono,
  PageHeader,
  Panel,
  PanelHeader,
  Progress,
  ScrollX,
  Skeleton,
} from "../components/ui";
import type { DeploymentKind, RuntimeParams } from "../lib/types";
import { useI18n } from "../i18n/provider";

const KIND_TONE: Record<DeploymentKind, "iris" | "aqua" | "sky"> = {
  shards: "iris",
  whole: "aqua",
  tensor_parallel: "sky",
};

function createdAtLabel(iso: string, locale: string) {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return "-";
  return new Intl.DateTimeFormat(locale, {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(date);
}

/**
 * A caixa de reimplantar: os parâmetros comuns mais os dois botões que são de
 * verdade por máquina.
 *
 * Os campos vêm preenchidos com o que a implantação está rodando — que é o que
 * o registro guarda —, então mexer num campo é mexer num valor real, e não
 * preencher um formulário do zero torcendo para acertar o resto.
 */
function RedeployDialog({
  row,
  open,
  onClose,
}: {
  row: DeploymentRow;
  open: boolean;
  onClose: () => void;
}) {
  const { t } = useI18n();
  const toast = useToast();
  const invalidate = useInvalidate();
  const { data: jobsData } = useJobs();

  const first = row.deployment.nodes[0];
  const [params, setParams] = useState<RuntimeParams>({ ...first.config });
  const [perNode, setPerNode] = useState<
    Record<string, { n_gpu_layers?: number; offload_kqv?: boolean }>
  >(() =>
    Object.fromEntries(
      row.deployment.nodes.map((n) => [
        n.node_id,
        { n_gpu_layers: n.config.n_gpu_layers, offload_kqv: n.config.offload_kqv },
      ]),
    ),
  );
  const [busy, setBusy] = useState(false);
  const [jobId, setJobId] = useState<string | null>(null);

  const job = jobsData?.jobs.find((j) => j.id === jobId);
  const isTp = row.deployment.kind === "tensor_parallel";

  async function submit() {
    setBusy(true);
    try {
      const res = await redeployDeployment(row.deployment.deploy_id, {
        params,
        per_node: perNode,
      });
      setJobId(res.job_id);
      toast.success(t("deployments.redeployStarted"), row.deployment.model.label);
      invalidate();
    } catch (e) {
      toast.fromError(t("deployments.redeployFailed"), e);
    } finally {
      setBusy(false);
    }
  }

  return (
    <Modal
      open={open}
      size="lg"
      title={t("deployments.redeployTitle", { model: row.deployment.model.label })}
      onClose={onClose}
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            {t("common.close")}
          </Button>
          <Button onClick={submit} disabled={busy}>
            {t("deployments.redeployAction")}
          </Button>
        </>
      }
    >
      <div className="space-y-5">
        <p className="text-sm text-muted">
          {t("deployments.redeployHint", { id: row.deployment.deploy_id })}
        </p>

        {job && (
          <Panel>
            <div className="space-y-2 px-4 py-3">
              <div className="text-sm text-fg">{job.message || job.step}</div>
              <Progress value={job.progress ?? 0} />
              {job.outputs.length > 0 && (
                <ul className="mt-2 space-y-1 border-t border-line pt-2 text-[0.72rem] text-muted">
                  {job.outputs.map((output) => (
                    <li key={output} className="break-path font-mono">
                      {output}
                    </li>
                  ))}
                </ul>
              )}
            </div>
          </Panel>
        )}

        <RuntimeParamsForm value={params} onChange={setParams} disabled={busy} isTp={isTp} />

        <div className="space-y-2 border-t border-line pt-4">
          <div className="text-[0.82rem] text-fg">{t("deployments.perMachine")}</div>
          <ScrollX>
            <table className="w-full text-sm">
              <thead>
                <tr className="text-left text-xs text-faint">
                  <th className="py-1 pr-4">{t("deployments.machine")}</th>
                  <th className="py-1 pr-4">{t("deployments.layers")}</th>
                  <th className="py-1 pr-4">{t("deploy.knob.n_gpu_layers")}</th>
                  <th className="py-1">{t("deploy.offloadKqv")}</th>
                </tr>
              </thead>
              <tbody>
                {row.deployment.nodes.map((n) => (
                  <tr key={n.node_id} className="border-t border-line">
                    <td className="py-2 pr-4">{n.hostname || n.node_id.slice(0, 12)}</td>
                    <td className="py-2 pr-4 text-muted">
                      {layerRange(n.start_layer, n.end_layer)}
                    </td>
                    <td className="py-2 pr-4">
                      <Field label="" >
                        <Input
                          type="number"
                          min={0}
                          disabled={busy}
                          aria-label={`${t("deploy.knob.n_gpu_layers")} ${n.hostname}`}
                          value={String(perNode[n.node_id]?.n_gpu_layers ?? "")}
                          onChange={(e) =>
                            setPerNode((s) => ({
                              ...s,
                              [n.node_id]: {
                                ...s[n.node_id],
                                n_gpu_layers: Number(e.target.value),
                              },
                            }))
                          }
                        />
                      </Field>
                    </td>
                    <td className="py-2">
                      <input
                        type="checkbox"
                        aria-label={`${t("deploy.offloadKqv")} ${n.hostname}`}
                        checked={perNode[n.node_id]?.offload_kqv ?? true}
                        disabled={busy}
                        onChange={(e) =>
                          setPerNode((s) => ({
                            ...s,
                            [n.node_id]: {
                              ...s[n.node_id],
                              offload_kqv: e.target.checked,
                            },
                          }))
                        }
                      />
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </ScrollX>
        </div>
      </div>
    </Modal>
  );
}

function Row({ row }: { row: DeploymentRow }) {
  const { locale, t } = useI18n();
  const toast = useToast();
  const invalidate = useInvalidate();
  const [redeploying, setRedeploying] = useState(false);
  const d = row.deployment;

  async function activate() {
    try {
      await activateDeployment(d.deploy_id);
      toast.success(t("deployments.activated", { model: d.model.label }));
      invalidate();
    } catch (e) {
      toast.fromError(t("deployments.activateFailed"), e);
    }
  }

  return (
    <tr className="border-t border-line align-top">
      <td className="whitespace-nowrap px-5 py-3">
        <Mono>{d.deploy_id}</Mono>
      </td>
      <td className="px-5 py-3">{d.model.label}</td>
      <td className="px-5 py-3">
        <Badge tone={KIND_TONE[d.kind]}>{t("deployments.kind." + d.kind)}</Badge>
      </td>
      <td className="whitespace-nowrap px-5 py-3 text-muted">
        {t("deployments.nodeCount", { count: d.nodes.length })}
      </td>
      <td className="whitespace-nowrap px-5 py-3 text-muted">
        <time dateTime={d.created_at}>{createdAtLabel(d.created_at, locale)}</time>
      </td>
      <td className="px-5 py-3">
        <div className="flex flex-wrap gap-1.5">
          {row.serving ? (
            <Badge tone="lime">{t("deployments.serving")}</Badge>
          ) : (
            <Badge tone="neutral">{t("deployments.idle")}</Badge>
          )}
          {row.is_default && <Badge tone="amber">{t("deployments.default")}</Badge>}
        </div>
      </td>
      <td className="px-5 py-3">
        <div className="flex flex-wrap items-center gap-2">
          <Button
            variant="ghost"
            onClick={() => setRedeploying(true)}
            icon={<RefreshCw className="h-3.5 w-3.5" />}
          >
            {t("deployments.redeploy")}
          </Button>
          {!row.is_default && (
            <Button
              variant="ghost"
              onClick={activate}
              icon={<Star className="h-3.5 w-3.5" />}
            >
              {t("deployments.makeDefault")}
            </Button>
          )}
          {row.serving && (
            <UndeployPipeline
              group={d.model.label}
              deployId={d.deploy_id}
              targets={d.nodes.map((n) => ({
                node_id: n.node_id,
                hostname: n.hostname,
                start_layer: n.start_layer,
                end_layer: n.end_layer,
              }))}
            />
          )}
          {/* O `Modal` se muda para o `body` por portal, então ficar aqui
              dentro é só onde ele nasce — não afeta o layout da tabela. */}
          {redeploying && (
            <RedeployDialog row={row} open onClose={() => setRedeploying(false)} />
          )}
        </div>
      </td>
    </tr>
  );
}

export function Deployments() {
  const { t } = useI18n();
  const { data, isLoading } = useDeployments();
  const rows = [...(data?.deployments ?? [])].sort((a, b) => {
    const aTime = Date.parse(a.deployment.created_at);
    const bTime = Date.parse(b.deployment.created_at);
    if (Number.isNaN(aTime) && Number.isNaN(bTime)) return 0;
    if (Number.isNaN(aTime)) return 1;
    if (Number.isNaN(bTime)) return -1;
    return bTime - aTime;
  });

  return (
    <div className="space-y-6">
      <PageHeader
        title={t("deployments.title")}
        subtitle={t("deployments.subtitle")}
      />

      <Panel>
        <PanelHeader
          eyebrow={t("deployments.eyebrow")}
          title={t("deployments.listTitle")}
          subtitle={t("deployments.listSubtitle")}
        />
        {isLoading ? (
          <div className="space-y-2 px-5 py-4">
            <Skeleton className="h-8" />
            <Skeleton className="h-8" />
          </div>
        ) : rows.length === 0 ? (
          <Empty icon={<Layers className="h-5 w-5" />} title={t("deployments.emptyTitle")}>
            {t("deployments.emptyHint")}
          </Empty>
        ) : (
          <ScrollX>
            <table className="w-full min-w-[72rem] text-sm">
              <thead>
                <tr className="text-left text-xs text-faint">
                  <th className="px-5 py-2">{t("deployments.id")}</th>
                  <th className="px-5 py-2">{t("deployments.model")}</th>
                  <th className="px-5 py-2">{t("deployments.kindLabel")}</th>
                  <th className="px-5 py-2">{t("deployments.machines")}</th>
                  <th className="px-5 py-2">{t("deployments.created")}</th>
                  <th className="px-5 py-2">{t("deployments.state")}</th>
                  <th className="px-5 py-2">{t("deployments.actions")}</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((r) => (
                  <Row key={r.deployment.deploy_id} row={r} />
                ))}
              </tbody>
            </table>
          </ScrollX>
        )}
      </Panel>
    </div>
  );
}
