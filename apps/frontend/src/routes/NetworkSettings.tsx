/**
 * The network as an administrator sees it: who is connected, who is blocked,
 * and whether this machine is even taking part.
 */
import {
  Ban,
  LogIn,
  LogOut,
  Lock,
  Network,
  Radio,
  Save,
  ShieldCheck,
  ShieldOff,
  Undo2,
} from "lucide-react";
import { useEffect, useState } from "react";
import { Confirm } from "../components/Confirm";
import { HuggingFaceSettingsPanel } from "../components/HuggingFaceSettingsPanel";
import { MachineIcon } from "../components/MachineIcon";
import {
  Badge,
  Button,
  Dot,
  Empty,
  ErrorState,
  Field,
  Footnote,
  Input,
  KeyValue,
  Mono,
  PageHeader,
  Panel,
  PanelHeader,
  Skeleton,
} from "../components/ui";
import { api } from "../lib/api";
import { layerRange } from "../lib/format";
import { useAdminNodes, useCluster, useInvalidate, useNode } from "../lib/queries";
import { useToast } from "../lib/toast";
import type { NodeInfo } from "../lib/types";
import { useI18n } from "../i18n/provider";

/**
 * A criptografia do cluster, e quem manda nela.
 *
 * O interruptor só é editável no nó principal - a decisão é dele e vale para a
 * rede inteira. Nos demais o painel aparece em leitura, porque esconder o
 * estado faria o operador de um nó comum não ter onde ver se o tráfego da
 * máquina dele está cifrado. A recusa que vale é a do servidor (`save_tls`
 * responde 403); isto aqui é só a tela dizendo a verdade antes do clique.
 */
export function TlsPanel({
  tlsRequired,
  authority,
  busy = false,
  onToggle,
}: {
  tlsRequired: boolean;
  authority: boolean;
  busy?: boolean;
  onToggle?: (next: boolean) => void;
}) {
  const { t } = useI18n();
  return (
    <Panel>
      <PanelHeader
        title={t("network.tlsTitle")}
        subtitle={t("network.tlsSubtitle")}
        actions={
          <Button
            variant={tlsRequired ? "danger" : "primary"}
            role="switch"
            aria-checked={tlsRequired}
            disabled={!authority || busy}
            loading={busy}
            icon={
              tlsRequired ? (
                <ShieldOff className="h-3.5 w-3.5" />
              ) : (
                <Lock className="h-3.5 w-3.5" />
              )
            }
            onClick={() => onToggle?.(!tlsRequired)}
          >
            {t(tlsRequired ? "network.tlsDisable" : "network.tlsEnable")}
          </Button>
        }
      />
      <div className="space-y-3 px-5 py-4">
        <KeyValue label={t("network.tlsState")}>
          <Badge tone={tlsRequired ? "aqua" : "amber"}>
            {tlsRequired ? (
              <ShieldCheck className="h-3.5 w-3.5" />
            ) : (
              <ShieldOff className="h-3.5 w-3.5" />
            )}
            {t(tlsRequired ? "network.tlsOn" : "network.tlsOff")}
          </Badge>
        </KeyValue>
        <Footnote icon={<ShieldCheck className="h-3.5 w-3.5" />}>
          {t("network.tlsHowItWorks")}
        </Footnote>
        {!authority && (
          <Footnote icon={<Ban className="h-3.5 w-3.5" />}>
            {t("network.tlsNotAuthority")}
          </Footnote>
        )}
      </div>
    </Panel>
  );
}

export function NetworkSettings() {
  const { t } = useI18n();
  const node = useNode();
  const cluster = useCluster();
  const nodes = useAdminNodes();
  const toast = useToast();
  const invalidate = useInvalidate();

  const [pending, setPending] = useState<{ id: string; hostname: string } | null>(null);
  const [busy, setBusy] = useState(false);

  const [tlsSaving, setTlsSaving] = useState(false);

  async function toggleTls(next: boolean) {
    setTlsSaving(true);
    try {
      await api.post("/api/ui/network/tls", { tls_required: next });
      toast.success(
        t(next ? "network.tlsEnabled" : "network.tlsDisabled"),
        t("network.tlsPropagates"),
      );
      invalidate();
    } catch (e) {
      toast.fromError(t("network.tlsFailed"), e);
    } finally {
      setTlsSaving(false);
    }
  }

  const [advertiseAddr, setAdvertiseAddr] = useState("");
  const [addrSaving, setAddrSaving] = useState(false);
  const [addrLoaded, setAddrLoaded] = useState(false);

  // Load once: re-syncing on every poll would fight the operator's typing.
  useEffect(() => {
    if (addrLoaded || !node.data) return;
    setAdvertiseAddr(node.data.advertise_addr ?? "");
    setAddrLoaded(true);
  }, [node.data, addrLoaded]);

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

  const detached = node.data?.detached ?? false;

  /** Hostnames come from the cluster view; the admin list carries only ids. */
  const systemOf = (id: string) =>
    cluster.data?.peers.find((p) => p.node_id === id)?.node?.system ?? null;

  async function setBlocked(id: string, blocked: boolean) {
    setBusy(true);
    try {
      await api.post(`/admin/nodes/${id}/${blocked ? "block" : "unblock"}`);
      toast.success(blocked ? t("network.blocked") : t("network.unblocked"));
      void nodes.refetch();
    } catch (e) {
      toast.fromError(blocked ? t("network.blockFailed") : t("network.unblockFailed"), e);
    } finally {
      setBusy(false);
      setPending(null);
    }
  }

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

  return (
    <>
      <PageHeader
        title={t("network.title")}
        subtitle={t("network.subtitle")}
      />

      <div className="space-y-5">
        <Panel>
          <PanelHeader
            eyebrow={t("network.thisMachine")}
            title={node.data?.hostname ?? "—"}
            subtitle={
              detached
                ? t("network.offlineDetail")
                : t("network.onlineDetail")
            }
            actions={
              <Button
                variant={detached ? "primary" : "danger"}
                loading={busy}
                icon={
                  detached ? <LogIn className="h-3.5 w-3.5" /> : <LogOut className="h-3.5 w-3.5" />
                }
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
              <KeyValue label={t("network.role")}>{node.data.role}</KeyValue>
            </dl>
          )}
        </Panel>

        <TlsPanel
          tlsRequired={node.data?.tls_required ?? false}
          authority={node.data?.tls_authority ?? false}
          busy={tlsSaving}
          onToggle={(next) => void toggleTls(next)}
        />

        <HuggingFaceSettingsPanel />

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

        {nodes.isError && (
          <ErrorState
            title={t("network.listFailed")}
            error={nodes.error}
            onRetry={() => void nodes.refetch()}
          />
        )}
        {nodes.isLoading && <Skeleton className="h-48 w-full" />}

        {nodes.data && (
          <Panel>
            <PanelHeader
              title={t("network.knownCount", { count: nodes.data.length })}
              subtitle={t("network.knownDetail")}
            />
            {nodes.data.length === 0 ? (
              <Empty icon={<Network className="h-5 w-5" />} title={t("network.noNodes")}>
                {t("network.noNodesDetail")}
              </Empty>
            ) : (
              <div className="divide-y divide-line">
                {nodes.data.map((n) => (
                  <div
                    key={n.node_id}
                    className="flex flex-wrap items-center gap-x-4 gap-y-2 px-5 py-3.5"
                  >
                    <MachineIcon system={systemOf(n.node_id)} size="sm" />
                    <div className="min-w-[12rem] flex-1">
                      <div className="flex flex-wrap items-center gap-2">
                        <span className="font-medium text-fg">
                          {n.hostname || n.short_id}
                        </span>
                        {n.is_self && (
                          <Badge tone="neutral" className="px-1.5 py-0 text-[0.62rem]">
                            {t("network.here")}
                          </Badge>
                        )}
                        {n.blocked && <Badge tone="rose">{t("network.blockedLabel")}</Badge>}
                        {n.verified && <Badge tone="aqua">{t("network.verified")}</Badge>}
                      </div>
                      <p className="mt-0.5 font-mono text-[0.7rem] text-faint">
                        {n.short_id} · {n.addr || t("network.noAddress")}
                      </p>
                    </div>
                    {n.total_layers ? (
                      <Mono className="shrink-0 text-iris">
                        {layerRange(n.start_layer ?? 0, n.end_layer ?? 0)}
                      </Mono>
                    ) : null}
                    <span className="flex shrink-0 items-center gap-1.5 text-[0.78rem] text-muted">
                      <Dot tone={n.connected ? "aqua" : "neutral"} />
                      {n.connected ? t("network.connected") : t("network.away")}
                    </span>
                    {!n.is_self && (
                      <Button
                        size="sm"
                        variant={n.blocked ? "outline" : "danger"}
                        icon={
                          n.blocked ? (
                            <Undo2 className="h-3.5 w-3.5" />
                          ) : (
                            <Ban className="h-3.5 w-3.5" />
                          )
                        }
                        onClick={() =>
                          n.blocked
                            ? void setBlocked(n.node_id, false)
                            : setPending({ id: n.node_id, hostname: n.hostname || n.short_id })
                        }
                      >
                        {n.blocked ? t("network.unblock") : t("network.block")}
                      </Button>
                    )}
                  </div>
                ))}
              </div>
            )}
            <Footnote
              className="border-t border-line px-5 py-3"
              icon={<ShieldOff className="h-3.5 w-3.5" />}
            >
              {t("network.identityNote")}
            </Footnote>
          </Panel>
        )}
      </div>

      <Confirm
        open={pending !== null}
        title={t("network.blockTitle", { hostname: pending?.hostname ?? "" })}
        description={t("network.blockDescription")}
        confirmLabel={t("network.block")}
        tone="danger"
        busy={busy}
        onConfirm={() => pending && void setBlocked(pending.id, true)}
        onCancel={() => setPending(null)}
      />
    </>
  );
}
