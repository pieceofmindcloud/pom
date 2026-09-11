import { TrustAnchor } from "./TrustAnchor";

/** A tela única reúne confiança, esta máquina e a chave de conexão. */
export function NetworkSharing() {
  return <TrustAnchor sharing />;
}
