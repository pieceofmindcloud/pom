/**
 * "Send to…" beside a file on the Models screen: offers one local GGUF (a whole
 * model, a vision projector, an MTP draft or an image model file) to another
 * machine of the network through the existing `/api/transfer/send`.
 *
 * It is an offer, not a remote write: the other machine's operator accepts it
 * (bell or the Models screen's Transfers tab) before any byte leaves. The
 * browser sends only the basename and the backend resolves it again in the
 * local catalog. Machines that already list a file with that name are left
 * out. GGUF and safetensors files are sendable; any other format shows no
 * selector, because the backend refuses to send it.
 */
import { useState } from "react";
import { Select } from "./ui";
import { api } from "../lib/api";
import { uniquePeerName } from "../lib/peerName";
import { useClusterCatalog } from "../lib/queries";
import { useToast } from "../lib/toast";
import { tpPartsOf, type PeerCatalog } from "../lib/types";
import { useI18n } from "../i18n/provider";

const basename = (path: string) => path.split(/[\\/]/).pop() ?? path;

/** The transfer accepts `.gguf`, `.safetensors` or an extensionless name (`has_transfer_name`). */
export function isSendable(path: string): boolean {
  const name = basename(path);
  return /\.(gguf|safetensors)$/i.test(name) || (!name.includes(".") && !name.startsWith("sha256-"));
}

function peerHas(peer: PeerCatalog, filename: string): boolean {
  const paths = [
    ...(peer.models ?? []).map((m) => m.path),
    ...(peer.shard_sets ?? []).flatMap((s) => [...s.shards, ...tpPartsOf(s)]).map((m) => m.path),
    ...(peer.sidecars ?? []).map((s) => s.path),
    ...(peer.image_files ?? []).map((f) => f.path),
  ];
  return paths.some((p) => basename(p) === filename);
}

export function SendToSelect({ path, name }: { path: string; name: string }) {
  const { t } = useI18n();
  const toast = useToast();
  const clusterCatalog = useClusterCatalog();
  const [sending, setSending] = useState(false);

  if (!isSendable(path)) return null;
  const filename = basename(path);
  const targets = (clusterCatalog.data?.peers ?? []).filter(
    (p) => !p.is_self && !peerHas(p, filename),
  );
  if (targets.length === 0) return null;

  async function send(nodeId: string) {
    setSending(true);
    try {
      await api.post("/api/transfer/send", { to_node_id: nodeId, filename });
      toast.success(t("models.sendOfferSent"), t("models.sendOfferSentDetail"));
    } catch (e) {
      toast.fromError(t("models.sendOfferFailed"), e);
    } finally {
      setSending(false);
    }
  }

  return (
    <Select
      compact
      className="w-44"
      value=""
      disabled={sending}
      aria-label={t("models.sendToAria", { name })}
      onChange={(e) => e.target.value && void send(e.target.value)}
    >
      <option value="">{sending ? t("models.sending") : t("models.sendTo")}</option>
      {targets.map((p) => (
        <option key={p.node_id} value={p.node_id}>
          {uniquePeerName(p, targets)}
        </option>
      ))}
    </Select>
  );
}
