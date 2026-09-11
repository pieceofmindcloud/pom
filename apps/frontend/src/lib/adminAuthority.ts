/**
 * UI boundary for authority and profile transitions.
 *
 * The browser knows the operation names and response contracts, but not how
 * the node stores grants or builds signatures. Keeping those choices behind
 * this small adapter lets the backend mechanism evolve without changing the
 * screens.
 */
import { api } from "./api";

export const adminAuthorityPaths = {
  view: "/api/ui/network/admin-authority",
  /**
   * A raiz de confiança, e não o roster de autoridade.
   *
   * Fica fora da superfície de admin de propósito: antes de ancorar, um nó
   * com `profile: admin` não tem capacidade efetiva de admin, e é justamente
   * aí que o operador precisa desta tela. Ver `webui/trust.rs`.
   */
  trustAnchor: "/api/ui/network/trust-anchor",
  requestNode: "/api/ui/network/request-node",
  relinquishNode: "/api/ui/network/relinquish-node",
  resign: "/admin/authority/resign",
  demote: (nodeId: string) =>
    `/admin/authority/secondaries/${encodeURIComponent(nodeId)}/demote`,
  promote: (nodeId: string) =>
    `/admin/nodes/${encodeURIComponent(nodeId)}/promote-admin`,
} as const;

export const trustAnchorApi = {
  /**
   * Instala a âncora transportada pelo operador.
   *
   * `fingerprint` é calculado localmente a partir do documento, e a tela exige
   * que o operador confirme esse valor contra a mensagem separada do admin.
   * O servidor continua recusando loopback ausente, nó já ancorado e valor que
   * não corresponde à cadeia completa.
   */
  install(document: string, fingerprint: string) {
    return api.post<{ anchored: boolean; fingerprint: string }>(
      adminAuthorityPaths.trustAnchor,
      { document, fingerprint },
    );
  },
};

export const adminAuthorityApi = {
  promoteNode(nodeId: string) {
    return api.post(adminAuthorityPaths.promote(nodeId));
  },
  resign() {
    return api.post(adminAuthorityPaths.resign);
  },
  demoteSecondary(nodeId: string) {
    return api.post(adminAuthorityPaths.demote(nodeId));
  },
  requestNodePromotion() {
    return api.post(adminAuthorityPaths.requestNode);
  },
  relinquishNode() {
    return api.post(adminAuthorityPaths.relinquishNode);
  },
};
