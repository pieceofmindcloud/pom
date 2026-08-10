import { Check, Download, Inbox, Upload, X } from "lucide-react";
import { Link } from "react-router-dom";
import {
  Badge,
  Button,
  Empty,
  ErrorState,
  Footnote,
  PageHeader,
  Panel,
  PanelHeader,
  Progress,
  Skeleton,
} from "../components/ui";
import { api } from "../lib/api";
import { bytes, duration, layerRange, rate } from "../lib/format";
import { useOffers, useTransfers } from "../lib/queries";
import { useToast } from "../lib/toast";

export function Transfers() {
  const transfers = useTransfers();
  const offers = useOffers();
  const toast = useToast();

  const jobs = transfers.data?.jobs ?? [];
  const active = jobs.filter((j) => j.state !== "done" && j.state !== "failed");
  const finished = jobs.filter((j) => j.state === "done" || j.state === "failed");
  const waiting = offers.data?.offers ?? [];

  async function answer(id: string, accept: boolean) {
    try {
      await api.post(`/api/transfer/offers/${id}/${accept ? "accept" : "reject"}`);
      toast.success(accept ? "Transferência iniciada" : "Oferta recusada");
      void offers.refetch();
      void transfers.refetch();
    } catch (e) {
      toast.fromError("A oferta não pôde ser respondida", e);
    }
  }

  return (
    <>
      <PageHeader
        title="Transferências"
        subtitle="Uma fatia de 10 GB leva minutos no enlace real, então tudo aqui retoma de onde parou e confere o MD5 antes de o arquivo tomar o nome final."
        actions={
          <Link to="/transferencias/enviar">
            <Button variant="primary" icon={<Upload className="h-3.5 w-3.5" />}>
              Enviar um arquivo
            </Button>
          </Link>
        }
      />

      {(transfers.isError || offers.isError) && (
        <ErrorState
          error={transfers.error ?? offers.error}
          onRetry={() => void transfers.refetch()}
        />
      )}

      <div className="space-y-5">
        {waiting.length > 0 && (
          <Panel>
            <PanelHeader
              eyebrow="Precisa da sua resposta"
              title={`${waiting.length} ${waiting.length === 1 ? "oferta" : "ofertas"} de outra máquina`}
              subtitle="Nada é gravado no disco desta máquina antes de você aceitar."
            />
            <div className="divide-y divide-line">
              {waiting.map((o) => (
                <div
                  key={o.id}
                  className="flex flex-wrap items-center gap-x-4 gap-y-3 px-5 py-3.5"
                >
                  <div className="min-w-[14rem] flex-1">
                    <div className="flex flex-wrap items-center gap-2">
                      <span className="font-medium text-fg">{o.name}</span>
                      {o.start_layer != null && o.end_layer != null && (
                        <Badge mono tone="iris">
                          {layerRange(o.start_layer, o.end_layer)}
                        </Badge>
                      )}
                    </div>
                    <p className="break-path mt-0.5 text-[0.78rem] text-muted">
                      de <span className="text-fg">{o.from_host}</span> · {bytes(o.size_bytes)}{" "}
                      · <span className="font-mono text-faint">{o.remote_path}</span>
                    </p>
                  </div>
                  <div className="flex shrink-0 gap-2">
                    <Button
                      variant="primary"
                      size="sm"
                      icon={<Check className="h-3.5 w-3.5" />}
                      onClick={() => void answer(o.id, true)}
                    >
                      Aceitar
                    </Button>
                    <Button
                      variant="ghost"
                      size="sm"
                      icon={<X className="h-3.5 w-3.5" />}
                      onClick={() => void answer(o.id, false)}
                    >
                      Recusar
                    </Button>
                  </div>
                </div>
              ))}
            </div>
          </Panel>
        )}

        <Panel>
          {/* Only worth a header when there is something under it; otherwise the
              empty state below already carries the whole message. */}
          {(transfers.isLoading || active.length > 0) && (
            <PanelHeader
              title={
                active.length
                  ? `${active.length} ${active.length === 1 ? "transferência" : "transferências"} em andamento`
                  : "Nada em andamento"
              }
              subtitle="O andamento vem do nó, então recarregar a página não perde a barra."
            />
          )}
          {transfers.isLoading ? (
            <div className="space-y-3 p-5">
              <Skeleton className="h-12 w-full" />
            </div>
          ) : active.length === 0 ? (
            <Empty icon={<Download className="h-5 w-5" />} title="Nada em andamento">
              Os arquivos recebidos vão para a subpasta <span className="font-mono">recebidos/</span>{" "}
              da pasta de fatias configurada.
            </Empty>
          ) : (
            <div className="divide-y divide-line">
              {active.map((t) => (
                <div key={t.id} className="px-5 py-4">
                  <div className="flex flex-wrap items-baseline justify-between gap-x-4 gap-y-1">
                    <span className="font-medium text-fg">{t.name}</span>
                    <span className="font-mono text-[0.78rem] text-muted">
                      {bytes(t.bytes_done)} de {bytes(t.bytes_total)}
                    </span>
                  </div>
                  <Progress
                    className="mt-2.5"
                    tone="aqua"
                    value={t.bytes_total > 0 ? (t.bytes_done / t.bytes_total) * 100 : null}
                  />
                  <p className="mt-2 flex flex-wrap gap-x-4 font-mono text-[0.7rem] text-faint">
                    <span>de {t.from_host}</span>
                    <span>{rate(t.bytes_per_sec)}</span>
                    {t.eta_secs != null && <span>faltam {duration(t.eta_secs)}</span>}
                    <span>{t.detail}</span>
                  </p>
                </div>
              ))}
            </div>
          )}
        </Panel>

        {finished.length > 0 && (
          <Panel>
            <PanelHeader title="Terminadas" />
            <div className="divide-y divide-line">
              {finished.map((t) => (
                <div
                  key={t.id}
                  className="flex flex-wrap items-center justify-between gap-x-4 gap-y-1 px-5 py-3"
                >
                  <span className="min-w-0 truncate text-[0.88rem] text-fg">{t.name}</span>
                  <div className="flex shrink-0 items-center gap-3">
                    <span className="font-mono text-[0.75rem] text-faint">
                      {bytes(t.bytes_total)}
                    </span>
                    <Badge tone={t.state === "done" ? "aqua" : "rose"}>
                      {t.state === "done" ? "concluída" : t.detail || "falhou"}
                    </Badge>
                  </div>
                </div>
              ))}
            </div>
          </Panel>
        )}
      </div>

      {waiting.length === 0 && jobs.length === 0 && (
        <Footnote
          className="mt-4"
          icon={<Inbox className="h-3.5 w-3.5" />}
        >
          Ofertas recebidas também aparecem no sino, no topo da tela.
        </Footnote>
      )}
    </>
  );
}
