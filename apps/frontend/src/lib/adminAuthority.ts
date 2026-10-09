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
   * The trust root, not the authority roster.
   *
   * It is kept out of the admin surface on purpose: before anchoring, a node
   * with `profile: admin` has no effective admin capability, and that is
   * exactly when the operator needs this screen. See `webui/trust.rs`.
   */
  trustAnchor: "/api/ui/network/trust-anchor",
  resign: "/admin/authority/resign",
  demote: (nodeId: string) =>
    `/admin/authority/secondaries/${encodeURIComponent(nodeId)}/demote`,
  promote: (nodeId: string) =>
    `/admin/nodes/${encodeURIComponent(nodeId)}/promote-admin`,
} as const;

export const trustAnchorApi = {
  /**
   * Installs the anchor carried by the operator.
   *
   * `fingerprint` is computed locally from the document, and the screen requires
   * the operator to confirm that value against the admin's separate message.
   * The server still refuses a missing loopback, an already anchored node and a
   * value that does not match the complete chain.
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
};
