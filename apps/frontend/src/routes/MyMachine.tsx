/**
 * Esta máquina isolada da rede: identidade, portas, autoridade efetiva e o
 * endereço anunciado aos peers. Separado de `NetworkSettings.tsx`, que mostra
 * o retrato geral da rede.
 */
import { LogIn, LogOut, Radio, Save } from "lucide-react";
import { useEffect, useState } from "react";
import { Button, Field, Footnote, Input, KeyValue, Mono, PageHeader, Panel, PanelHeader } from "../components/ui";
import { api } from "../lib/api";
import { useInvalidate, useNode } from "../lib/queries";
import { useToast } from "../lib/toast";
import type { NodeInfo } from "../lib/types";
import { useI18n } from "../i18n/provider";

export function MyMachine() {
  const { t } = useI18n();
  const node = useNode();
  const toast = useToast();
  const invalidate = useInvalidate();

  const [busy, setBusy] = useState(false);
  const [advertiseAddr, setAdvertiseAddr] = useState("");
  const [addrSaving, setAddrSaving] = useState(false);
  const [addrLoaded, setAddrLoaded] = useState(false);
  const [nodeName, setNodeName] = useState("");
  const [nameSaving, setNameSaving] = useState(false);

  // Load once: re-syncing on every poll would fight the operator's typing.
  useEffect(() => {
    if (addrLoaded || !node.data) return;
    setAdvertiseAddr(node.data.advertise_addr ?? "");
    setNodeName(node.data.node_name ?? "");
    setAddrLoaded(true);
  }, [node.data, addrLoaded]);

  const detached = node.data?.detached ?? false;

  async function toggleMembership() {
    setBusy(true);
    try {
      await api.post(detached ? "/api/ui/rejoin" : "/api/ui/leave");
      toast.success(detached ? t("network.back") : t("network.detached"));
      invalidate();
    } catch (e) {
      toast.fromError(t("network.operationFailed"), e);
    } finally {
      setBusy(false);
    }
  }

  async function saveAdvertiseAddr() {
    setAddrSaving(true);
    try {
      const saved = await api.post<Pick<NodeInfo, "advertise_addr" | "effective_address">>(
        "/api/ui/network/advertise-addr",
        { advertise_addr: advertiseAddr.trim() },
      );
      setAdvertiseAddr(saved.advertise_addr ?? "");
      toast.success(t("network.addrSaved"), t("network.addrSavedDetail"));
      invalidate();
    } catch (e) {
      toast.fromError(t("network.addrSaveFailed"), e);
    } finally {
      setAddrSaving(false);
    }
  }

  async function saveNodeName() {
    setNameSaving(true);
    try {
      const saved = await api.post<Pick<NodeInfo, "node_name" | "hostname">>(
        "/api/ui/network/node-name",
        { node_name: nodeName.trim() },
      );
      setNodeName(saved.node_name ?? "");
      toast.success(t("myMachine.nameSaved"), t("myMachine.nameSavedDetail"));
      invalidate();
    } catch (e) {
      toast.fromError(t("myMachine.nameSaveFailed"), e);
    } finally {
      setNameSaving(false);
    }
  }

  return (
    <>
      <PageHeader title={t("myMachine.title")} subtitle={t("myMachine.subtitle")} />

      <div className="space-y-5">
        <Panel>
          <PanelHeader
            eyebrow={t("network.thisMachine")}
            title={node.data?.hostname ?? "—"}
            subtitle={detached ? t("network.offlineDetail") : t("network.onlineDetail")}
            actions={
              <Button
                variant={detached ? "primary" : "danger"}
                loading={busy}
                icon={detached ? <LogIn className="h-3.5 w-3.5" /> : <LogOut className="h-3.5 w-3.5" />}
                onClick={() => void toggleMembership()}
              >
                {detached ? t("network.rejoin") : t("network.leave")}
              </Button>
            }
          />
          {node.data && (
            <dl className="grid gap-x-6 gap-y-4 px-5 py-4 sm:grid-cols-3">
              <KeyValue label="node_id">
                <Mono>{node.data.node_id}</Mono>
              </KeyValue>
              <KeyValue label={t("network.ports")}>
                <Mono>
                  {t("network.portValues", {
                    p2p: node.data.p2p_port,
                    gateway: node.data.gateway_port,
                    inference: node.data.inference_port ?? "—",
                  })}
                </Mono>
              </KeyValue>
              <KeyValue label={t("network.profile")}>
                {t(`profile.${node.data.profile}`)}
              </KeyValue>
              {/* O antigo "Papel" (principal/nó) saiu do `/api/ui/node` com o
                  modelo de confiança e deixava a célula em branco. O que existe
                  hoje é a autoridade efetiva, e ela responde à mesma pergunta:
                  esta máquina manda em quê. `profile: admin` sem concessão
                  aparece como sem autoridade, que é o que ele é. */}
              <KeyValue label={t("network.authority")}>
                {node.data.authority_level
                  ? t(`network.authority.${node.data.authority_level}`)
                  : t("network.authorityNone")}
              </KeyValue>
            </dl>
          )}
        </Panel>

        <Panel>
          <PanelHeader
            title={t("myMachine.nameTitle")}
            subtitle={t("myMachine.nameSubtitle")}
            actions={
              <Button
                variant="primary"
                icon={<Save className="h-3.5 w-3.5" />}
                loading={nameSaving}
                data-testid="node-name-save"
                onClick={() => void saveNodeName()}
                disabled={!addrLoaded}
              >
                {t("network.addrSave")}
              </Button>
            }
          />
          <div className="px-5 py-4">
            <Field label={t("myMachine.nameLabel")} hint={t("myMachine.nameHint")}>
              <Input
                data-testid="node-name"
                value={nodeName}
                maxLength={64}
                placeholder={node.data?.node_name ? "" : node.data?.hostname ?? ""}
                onChange={(e) => setNodeName(e.target.value)}
              />
            </Field>
          </div>
        </Panel>

        <Panel>
          <PanelHeader
            title={t("network.advertiseTitle")}
            subtitle={t("network.advertiseSubtitle")}
            actions={
              <Button
                variant="primary"
                icon={<Save className="h-3.5 w-3.5" />}
                loading={addrSaving}
                onClick={() => void saveAdvertiseAddr()}
                disabled={!addrLoaded}
              >
                {t("network.addrSave")}
              </Button>
            }
          />
          <div className="space-y-3 px-5 py-4">
            <Field label={t("network.addrLabel")} hint={t("network.addrHint")}>
              <Input
                value={advertiseAddr}
                spellCheck={false}
                placeholder={t("network.addrPlaceholder")}
                onChange={(e) => setAdvertiseAddr(e.target.value)}
              />
            </Field>
            <KeyValue label={t("network.addrEffective")}>
              <Mono>{node.data?.effective_address ?? "—"}</Mono>
            </KeyValue>
            <Footnote icon={<Radio className="h-3.5 w-3.5" />}>
              {t("network.addrPrecedence")}
            </Footnote>
          </div>
        </Panel>

      </div>
    </>
  );
}
