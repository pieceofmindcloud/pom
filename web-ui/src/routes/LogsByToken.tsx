/**
 * Who asked, and which machines answered.
 *
 * The node keeps the last few hundred generations in memory (`src/reqlog.rs`),
 * with the credential that paid for each one and the hops it actually took. It
 * is a rear-view mirror, not an audit trail: a node that restarts starts empty,
 * and the screen says so rather than implying completeness.
 */
import { KeyRound, Radio, Server } from "lucide-react";
import { useState } from "react";
import { Link } from "react-router-dom";
import {
  Badge,
  Empty,
  ErrorState,
  Mono,
  PageHeader,
  Panel,
  PanelHeader,
  ScrollX,
  Select,
  Skeleton,
} from "../components/ui";
import { clock, ms, num, since } from "../lib/format";
import { useCluster, useRequestLog } from "../lib/queries";
import type { RequestLogEntry } from "../lib/types";

/** Peer names, so a log line reads as machines rather than as hex. */
function useNodeNames() {
  const cluster = useCluster();
  const map = new Map<string, string>();
  for (const p of cluster.data?.peers ?? []) {
    map.set(p.node_id, p.node?.hostname || p.short_id);
  }
  return (id: string) => map.get(id) ?? `${id.slice(0, 8)}…`;
}

export function HopChain({ entry, name }: { entry: RequestLogEntry; name: (id: string) => string }) {
  if (entry.hops.length === 0) {
    return <span className="text-[0.78rem] text-faint">sem pipeline</span>;
  }
  return (
    <span className="flex flex-wrap items-center gap-1">
      {entry.hops.map((h, i) => (
        <span key={`${h}-${i}`} className="flex items-center gap-1">
          {i > 0 && <span className="text-faint">→</span>}
          <span className="font-mono text-[0.72rem] text-fg">{name(h)}</span>
        </span>
      ))}
    </span>
  );
}

function Tally({
  title,
  icon,
  rows,
  emptyLabel,
}: {
  title: string;
  icon: React.ReactNode;
  rows: Array<[string, number]>;
  emptyLabel: string;
}) {
  const max = Math.max(1, ...rows.map(([, n]) => n));
  return (
    <Panel>
      <PanelHeader title={title} eyebrow={<span className="flex items-center gap-1.5">{icon}</span>} />
      {rows.length === 0 ? (
        <p className="px-5 py-6 text-center text-[0.82rem] text-faint">{emptyLabel}</p>
      ) : (
        <ul className="space-y-2.5 px-5 py-4">
          {rows.map(([label, count]) => (
            <li key={label} className="min-w-0">
              <div className="mb-1 flex items-baseline justify-between gap-3">
                <span className="truncate text-[0.82rem] text-fg">{label}</span>
                <Mono className="shrink-0 text-muted">{num(count)}</Mono>
              </div>
              <div className="h-1.5 w-full overflow-hidden rounded-full bg-line">
                <div
                  className="h-full rounded-full bg-iris transition-[width] duration-500 ease-swift"
                  style={{ width: `${(count / max) * 100}%` }}
                />
              </div>
            </li>
          ))}
        </ul>
      )}
    </Panel>
  );
}

export function LogsByToken() {
  const [nodeFilter, setNodeFilter] = useState("");
  const cluster = useCluster();
  const log = useRequestLog(nodeFilter ? { node_id: nodeFilter } : {});
  const name = useNodeNames();

  const data = log.data as
    | {
        entries: RequestLogEntry[];
        total: number;
        errors?: number;
        by_token?: Record<string, number>;
        by_node?: Record<string, number>;
      }
    | undefined;

  const byToken = Object.entries(data?.by_token ?? {}).sort((a, b) => b[1] - a[1]);
  const byNode = Object.entries(data?.by_node ?? {})
    .sort((a, b) => b[1] - a[1])
    .map(([id, n]) => [name(id), n] as [string, number]);

  return (
    <>
      <PageHeader
        title="Requisições por token e por nó"
        subtitle="O que a rede foi chamada a gerar, com a credencial que pagou por cada chamada e as máquinas que participaram."
        actions={
          <Select
            className="w-56"
            value={nodeFilter}
            onChange={(e) => setNodeFilter(e.target.value)}
            aria-label="Filtrar por nó"
          >
            <option value="">Todos os nós</option>
            {(cluster.data?.peers ?? []).map((p) => (
              <option key={p.node_id} value={p.node_id}>
                {p.node?.hostname || p.short_id}
              </option>
            ))}
          </Select>
        }
      />

      {log.isError && <ErrorState error={log.error} onRetry={() => void log.refetch()} />}
      {log.isLoading && <Skeleton className="h-64 w-full" />}

      {data && (
        <div className="space-y-5">
          <div className="grid gap-5 lg:grid-cols-2">
            <Tally
              title="Por API token"
              icon={<KeyRound className="h-3 w-3" />}
              rows={byToken}
              emptyLabel="Nenhuma requisição registrada ainda."
            />
            <Tally
              title="Por nó da rede"
              icon={<Server className="h-3 w-3" />}
              rows={byNode}
              emptyLabel="Nenhum salto registrado ainda."
            />
          </div>

          <Panel>
            <PanelHeader
              title={`${num(data.total)} ${data.total === 1 ? "requisição" : "requisições"} na memória deste nó`}
              subtitle="O registro vive na memória do processo: reiniciar o nó começa do zero. Só as mais recentes ficam."
              actions={
                data.errors ? (
                  <Badge tone="rose">
                    {num(data.errors)} {data.errors === 1 ? "erro" : "erros"}
                  </Badge>
                ) : undefined
              }
            />
            {data.entries.length === 0 ? (
              <Empty icon={<Radio className="h-5 w-5" />} title="Nada registrado ainda">
                Assim que a rede atender uma requisição, ela aparece aqui. Você pode
                provocar uma no{" "}
                <Link to="/chat" className="text-iris hover:underline">
                  chat de teste
                </Link>
                .
              </Empty>
            ) : (
              <ScrollX>
                <table className="w-full min-w-[52rem] border-collapse text-[0.82rem]">
                  <thead>
                    <tr className="border-b border-line text-left">
                      {["Hora", "Token", "Rota", "Caminho", "Tokens", "Duração", ""].map(
                        (h) => (
                          <th key={h} className="px-4 py-2.5 font-medium text-muted">
                            {h}
                          </th>
                        ),
                      )}
                    </tr>
                  </thead>
                  <tbody>
                    {data.entries.map((e) => (
                      <tr
                        key={e.id}
                        className="border-b border-line last:border-0 hover:bg-raised"
                      >
                        <td className="whitespace-nowrap px-4 py-2.5">
                          <Mono className="text-fg">{clock(e.at)}</Mono>
                          <span className="ml-2 text-[0.7rem] text-faint">
                            {since(e.at)}
                          </span>
                        </td>
                        <td className="px-4 py-2.5">
                          <span className="text-fg">{e.token_label}</span>
                          {e.token_hint && (
                            <Mono className="ml-1.5 text-faint">{e.token_hint}</Mono>
                          )}
                          {e.token_node_id && (
                            <span className="ml-1.5 text-[0.7rem] text-faint">
                              · {name(e.token_node_id)}
                            </span>
                          )}
                        </td>
                        <td className="px-4 py-2.5">
                          <Mono className="text-muted">{e.route}</Mono>
                        </td>
                        <td className="px-4 py-2.5">
                          <HopChain entry={e} name={name} />
                        </td>
                        <td className="whitespace-nowrap px-4 py-2.5">
                          <Mono className="text-muted">
                            {e.prompt_tokens ?? "—"} → {e.completion_tokens ?? "—"}
                          </Mono>
                        </td>
                        <td className="whitespace-nowrap px-4 py-2.5">
                          <Mono className="text-muted">{ms(e.duration_ms)}</Mono>
                        </td>
                        <td className="px-4 py-2.5">
                          <Badge
                            tone={
                              e.status === "ok"
                                ? "aqua"
                                : e.status === "partial"
                                  ? "amber"
                                  : "rose"
                            }
                          >
                            {e.status}
                          </Badge>
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </ScrollX>
            )}
          </Panel>
        </div>
      )}
    </>
  );
}
