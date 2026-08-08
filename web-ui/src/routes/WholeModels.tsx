import { Boxes, FolderSearch, RefreshCw, Scissors, Upload } from "lucide-react";
import { Link } from "react-router-dom";
import {
  Badge,
  Button,
  Empty,
  ErrorState,
  PageHeader,
  Panel,
  PanelHeader,
  Skeleton,
} from "../components/ui";
import { bytes, num } from "../lib/format";
import { useLocalCatalog } from "../lib/queries";
import type { DiscoveredModel, ModelSource } from "../lib/types";

const SOURCE_TONE: Record<string, "sky" | "amber" | "neutral"> = {
  lmstudio: "sky",
  ollama: "amber",
};

export function ModelRow({
  model,
  actions,
}: {
  model: DiscoveredModel;
  actions?: React.ReactNode;
}) {
  return (
    <div className="flex flex-wrap items-center gap-x-4 gap-y-2 px-5 py-3.5 transition-colors duration-150 hover:bg-raised">
      <div className="min-w-[14rem] flex-1">
        <div className="flex flex-wrap items-center gap-2">
          <span className="truncate font-medium text-fg" title={model.name}>
            {model.name}
          </span>
          <Badge tone={SOURCE_TONE[model.source] ?? "neutral"}>{model.source_label}</Badge>
          {model.quant && (
            <Badge mono tone="neutral">
              {model.quant}
            </Badge>
          )}
        </div>
        <p className="break-path mt-1 font-mono text-[0.7rem] text-faint">{model.path}</p>
      </div>

      <dl className="flex shrink-0 items-center gap-5">
        <div>
          <dt className="eyebrow">Tamanho</dt>
          <dd className="font-mono text-[0.82rem] text-fg">{bytes(model.size_bytes)}</dd>
        </div>
        <div>
          <dt className="eyebrow">Camadas</dt>
          <dd className="font-mono text-[0.82rem] text-fg">
            {model.total_layers != null ? num(model.total_layers) : "—"}
          </dd>
        </div>
        <div className="hidden sm:block">
          <dt className="eyebrow">Arquitetura</dt>
          <dd className="text-[0.82rem] text-fg">{model.architecture ?? "—"}</dd>
        </div>
      </dl>

      {actions && <div className="flex shrink-0 items-center gap-2">{actions}</div>}
    </div>
  );
}

export function ScannedRoots({
  roots,
}: {
  roots: Array<{ path: string; exists: boolean; found: number; source: ModelSource }>;
}) {
  return (
    <div className="border-t border-line px-5 py-3">
      <div className="eyebrow mb-2">Pastas varridas</div>
      <ul className="space-y-1">
        {roots.map((r) => (
          <li
            key={r.path}
            className="flex flex-wrap items-baseline justify-between gap-x-3 gap-y-0.5"
          >
            <span className="break-path font-mono text-[0.72rem] text-muted">{r.path}</span>
            <span className="shrink-0 text-[0.72rem] text-faint">
              {r.exists
                ? `${r.found} ${r.found === 1 ? "arquivo" : "arquivos"}`
                : "não existe nesta máquina"}
            </span>
          </li>
        ))}
      </ul>
      <p className="mt-2.5 text-xs text-faint">
        Escolha onde procurar em{" "}
        <Link to="/config/pastas" className="text-iris hover:underline">
          Configurações › Pastas de modelos
        </Link>
        .
      </p>
    </div>
  );
}

export function WholeModels() {
  const catalog = useLocalCatalog();
  const models = catalog.data?.models ?? [];

  return (
    <>
      <PageHeader
        title="Modelos inteiros"
        subtitle="Os GGUF completos encontrados nesta máquina. Um modelo inteiro é o que se fatia; as fatias ficam na tela ao lado."
        actions={
          <Button
            icon={<RefreshCw className="h-3.5 w-3.5" />}
            loading={catalog.isFetching}
            onClick={() => void catalog.refetch()}
          >
            Varrer de novo
          </Button>
        }
      />

      {catalog.isError && (
        <ErrorState error={catalog.error} onRetry={() => void catalog.refetch()} />
      )}

      {catalog.isLoading && (
        <Panel className="space-y-3 p-5">
          {[0, 1, 2].map((i) => (
            <Skeleton key={i} className="h-14 w-full" />
          ))}
        </Panel>
      )}

      {catalog.data && (
        <Panel>
          <PanelHeader
            eyebrow={catalog.data.hostname}
            title={`${models.length} ${models.length === 1 ? "modelo" : "modelos"} nesta máquina`}
            subtitle="Varrer um volume lento leva tempo na primeira vez; o resultado anterior fica na tela enquanto isso."
          />
          {models.length === 0 ? (
            <Empty
              icon={<FolderSearch className="h-5 w-5" />}
              title="Nenhum modelo inteiro encontrado"
              action={
                <Link to="/config/pastas">
                  <Button variant="primary">Escolher as pastas</Button>
                </Link>
              }
            >
              A varredura cobriu as pastas listadas abaixo e não achou nenhum `.gguf`
              completo.
            </Empty>
          ) : (
            <div className="divide-y divide-line">
              {models.map((m) => (
                <ModelRow
                  key={m.id}
                  model={m}
                  actions={
                    <>
                      <Link
                        to={`/modelos/fatiar?path=${encodeURIComponent(m.path)}&name=${encodeURIComponent(m.name)}`}
                      >
                        <Button size="sm" icon={<Scissors className="h-3.5 w-3.5" />}>
                          Fatiar
                        </Button>
                      </Link>
                      <Link
                        to={`/transferencias/enviar?path=${encodeURIComponent(m.path)}`}
                      >
                        <Button
                          size="sm"
                          variant="ghost"
                          icon={<Upload className="h-3.5 w-3.5" />}
                          aria-label={`Enviar ${m.name}`}
                        >
                          Enviar
                        </Button>
                      </Link>
                    </>
                  }
                />
              ))}
            </div>
          )}
          {catalog.data.scanned.length > 0 && (
            <ScannedRoots roots={catalog.data.scanned} />
          )}
        </Panel>
      )}

      {catalog.data && models.length > 0 && (
        <p className="mt-4 flex items-center gap-2 text-xs text-faint">
          <Boxes className="h-3.5 w-3.5 shrink-0" />
          Fatiar é local: cada máquina extrai a própria faixa da própria cópia do GGUF.
          Nenhum peso trafega pela rede.
        </p>
      )}
    </>
  );
}
