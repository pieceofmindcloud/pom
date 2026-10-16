import { TrustAnchor } from "./TrustAnchor";

/** The single screen brings together trust, this machine and the connection key. */
export function NetworkSharing() {
  return <TrustAnchor sharing />;
}
