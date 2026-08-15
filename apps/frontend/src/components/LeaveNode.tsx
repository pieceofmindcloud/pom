/**
 * Tirar **uma** máquina da rede, do card dela no Painel.
 *
 * É o mesmo ato do menu "Este nó › Sair da rede", que sempre valeu só para a
 * máquina em que o painel está aberto. Mas o painel de um nó mostra o cluster
 * inteiro, e ir até a interface de cada máquina para desligá-la é uma volta que
 * o operador não deveria dar — então o pedido vai para o nó local com o
 * `node_id` do alvo e ele repassa (`/api/ui/leave`, `webui::forward_leave`).
 * Quem se desliga continua sendo o dono daquele processo: este nó não detacha
 * ninguém por fora.
 *
 * Confirmação explícita porque o caminho de volta não é este: a máquina que sai
 * some do painel, e reentrar se faz pela aba Rede *dela*.
 */
import { useState } from "react";
import { LogOut } from "lucide-react";
import { leaveNetwork } from "../lib/api";
import { useInvalidate } from "../lib/queries";
import { useToast } from "../lib/toast";
import { Confirm } from "./Confirm";
import { Button } from "./ui";
import { useI18n } from "../i18n/provider";

export function LeaveNode({
  nodeId,
  hostname,
  isSelf,
}: {
  nodeId: string;
  hostname: string;
  /** No próprio nó o pedido é local — sem repasse e sem `node_id`. */
  isSelf: boolean;
}) {
  const { t } = useI18n();
  const [asking, setAsking] = useState(false);
  const [busy, setBusy] = useState(false);
  const toast = useToast();
  const invalidate = useInvalidate();

  async function leave() {
    setBusy(true);
    try {
      await leaveNetwork(isSelf ? undefined : nodeId);
      toast.success(t("dashboard.leftNode", { hostname }), t("dashboard.leftNodeDetail"));
      invalidate();
    } catch (e) {
      toast.fromError(t("dashboard.leaveNodeFailed", { hostname }), e);
    } finally {
      setBusy(false);
      setAsking(false);
    }
  }

  return (
    <>
      <Button
        size="sm"
        variant="outline"
        onClick={() => setAsking(true)}
        className="shrink-0"
      >
        <LogOut className="h-3.5 w-3.5" />
        {t("dashboard.leaveNode")}
      </Button>

      <Confirm
        open={asking}
        busy={busy}
        tone="danger"
        title={t("dashboard.leaveNodeTitle", { hostname })}
        confirmLabel={t("dashboard.leaveNodeConfirm")}
        onCancel={() => setAsking(false)}
        onConfirm={() => void leave()}
        description={t("dashboard.leaveNodeDescription", { hostname })}
      />
    </>
  );
}
