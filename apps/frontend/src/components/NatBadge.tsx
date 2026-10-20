/**
 * The top bar's "Behind NAT" badge and the dialog it opens.
 *
 * The node learns from the NAT report whether the public address the relays
 * saw is one of its own (`ClusterView.traversal.nat`). When it is not, other
 * networks can only reach this machine through a hole punch or a relay, and a
 * slow pipeline usually starts there. The badge says so on every screen, and
 * the dialog explains what it means and how to give the machine a direct path.
 * Nothing is drawn while the report is `public` or not in yet (`unknown`).
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

  if (cluster.data?.traversal?.nat !== "private") return null;

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
