/**
 * The network as an administrator sees it: who is connected, who is blocked,
 * and whether this machine is even taking part.
 */
import { Ban, LogIn, LogOut, Network, ShieldOff, Undo2 } from "lucide-react";
import { useState } from "react";
import { Confirm } from "../components/Confirm";
import { MachineIcon } from "../components/MachineIcon";
import {
  Badge,
  Button,
  Dot,
  Empty,
  ErrorState,
  Footnote,
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

export function NetworkSettings() {
  const node = useNode();
  const cluster = useCluster();
  const nodes = useAdminNodes();
  const toast = useToast();
  const invalidate = useInvalidate();

  const [pending, setPending] = useState<{ id: string; hostname: string } | null>(null);
  const [busy, setBusy] = useState(false);

  const detached = node.data?.detached ?? false;

  /** Hostnames come from the cluster view; the admin list carries only ids. */
  const systemOf = (id: string) =>
    cluster.data?.peers.find((p) => p.node_id === id)?.node?.system ?? null;

  async function setBlocked(id: string, blocked: boolean) {
    setBusy(true);
    try {
      await api.post(`/admin/nodes/${id}/${blocked ? "block" : "unblock"}`);
      toast.success(blocked ? "Nó bloqueado" : "Nó liberado");
      void nodes.refetch();
    } catch (e) {
      toast.fromError(blocked ? "O nó não foi bloqueado" : "O nó não foi liberado", e);
    } finally {
      setBusy(false);
      setPending(null);
    }
  }

  async function toggleMembership() {
    setBusy(true);
    try {
      await api.post(detached ? "/api/ui/rejoin" : "/api/ui/leave");
      toast.success(detached ? "De volta à rede" : "Fora da rede");
      invalidate();
    } catch (e) {
      toast.fromError("A operação não foi aceita pelo nó", e);
    } finally {
      setBusy(false);
    }
  }

  return (
    <>
      <PageHeader
        title="Rede"
        subtitle="Quem este nó enxerga, e quem ele se recusa a atender."
      />

      <div className="space-y-5">
        <Panel>
          <PanelHeader
            eyebrow="Esta máquina"
            title={node.data?.hostname ?? "—"}
            subtitle={
              detached
                ? "Fora da rede: parou de anunciar e esqueceu os peers que conhecia."
                : "Na rede, anunciando a cada cinco segundos."
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
                {detached ? "Voltar para a rede" : "Sair da rede"}
              </Button>
            }
          />
          {node.data && (
            <dl className="grid gap-x-6 gap-y-4 px-5 py-4 sm:grid-cols-3">
              <KeyValue label="node_id">
                <Mono>{node.data.node_id}</Mono>
              </KeyValue>
              <KeyValue label="Portas">
                <Mono>
                  P2P {node.data.p2p_port} · gateway {node.data.gateway_port} · inferência{" "}
                  {node.data.inference_port ?? "—"}
                </Mono>
              </KeyValue>
              <KeyValue label="Papel">{node.data.role}</KeyValue>
            </dl>
          )}
        </Panel>

        {nodes.isError && (
          <ErrorState
            title="A lista de nós não veio"
            error={nodes.error}
            onRetry={() => void nodes.refetch()}
          />
        )}
        {nodes.isLoading && <Skeleton className="h-48 w-full" />}

        {nodes.data && (
          <Panel>
            <PanelHeader
              title={`${nodes.data.length} ${nodes.data.length === 1 ? "nó conhecido" : "nós conhecidos"}`}
              subtitle="Bloquear um nó exclui sua identidade do painel, da implantação e da inferência deste processo."
            />
            {nodes.data.length === 0 ? (
              <Empty icon={<Network className="h-5 w-5" />} title="Nenhum nó conhecido">
                Nem esta máquina aparece, o que normalmente quer dizer que ela não está
                anunciando.
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
                            aqui
                          </Badge>
                        )}
                        {n.blocked && <Badge tone="rose">bloqueado</Badge>}
                        {n.verified && <Badge tone="aqua">verificado</Badge>}
                      </div>
                      <p className="mt-0.5 font-mono text-[0.7rem] text-faint">
                        {n.short_id} · {n.addr || "sem endereço"}
                      </p>
                    </div>
                    {n.total_layers ? (
                      <Mono className="shrink-0 text-iris">
                        {layerRange(n.start_layer ?? 0, n.end_layer ?? 0)}
                      </Mono>
                    ) : null}
                    <span className="flex shrink-0 items-center gap-1.5 text-[0.78rem] text-muted">
                      <Dot tone={n.connected ? "aqua" : "neutral"} />
                      {n.connected ? "conectado" : "ausente"}
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
                        {n.blocked ? "Liberar" : "Bloquear"}
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
              A identidade de uma máquina é o node_id, guardado em{" "}
              <span className="font-mono">.rdma-p2p-node.key</span>. Apagar esse arquivo cria
              uma máquina nova aos olhos dos outros peers — e o bloqueio deixa de valer.
            </Footnote>
          </Panel>
        )}
      </div>

      <Confirm
        open={pending !== null}
        title={`Bloquear ${pending?.hostname}?`}
        description="Este nó deixa de usar a identidade dessa máquina no painel, implantação e inferência. Se ela estiver servindo camadas agora, o pipeline se abre e as requisições param de completar."
        confirmLabel="Bloquear"
        tone="danger"
        busy={busy}
        onConfirm={() => pending && void setBlocked(pending.id, true)}
        onCancel={() => setPending(null)}
      />
    </>
  );
}
