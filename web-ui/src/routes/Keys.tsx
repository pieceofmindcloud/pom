/**
 * The two credentials this product has, kept on separate screens because they
 * do very different jobs.
 *
 * The **connection key** is the cluster's operator credential: it is the same
 * string on every machine, it is what a second machine is given at setup, and
 * it also opens the `/v1` routes. The **API tokens** are what you hand to a
 * harness — one per node, so the Logs screen can say which one paid for what.
 */
import {
  Check,
  Copy,
  Eye,
  EyeOff,
  KeyRound,
  Plus,
  RefreshCw,
  ShieldAlert,
  Trash2,
} from "lucide-react";
import { useState } from "react";
import { Confirm } from "../components/Confirm";
import {
  Badge,
  Button,
  Empty,
  ErrorState,
  Field,
  Input,
  Mono,
  PageHeader,
  Panel,
  PanelHeader,
  Select,
  Skeleton,
} from "../components/ui";
import { api } from "../lib/api";
import { since } from "../lib/format";
import { useApiKeys, useCluster, useNode } from "../lib/queries";
import { useToast } from "../lib/toast";

function Secret({ value, label }: { value: string; label: string }) {
  const [shown, setShown] = useState(false);
  const [copied, setCopied] = useState(false);
  return (
    <div className="flex min-w-0 items-center gap-2">
      <code className="min-w-0 flex-1 truncate rounded-lg border border-line bg-ink px-3 py-2 font-mono text-[0.78rem] text-fg">
        {shown ? value : "•".repeat(Math.min(48, value.length))}
      </code>
      <Button
        size="sm"
        variant="ghost"
        aria-label={shown ? `Esconder ${label}` : `Mostrar ${label}`}
        onClick={() => setShown((s) => !s)}
        icon={shown ? <EyeOff className="h-3.5 w-3.5" /> : <Eye className="h-3.5 w-3.5" />}
      />
      <Button
        size="sm"
        variant="ghost"
        aria-label={`Copiar ${label}`}
        onClick={() => {
          void navigator.clipboard.writeText(value);
          setCopied(true);
          window.setTimeout(() => setCopied(false), 1600);
        }}
        icon={
          copied ? (
            <Check className="h-3.5 w-3.5 text-aqua" />
          ) : (
            <Copy className="h-3.5 w-3.5" />
          )
        }
      />
    </div>
  );
}

// ── API tokens ───────────────────────────────────────────────────────────────

export function TokenKeys() {
  const keys = useApiKeys();
  const cluster = useCluster();
  const toast = useToast();

  const [label, setLabel] = useState("");
  const [nodeId, setNodeId] = useState("");
  const [creating, setCreating] = useState(false);
  const [revoking, setRevoking] = useState<{ key: string; label: string } | null>(null);
  const [busy, setBusy] = useState(false);

  const nameOf = (id: string) =>
    cluster.data?.peers.find((p) => p.node_id === id)?.node?.hostname ??
    (id ? `${id.slice(0, 8)}…` : "toda a rede");

  async function create() {
    setCreating(true);
    try {
      await api.post("/admin/api-keys", {
        label: label.trim() || "sem nome",
        node_id: nodeId || null,
      });
      toast.success("Token criado", "Copie agora: ele fica listado, mas mascarado.");
      setLabel("");
      void keys.refetch();
    } catch (e) {
      toast.fromError("O token não foi criado", e);
    } finally {
      setCreating(false);
    }
  }

  async function revoke(key: string) {
    setBusy(true);
    try {
      await api.del(`/admin/api-keys/${encodeURIComponent(key)}`);
      toast.success("Token revogado");
      void keys.refetch();
    } catch (e) {
      toast.fromError("O token não foi revogado", e);
    } finally {
      setBusy(false);
      setRevoking(null);
    }
  }

  return (
    <>
      <PageHeader
        title="API keys"
        subtitle="Um token por nó, ou por harness. Toda requisição que chega com um token fica atribuída a ele na tela de Logs."
      />

      <div className="grid gap-5 lg:grid-cols-[minmax(0,1fr)_minmax(0,22rem)]">
        <div className="space-y-5">
          {keys.isError && (
            <ErrorState error={keys.error} onRetry={() => void keys.refetch()} />
          )}
          {keys.isLoading && <Skeleton className="h-48 w-full" />}

          {keys.data && (
            <Panel>
              <PanelHeader
                title={`${keys.data.length} ${keys.data.length === 1 ? "token" : "tokens"}`}
                subtitle="Tokens convivem: criar um novo não revoga os anteriores."
                actions={
                  <Button
                    size="sm"
                    icon={<RefreshCw className="h-3.5 w-3.5" />}
                    loading={keys.isFetching}
                    onClick={() => void keys.refetch()}
                  >
                    Atualizar
                  </Button>
                }
              />
              {keys.data.length === 0 ? (
                <Empty icon={<KeyRound className="h-5 w-5" />} title="Nenhum token ainda">
                  Enquanto não houver token, a chave de conexão do cluster é a única
                  credencial que abre as rotas <span className="font-mono">/v1</span>.
                </Empty>
              ) : (
                <div className="divide-y divide-line">
                  {keys.data.map((k) => (
                    <div key={k.key} className="px-5 py-3.5">
                      <div className="flex flex-wrap items-center justify-between gap-3">
                        <div className="flex min-w-0 flex-wrap items-center gap-2">
                          <span className="font-medium text-fg">{k.label}</span>
                          <Badge tone={k.active ? "aqua" : "neutral"}>
                            {k.active ? "ativo" : "inativo"}
                          </Badge>
                          <Badge tone="neutral">{nameOf(k.node_id)}</Badge>
                        </div>
                        <div className="flex shrink-0 items-center gap-3">
                          <span className="text-[0.72rem] text-faint">
                            criado {since(k.created_at)}
                          </span>
                          <Button
                            size="sm"
                            variant="ghost"
                            aria-label={`Revogar ${k.label}`}
                            icon={<Trash2 className="h-3.5 w-3.5" />}
                            onClick={() => setRevoking({ key: k.key, label: k.label })}
                          />
                        </div>
                      </div>
                      <div className="mt-2.5">
                        <Secret value={k.key} label={`token ${k.label}`} />
                      </div>
                    </div>
                  ))}
                </div>
              )}
            </Panel>
          )}
        </div>

        <Panel className="self-start">
          <PanelHeader title="Novo token" />
          <div className="space-y-4 px-5 py-4">
            <Field label="Para que serve" hint="Aparece na tela de Logs em cada requisição.">
              <Input
                value={label}
                onChange={(e) => setLabel(e.target.value)}
                placeholder="opencode, painel, CI…"
              />
            </Field>
            <Field
              label="Nó da rede"
              hint="Serve para saber de onde vieram as chamadas. Não restringe o token a esse nó — qualquer peer aceita qualquer token do cluster."
            >
              <Select value={nodeId} onChange={(e) => setNodeId(e.target.value)}>
                <option value="">Toda a rede</option>
                {(cluster.data?.peers ?? []).map((p) => (
                  <option key={p.node_id} value={p.node_id}>
                    {p.node?.hostname || p.short_id}
                  </option>
                ))}
              </Select>
            </Field>
            <Button
              variant="primary"
              icon={<Plus className="h-3.5 w-3.5" />}
              loading={creating}
              onClick={() => void create()}
            >
              Criar token
            </Button>
          </div>
        </Panel>
      </div>

      <Confirm
        open={revoking !== null}
        title={`Revogar o token ${revoking?.label}?`}
        description="Quem estiver usando este token passa a receber 403 na próxima chamada. Não há como recuperá-lo."
        confirmLabel="Revogar"
        tone="danger"
        busy={busy}
        onConfirm={() => revoking && void revoke(revoking.key)}
        onCancel={() => setRevoking(null)}
      />
    </>
  );
}

// ── Connection key ───────────────────────────────────────────────────────────

export function ConnectionKeys() {
  const node = useNode();
  const toast = useToast();
  const [confirming, setConfirming] = useState(false);
  const [busy, setBusy] = useState(false);

  async function regenerate() {
    setBusy(true);
    try {
      await api.post("/admin/regenerate-key");
      toast.success(
        "Chave trocada",
        "Os outros nós precisam da nova para continuar conversando com este.",
      );
      void node.refetch();
    } catch (e) {
      toast.fromError("A chave não foi trocada", e);
    } finally {
      setBusy(false);
      setConfirming(false);
    }
  }

  return (
    <>
      <PageHeader
        title="Chaves de conexão"
        subtitle="A credencial de operador do cluster: a mesma string em todas as máquinas."
      />

      {node.isLoading && <Skeleton className="h-48 w-full" />}
      {node.isError && <ErrorState error={node.error} onRetry={() => void node.refetch()} />}

      {node.data && (
        <div className="grid gap-5 lg:grid-cols-[minmax(0,1fr)_minmax(0,22rem)]">
          <Panel>
            <PanelHeader
              eyebrow={node.data.hostname}
              title="Chave deste cluster"
              subtitle="É o que se cola na tela de configuração de uma máquina nova para ela entrar na rede."
              actions={
                <Button
                  variant="danger"
                  icon={<RefreshCw className="h-3.5 w-3.5" />}
                  onClick={() => setConfirming(true)}
                >
                  Gerar outra
                </Button>
              }
            />
            <div className="space-y-4 px-5 py-4">
              <Secret value={node.data.connection_key} label="chave de conexão" />
              <dl className="grid gap-4 sm:grid-cols-2">
                <div>
                  <dt className="eyebrow mb-1">Endereço de bootstrap</dt>
                  <dd>
                    <Mono className="text-fg">
                      {node.data.system.hostname}:{node.data.p2p_port}
                    </Mono>
                  </dd>
                </div>
                <div>
                  <dt className="eyebrow mb-1">Base URL da API</dt>
                  <dd>
                    <Mono className="text-fg">
                      http://{node.data.system.hostname}:{node.data.gateway_port}/v1
                    </Mono>
                  </dd>
                </div>
              </dl>
            </div>
          </Panel>

          <Panel className="self-start">
            <PanelHeader title="O que esta chave abre" />
            <ul className="space-y-3 px-5 py-4 text-[0.83rem] leading-relaxed text-muted">
              <li>
                As rotas de administração (<span className="font-mono">/admin/*</span>) e de
                configuração de cluster deste nó e dos peers.
              </li>
              <li>
                As rotas compatíveis com OpenAI (<span className="font-mono">/v1</span>) —
                por isso o chat de teste funciona sem criar token nenhum.
              </li>
              <li className="flex gap-2.5 pt-1">
                <ShieldAlert className="mt-0.5 h-4 w-4 shrink-0 text-amber" />
                Trocar a chave aqui muda só esta máquina. As outras continuam com a antiga
                até serem atualizadas, e param de aceitar as chamadas desta.
              </li>
            </ul>
          </Panel>
        </div>
      )}

      <Confirm
        open={confirming}
        title="Gerar outra chave de conexão?"
        description="A chave atual deixa de valer nesta máquina imediatamente. Todos os outros nós e harnesses precisam receber a nova, senão o cluster se parte."
        confirmLabel="Gerar outra"
        tone="danger"
        busy={busy}
        onConfirm={() => void regenerate()}
        onCancel={() => setConfirming(false)}
      />
    </>
  );
}
