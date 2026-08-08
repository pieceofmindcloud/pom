/**
 * Sending a file this machine's own operator picked.
 *
 * Any readable path is allowed here, deliberately: the models live under LM
 * Studio, under Ollama and in loose folders, and "it is on this machine so I
 * must be able to send it" is the requirement. What keeps that from becoming
 * "any peer can read any file" is that the path came from *this* screen — the
 * receiving peer only ever gets an opaque grant id back (see `src/transfer.rs`).
 */
import { FolderInput, Send, ShieldCheck } from "lucide-react";
import { useState } from "react";
import { useSearchParams } from "react-router-dom";
import {
  Button,
  Empty,
  Field,
  Input,
  PageHeader,
  Panel,
  PanelHeader,
  Select,
  Skeleton,
} from "../components/ui";
import { api } from "../lib/api";
import { useCluster, useLocalCatalog } from "../lib/queries";
import { useToast } from "../lib/toast";

export function SendFile() {
  const [params] = useSearchParams();
  const cluster = useCluster();
  const catalog = useLocalCatalog();
  const toast = useToast();

  const [path, setPath] = useState(params.get("path") ?? "");
  const [target, setTarget] = useState("");
  const [sending, setSending] = useState(false);

  const peers = (cluster.data?.peers ?? []).filter((p) => !p.is_self);
  const local = [
    ...(catalog.data?.models ?? []),
    ...(catalog.data?.shard_sets ?? []).flatMap((s) => s.shards),
  ];

  async function send() {
    if (!path.trim() || !target) return;
    setSending(true);
    try {
      await api.post("/api/transfer/send", {
        to_node_id: target,
        path: path.trim(),
      });
      toast.success(
        "Oferta enviada",
        "O dono da outra máquina precisa aceitar; só então os bytes saem daqui.",
      );
      setPath("");
    } catch (e) {
      toast.fromError("A oferta não saiu", e);
    } finally {
      setSending(false);
    }
  }

  return (
    <>
      <PageHeader
        title="Enviar um arquivo desta máquina"
        subtitle="Para o que a varredura não acha: digite o caminho, escolha o destino. Vale qualquer arquivo legível daqui."
      />

      {cluster.isLoading && <Skeleton className="h-56 w-full" />}

      {cluster.data && peers.length === 0 ? (
        <Panel>
          <Empty icon={<Send className="h-5 w-5" />} title="Nenhuma outra máquina na rede">
            Não há para quem enviar. Confira a descoberta em Configurações › Rede.
          </Empty>
        </Panel>
      ) : (
        <div className="grid gap-5 lg:grid-cols-[minmax(0,1fr)_minmax(0,20rem)]">
          <Panel>
            <PanelHeader title="Arquivo e destino" />
            <div className="space-y-4 px-5 py-4">
              <Field
                label="Caminho nesta máquina"
                hint="Caminho absoluto. Um GGUF do LM Studio, um blob do Ollama ou qualquer arquivo solto serve."
              >
                <Input
                  value={path}
                  onChange={(e) => setPath(e.target.value)}
                  placeholder="/Users/você/models/qwen/qwen_0_15.gguf"
                  spellCheck={false}
                />
              </Field>

              {local.length > 0 && (
                <Field
                  label="Ou escolha um dos arquivos que a varredura achou"
                  hint="Preenche o campo acima; ainda dá para editar."
                >
                  <Select value="" onChange={(e) => e.target.value && setPath(e.target.value)}>
                    <option value="">Escolher da lista…</option>
                    {local.map((m) => (
                      <option key={m.id} value={m.path}>
                        {m.name}
                      </option>
                    ))}
                  </Select>
                </Field>
              )}

              <Field label="Máquina de destino">
                <Select value={target} onChange={(e) => setTarget(e.target.value)}>
                  <option value="">Escolher…</option>
                  {peers.map((p) => (
                    <option key={p.node_id} value={p.node_id}>
                      {p.node?.hostname || p.short_id} · {p.addr}
                    </option>
                  ))}
                </Select>
              </Field>

              <Button
                variant="primary"
                icon={<Send className="h-3.5 w-3.5" />}
                loading={sending}
                disabled={!path.trim() || !target}
                onClick={() => void send()}
              >
                Oferecer o arquivo
              </Button>
            </div>
          </Panel>

          <Panel className="self-start">
            <PanelHeader eyebrow="Como funciona" title="Oferta, não escrita" />
            <ul className="space-y-3.5 px-5 py-4 text-[0.83rem] leading-relaxed text-muted">
              <li className="flex gap-2.5">
                <ShieldCheck className="mt-0.5 h-4 w-4 shrink-0 text-aqua" />
                Nada é gravado no disco alheio sem convite: o outro lado vê a oferta no
                sino e decide.
              </li>
              <li className="flex gap-2.5">
                <FolderInput className="mt-0.5 h-4 w-4 shrink-0 text-sky" />
                Aceita, a máquina de destino busca o arquivo e o grava em{" "}
                <span className="font-mono text-fg">recebidos/</span>, dentro da pasta de
                fatias dela.
              </li>
              <li className="flex gap-2.5">
                <Send className="mt-0.5 h-4 w-4 shrink-0 text-iris" />
                A transferência retoma por <span className="font-mono text-fg">Range</span>{" "}
                e o MD5 é conferido antes de o arquivo tomar o nome final. Se não bater, o
                parcial é apagado.
              </li>
            </ul>
          </Panel>
        </div>
      )}
    </>
  );
}
