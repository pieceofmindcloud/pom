/**
 * Giving this machine's memory back, on the machine's own terms.
 *
 * The decision belongs to the node, not to a coordinator: the operator sitting
 * at a machine that suddenly needs its RAM says so here, and the cluster deals
 * with the consequence. So the release is never refused — the only thing this
 * screen owes the operator is an honest account of what happens next.
 *
 * That account is fetched, not guessed. Asking the node who *would* take the
 * slice over means the confirmation can say "another machine picks it up" or
 * "these layers go down" truthfully; a blanket warning on both paths is one the
 * operator learns to click through without reading.
 */
import { useState } from "react";
import { HandCoins, TriangleAlert } from "lucide-react";
import { api } from "../lib/api";
import { useInvalidate } from "../lib/queries";
import { useToast } from "../lib/toast";
import { layerRange } from "../lib/format";
import { Confirm } from "./Confirm";
import { Button, Footnote } from "./ui";

interface Candidate {
  node_id: string;
  hostname: string;
}

interface CandidateProbe {
  serving: boolean;
  slice?: {
    model: string;
    start_layer: number;
    end_layer: number;
    total_layers: number;
  };
  candidate: Candidate | null;
}

interface ReleaseResult {
  released: boolean;
  reassigned_to: Candidate | null;
  uncovered: boolean;
  reason?: string;
}

export function ReleaseSlice({
  startLayer,
  endLayer,
}: {
  startLayer: number;
  endLayer: number;
}) {
  const [probe, setProbe] = useState<CandidateProbe | null>(null);
  const [probing, setProbing] = useState(false);
  const [busy, setBusy] = useState(false);
  const toast = useToast();
  const invalidate = useInvalidate();

  const range = layerRange(startLayer, endLayer);

  async function ask() {
    setProbing(true);
    try {
      setProbe(await api.get<CandidateProbe>("/api/ui/release/candidate"));
    } catch {
      // Losing the probe must not block the release — the machine may need its
      // memory precisely because things are going wrong. Fall back to the
      // pessimistic wording, which is the safe thing to show when unsure.
      setProbe({ serving: true, candidate: null });
    } finally {
      setProbing(false);
    }
  }

  async function release() {
    setBusy(true);
    try {
      const r = await api.post<ReleaseResult>("/api/ui/release", {});
      if (r.reassigned_to) {
        toast.success(
          `${range} agora é de ${r.reassigned_to.hostname}`,
          "A memória deste nó foi liberada e a rede continua atendendo.",
        );
      } else {
        toast.warning(
          `${range} ficou sem ninguém`,
          r.reason ??
            "A memória foi liberada. O modelo para de atender até alguém assumir esta faixa.",
        );
      }
      invalidate();
    } catch (e) {
      toast.fromError("A fatia continua carregada", e);
    } finally {
      setBusy(false);
      setProbe(null);
    }
  }

  const successor = probe?.candidate;

  return (
    <>
      <div className="flex flex-wrap items-center justify-between gap-3 border-t border-line px-5 py-3">
        <Footnote icon={<HandCoins className="h-3.5 w-3.5" />} className="min-w-0 flex-1">
          Precisa da memória desta máquina agora? Devolva{" "}
          <span className="font-mono text-muted">{range}</span> — o nó continua na
          rede e a faixa vai para outro, se houver quem a tenha.
        </Footnote>
        <Button
          size="sm"
          variant="outline"
          loading={probing}
          onClick={() => void ask()}
          className="shrink-0"
        >
          Liberar memória
        </Button>
      </div>

      <Confirm
        open={probe != null}
        busy={busy}
        tone={successor ? "primary" : "danger"}
        title={`Liberar ${range}?`}
        confirmLabel={successor ? "Liberar e passar adiante" : "Liberar mesmo assim"}
        onCancel={() => setProbe(null)}
        onConfirm={() => void release()}
        description={
          successor ? (
            <>
              A memória desta máquina é devolvida agora, e{" "}
              <span className="font-medium text-fg">{successor.hostname}</span> está
              ocioso com esta fatia no disco — ele assume a faixa em seguida. O
              modelo continua atendendo.
            </>
          ) : (
            <span className="flex items-start gap-2">
              <TriangleAlert className="mt-0.5 h-4 w-4 shrink-0 text-amber" />
              <span>
                Nenhum outro nó está ocioso e com esta fatia no disco.{" "}
                <span className="font-medium text-fg">
                  {range} fica descoberta e o modelo para de atender
                </span>{" "}
                até alguém assumir. A memória é liberada de qualquer forma — a
                escolha é desta máquina.
              </span>
            </span>
          )
        }
      />
    </>
  );
}
