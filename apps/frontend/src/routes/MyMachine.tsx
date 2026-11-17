/**
 * This machine in isolation from the network: identity, ports, effective
 * authority and the address announced to the peers. Separate from
 * `NetworkSettings.tsx`, which shows the overall picture of the network.
 */
import { Check, LogIn, LogOut, Radio, Save } from "lucide-react";
import { useEffect, useState } from "react";
import { Button, Field, Footnote, Input, KeyValue, Mono, PageHeader, Panel, PanelHeader } from "../components/ui";
import { api } from "../lib/api";
import { useInvalidate, useNode } from "../lib/queries";
import { useToast } from "../lib/toast";
import type { NodeInfo } from "../lib/types";
import { useI18n } from "../i18n/provider";

/** What `GET /api/ui/network/transport` answers: the saved switch, what this
 * run is doing, and whether the saved value still waits for its restart. */
interface TransportView {
  no_iroh: boolean;
  live: boolean;
  restart_needed: boolean;
}

/** The one line that says what the transport of this machine is. */
function transportStateKey(view: TransportView): string {
  if (view.live) return "myMachine.transportStateDirect";
  if (view.no_iroh) return "myMachine.transportStatePending";
  return "myMachine.transportStateIroh";
}

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
  // The transport mode of this machine: what is saved, what this run is doing,
  // and whether the two diverge (spec 2026-11-15). `null` while unknown or
  // unavailable - the panel stays out then, as the relay card does.
  const [transport, setTransport] = useState<TransportView | null>(null);
  const [transportSaving, setTransportSaving] = useState(false);

  // Load once: re-syncing on every poll would fight the operator's typing.
  useEffect(() => {
    if (addrLoaded || !node.data) return;
    setAdvertiseAddr(node.data.advertise_addr ?? "");
    setNodeName(node.data.node_name ?? "");
    setAddrLoaded(true);
  }, [node.data, addrLoaded]);

  // Read once: the mode of a run is decided at startup, so there is nothing
  // to poll. After a save the screen uses the node's own answer.
  useEffect(() => {
    let cancelled = false;
    void api
      .get<TransportView>("/api/ui/network/transport")
      .then((loaded) => {
        if (!cancelled) setTransport(loaded);
      })
      // Without an answer there is nothing true to show; the panel stays out.
      .catch(() => undefined);
    return () => {
      cancelled = true;
    };
  }, []);

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

  async function saveTransport(noIroh: boolean) {
    setTransportSaving(true);
    try {
      const saved = await api.post<TransportView>("/api/ui/network/transport", { no_iroh: noIroh });
      setTransport(saved);
      // The node says whether the saved mode is what this run is already doing.
      toast.success(
        t("myMachine.transportSaved"),
        saved.restart_needed ? t("myMachine.transportSavedRestart") : t("myMachine.transportSavedLive"),
      );
      invalidate();
    } catch (e) {
      toast.fromError(t("myMachine.transportSaveFailed"), e);
    } finally {
      setTransportSaving(false);
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
                {node.data.validator ? t("lobby.validatorRole") : t(`profile.${node.data.profile}`)}
              </KeyValue>
              {/* The old "Role" (principal/node) left `/api/ui/node` with the
                  trust model and left the cell blank. What exists today
                  is the effective authority, and it answers the same question:
                  what this machine is in charge of. `profile: admin` without a grant
                  shows as having no authority, which is what it is. */}
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

        {transport && (
          <Panel>
            <PanelHeader
              title={t("myMachine.transportTitle")}
              subtitle={t("myMachine.transportSubtitle")}
            />
            <div className="space-y-3 px-5 py-4">
              {/* The same card shape as the relay choices: a saved switch the
                  node answers back, never a draft saved by a button. */}
              <label
                data-testid="transport-direct-only"
                className={`flex items-start gap-3 rounded-lg border px-3 py-3 transition-colors ${transport.no_iroh ? "border-iris/50 bg-iris/[0.08]" : "border-line"} ${transportSaving ? "opacity-60" : "cursor-pointer hover:border-line-strong"}`}
              >
                <input
                  type="checkbox"
                  className="sr-only"
                  checked={transport.no_iroh}
                  disabled={transportSaving}
                  onChange={(event) => void saveTransport(event.target.checked)}
                />
                <span className="min-w-0 flex-1">
                  <span className="block text-sm font-medium text-fg">{t("myMachine.transportLabel")}</span>
                  <span className="mt-0.5 block text-xs leading-relaxed text-muted">{t("myMachine.transportHint")}</span>
                </span>
                <span className={`mt-1 flex h-4 w-4 shrink-0 items-center justify-center rounded border ${transport.no_iroh ? "border-iris bg-iris text-white" : "border-line"}`}>
                  {transport.no_iroh && <Check className="h-3 w-3" />}
                </span>
              </label>
              <KeyValue label={t("myMachine.transportNow")}>
                <span data-testid="transport-state">{t(transportStateKey(transport))}</span>
              </KeyValue>
              {transport.restart_needed && (
                <Footnote icon={<Radio className="h-3.5 w-3.5" />}>{t("myMachine.transportRestartNeeded")}</Footnote>
              )}
            </div>
          </Panel>
        )}

      </div>
    </>
  );
}
