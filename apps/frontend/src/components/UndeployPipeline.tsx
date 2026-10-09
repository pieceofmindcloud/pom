/**
 * Tirar a pipeline inteira do ar, de uma vez.
 *
 * O contrário de "Implantar na rede". "Descarregar modelo" já existia, mas vale
 * para uma máquina só: desfazer uma implantação de quatro caixas era quatro
 * idas ao painel de cada uma, e a que ficasse esquecida seguiria com a fatia
 * ocupando RAM e VRAM sem servir para nada — o pipeline só planeja com a
 * cobertura fechada, então uma máquina de fora não é meia pipeline, é nenhuma.
 *
 * Duas coisas que esta tela deve ao operador:
 *
 * * **Perguntar antes**, nomeando as máquinas que vão ser liberadas. A ação
 *   alcança todo o cluster e não se desfaz clicando de novo.
 * * **Relatar máquina a máquina.** Um peer que não respondeu continua com a
 *   memória presa, e é exatamente isso que o operador precisa saber para ir
 *   até ele. Um "pronto!" que engole a falha manda a pessoa embora achando que
 *   liberou memória que continua ocupada.
 *
 * Nada é apagado do disco — ver `undeploy_endpoint` em `deploy.rs`.
 */
import { useState } from "react";
import { CircleCheck, CircleX, PowerOff } from "lucide-react";
import { undeployPipeline, type UndeployReport } from "../lib/api";
import { layerRange } from "../lib/format";
import { useInvalidate } from "../lib/queries";
import { useToast } from "../lib/toast";
import { Confirm, Modal } from "./Confirm";
import { Button } from "./ui";
import { useI18n } from "../i18n/provider";

export interface UndeployTarget {
  node_id: string;
  hostname: string;
  start_layer: number;
  end_layer: number;
}

export function UndeployPipeline({
  group,
  targets,
  deployId,
}: {
  /** Modelo que está no ar. */
  group: string;
  /** Máquinas que servem esse modelo agora, na ordem do pipeline. */
  targets: UndeployTarget[];
  /**
   * Qual implantação sai do ar. Sem ele, sai quem estiver servindo o rótulo —
   * o comportamento do painel, que mostra uma pipeline de cada vez. Com ele o
   * pedido é exato, que é o que a tela de implantações precisa quando o mesmo
   * modelo está no ar duas vezes.
   */
  deployId?: string;
}) {
  const { t } = useI18n();
  const [asking, setAsking] = useState(false);
  const [busy, setBusy] = useState(false);
  const [report, setReport] = useState<UndeployReport | null>(null);
  const toast = useToast();
  const invalidate = useInvalidate();

  async function undeploy() {
    setBusy(true);
    try {
      const r = await undeployPipeline(group, deployId);
      setReport(r);
      if (r.failed === 0) {
        toast.success(t("undeploy.button"), t("undeploy.allReleased", { count: r.released }));
      } else {
        toast.warning(
          t("undeploy.failedTitle"),
          r.released === 0
            ? t("undeploy.noneReleased")
            : t("undeploy.partial", { released: r.released, failed: r.failed }),
        );
      }
      invalidate();
    } catch (e) {
      toast.error(
        t("undeploy.failedTitle"),
        e instanceof Error ? e.message : String(e),
      );
    } finally {
      setBusy(false);
      setAsking(false);
    }
  }

  return (
    <>
      <Button size="sm" variant="outline" onClick={() => setAsking(true)}>
        <PowerOff className="h-3.5 w-3.5" />
        {t("undeploy.button")}
      </Button>

      <Confirm
        open={asking}
        busy={busy}
        tone="danger"
        title={t("undeploy.title", { group })}
        confirmLabel={t("undeploy.confirm")}
        onCancel={() => setAsking(false)}
        onConfirm={() => void undeploy()}
        description={
          <>
            <p>{t("undeploy.description", { count: targets.length })}</p>
            <p className="mt-2 text-xs text-faint">{t("undeploy.keepsFiles")}</p>
            <div className="mt-3 overflow-hidden rounded-lg border border-line">
              <div className="eyebrow border-b border-line bg-raised px-3 py-1.5">
                {t("undeploy.machines")}
              </div>
              <ul className="divide-y divide-line">
                {targets.map((m) => (
                  <li
                    key={m.node_id}
                    className="flex items-baseline justify-between gap-3 px-3 py-2"
                  >
                    <span className="truncate text-[0.83rem] text-fg">{m.hostname}</span>
                    <span className="shrink-0 font-mono text-xs text-muted">
                      {layerRange(m.start_layer, m.end_layer)}
                    </span>
                  </li>
                ))}
              </ul>
            </div>
          </>
        }
      />

      <Modal
        open={report != null}
        title={t("undeploy.reportTitle")}
        onClose={() => setReport(null)}
        footer={
          <Button variant="primary" onClick={() => setReport(null)}>
            {t("common.close")}
          </Button>
        }
      >
        {report && (
          <>
            <p>
              {report.failed === 0
                ? t("undeploy.allReleased", { count: report.released })
                : report.released === 0
                  ? t("undeploy.noneReleased")
                  : t("undeploy.partial", {
                      released: report.released,
                      failed: report.failed,
                    })}
            </p>
            <ul className="mt-3 divide-y divide-line overflow-hidden rounded-lg border border-line">
              {report.results.map((r) => (
                <li key={r.node_id} className="flex gap-3 px-3 py-2.5">
                  {r.released ? (
                    <CircleCheck className="mt-0.5 h-4 w-4 shrink-0 text-aqua" />
                  ) : (
                    <CircleX className="mt-0.5 h-4 w-4 shrink-0 text-rose" />
                  )}
                  <div className="min-w-0 flex-1">
                    <div className="flex items-baseline justify-between gap-3">
                      <span className="truncate text-[0.83rem] font-medium text-fg">
                        {r.hostname}
                      </span>
                      <span className="shrink-0 font-mono text-xs text-muted">
                        {layerRange(r.start_layer, r.end_layer)}
                      </span>
                    </div>
                    <p className="break-path mt-0.5 text-xs leading-relaxed text-muted">
                      {r.released
                        ? t("undeploy.released")
                        : (r.error ?? t("undeploy.stillLoaded"))}
                    </p>
                  </div>
                </li>
              ))}
            </ul>
          </>
        )}
      </Modal>
    </>
  );
}
