/**
 * Sending a file this machine's own operator picked.
 *
 * Only files already present in the local catalog can be selected. The browser
 * sends a basename and the backend resolves it again, so a path traversal can
 * never choose an arbitrary file on the machine.
 */
import { FolderInput, Send, ShieldCheck } from "lucide-react";
import { useState } from "react";
import { useSearchParams } from "react-router-dom";
import {
  Button,
  Empty,
  Field,
  PageHeader,
  Panel,
  PanelHeader,
  Select,
  Skeleton,
} from "../components/ui";
import { api } from "../lib/api";
import { useCluster, useLocalCatalog } from "../lib/queries";
import { useToast } from "../lib/toast";
import { useI18n } from "../i18n/provider";

export function SendFile() {
  const { t } = useI18n();
  const [params] = useSearchParams();
  const cluster = useCluster();
  const catalog = useLocalCatalog();
  const toast = useToast();

  const [filename, setFilename] = useState(() => {
    const value = params.get("filename") ?? params.get("path") ?? "";
    return value.split(/[\\/]/).pop() ?? "";
  });
  const [target, setTarget] = useState("");
  const [sending, setSending] = useState(false);

  const peers = (cluster.data?.peers ?? []).filter((p) => !p.is_self);
  const local = [
    ...(catalog.data?.models ?? []),
    ...(catalog.data?.shard_sets ?? []).flatMap((s) => s.shards),
  ].filter((m) => /\.gguf$/i.test(m.path));

  async function send() {
    if (!filename.trim() || !target) return;
    setSending(true);
    try {
      await api.post("/api/transfer/send", {
        to_node_id: target,
        filename: filename.trim(),
      });
      toast.success(
        t("sendFile.offerSent"),
        t("sendFile.offerSentDetail"),
      );
      setFilename("");
    } catch (e) {
      toast.fromError(t("sendFile.offerFailed"), e);
    } finally {
      setSending(false);
    }
  }

  return (
    <>
      <PageHeader
        title={t("sendFile.title")}
        subtitle={t("sendFile.subtitle")}
      />

      {cluster.isLoading && <Skeleton className="h-56 w-full" />}

      {cluster.data && peers.length === 0 ? (
        <Panel>
          <Empty icon={<Send className="h-5 w-5" />} title={t("sendFile.noPeers")}>
            {t("sendFile.noPeersDetail")}
          </Empty>
        </Panel>
      ) : (
        <div className="grid gap-5 lg:grid-cols-[minmax(0,1fr)_minmax(0,20rem)]">
          <Panel>
            <PanelHeader title={t("sendFile.fileAndTarget")} />
            <div className="space-y-4 px-5 py-4">
              <Field
                label={t("sendFile.file")}
                hint={t("sendFile.fileHint")}
              >
                <Select value={filename} onChange={(e) => setFilename(e.target.value)}>
                  <option value="">{t("sendFile.pickFromList")}</option>
                  {local.map((m) => {
                    const name = m.path.split(/[\\/]/).pop() ?? m.name;
                    return (
                      <option key={m.id} value={name}>
                        {m.name} · {m.path}
                      </option>
                    );
                  })}
                </Select>
              </Field>

              <Field label={t("sendFile.destination")}>
                <Select value={target} onChange={(e) => setTarget(e.target.value)}>
                  <option value="">{t("sendFile.choose")}</option>
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
                disabled={!filename.trim() || !target || local.length === 0}
                onClick={() => void send()}
              >
                {t("sendFile.offer")}
              </Button>
            </div>
          </Panel>

          <Panel className="self-start">
            <PanelHeader eyebrow={t("sendFile.howItWorks")} title={t("sendFile.offerNotWrite")} />
            <ul className="space-y-3.5 px-5 py-4 text-[0.83rem] leading-relaxed text-muted">
              <li className="flex gap-2.5">
                <ShieldCheck className="mt-0.5 h-4 w-4 shrink-0 text-aqua" />
                <span className="min-w-0">
                  {t("sendFile.inviteOnly")}
                </span>
              </li>
              <li className="flex gap-2.5">
                <FolderInput className="mt-0.5 h-4 w-4 shrink-0 text-sky" />
                <span className="min-w-0">
                  {t("sendFile.received")}
                </span>
              </li>
              <li className="flex gap-2.5">
                <Send className="mt-0.5 h-4 w-4 shrink-0 text-iris" />
                <span className="min-w-0">
                  {t("sendFile.resume")}
                </span>
              </li>
            </ul>
          </Panel>
        </div>
      )}
    </>
  );
}
