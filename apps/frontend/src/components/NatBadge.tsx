/**
 * The top bar's "Behind NAT" badge and the dialog it opens.
 *
 * Being behind a NAT costs nothing while the hole punch works: the relay only
 * coordinates it and the machines then talk directly. The badge shows only
 * when that failed for some peer and the relay fell back to carrying the
 * traffic itself (`ClusterView.relayed_peers`, from `Network::peer_route`
 * kind `relayed`), which is what adds latency and, on a public relay, caps the
 * bandwidth. The dialog names those machines, explains why it is slow and how
 * to give this machine a direct path.
 */
import { useState } from "react";
import { Link } from "react-router-dom";
import { ShieldAlert } from "lucide-react";
import { useCluster, useNode } from "../lib/queries";
import { useI18n } from "../i18n/provider";
import { Modal } from "./Confirm";
import { Badge, Button } from "./ui";

/** Offset of the endpoint's UDP port from the P2P port (`p2p_port + 10000`). */
const ENDPOINT_PORT_OFFSET = 10000;

export function NatBadge() {
  const { t } = useI18n();
  const cluster = useCluster();
  const node = useNode();
  const [open, setOpen] = useState(false);

  const relayed = cluster.data?.relayed_peers ?? [];
  if (relayed.length === 0) return null;

  const p2p = node.data?.p2p_port;
  const ports = p2p ? { p2p, endpoint: p2p + ENDPOINT_PORT_OFFSET } : null;

  return (
    <>
      <button
        type="button"
        data-testid="topbar-nat"
        onClick={() => setOpen(true)}
        title={t("topbar.behindNatHint")}
        aria-haspopup="dialog"
        className="shrink-0 rounded-md focus-visible:outline-offset-2"
      >
        <Badge tone="amber">
          <ShieldAlert className="h-3 w-3 shrink-0" aria-hidden="true" />
          {t("topbar.behindNat")}
        </Badge>
      </button>
      <Modal
        open={open}
        size="lg"
        title={t("nat.title")}
        onClose={() => setOpen(false)}
        footer={
          <>
            <Link to="/config/network" onClick={() => setOpen(false)}>
              <Button variant="ghost">{t("nat.openNetworkSettings")}</Button>
            </Link>
            <Button variant="primary" onClick={() => setOpen(false)}>
              {t("common.close")}
            </Button>
          </>
        }
      >
        <div data-testid="nat-dialog" className="space-y-4 text-sm leading-relaxed text-muted">
          <p className="text-fg">{t("nat.intro")}</p>
          <p>{t("nat.relay")}</p>
          <div data-testid="nat-relayed-peers">
            <h3 className="mb-2 font-display text-sm font-semibold text-fg">{t("nat.relayedTitle")}</h3>
            <ul className="space-y-1">
              {relayed.map((peer) => (
                <li key={peer.node_id} className="flex flex-wrap items-center gap-2">
                  <span className="text-fg">{peer.hostname || peer.node_id.slice(0, 12)}</span>
                  <Badge tone={peer.relay_kind === "public" ? "amber" : "neutral"}>
                    {t(`nat.relayKind.${peer.relay_kind ?? "member"}`)}
                  </Badge>
                </li>
              ))}
            </ul>
          </div>
          <p>{t("nat.slowness")}</p>
          <div>
            <h3 className="mb-2 font-display text-sm font-semibold text-fg">{t("nat.fixTitle")}</h3>
            <ul className="list-disc space-y-1.5 pl-5">
              <li>{t("nat.fixIpv6")}</li>
              <li>{ports ? t("nat.fixRouterPorts", ports) : t("nat.fixRouter")}</li>
              <li>{t("nat.fixFirewall")}</li>
            </ul>
          </div>
          <p>{t("nat.ideal")}</p>
          <p className="rounded-lg border border-amber/40 bg-amber/10 px-3 py-2 text-fg">{t("nat.check")}</p>
        </div>
      </Modal>
    </>
  );
}
