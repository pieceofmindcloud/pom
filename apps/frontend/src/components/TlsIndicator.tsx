import { ShieldCheck, ShieldOff } from "lucide-react";
import { Link } from "react-router-dom";
import { useI18n } from "../i18n/provider";
import { useNode } from "../lib/queries";
import { Badge } from "./ui";

/**
 * TLS policy indicator shown beside the language flags.
 *
 * The icon and tone change with the policy. The visible label is intentionally
 * omitted, but the link keeps the meaning in its accessible name and tooltip.
 */
export function TlsChip() {
  const { t } = useI18n();
  const node = useNode();
  if (!node.data) return null;

  const on = node.data.tls_required;
  const label = t(on ? "topbar.tlsOnHint" : "topbar.tlsOffHint");
  return (
    <Link
      to="/config/rede"
      data-testid="tls-indicator"
      data-tls-state={on ? "on" : "off"}
      aria-label={label}
      title={label}
      className="shrink-0"
    >
      <Badge tone={on ? "aqua" : "amber"} className="h-8 w-8 justify-center px-0">
        {on ? (
          <ShieldCheck aria-hidden="true" className="h-3.5 w-3.5" />
        ) : (
          <ShieldOff aria-hidden="true" className="h-3.5 w-3.5" />
        )}
      </Badge>
    </Link>
  );
}
