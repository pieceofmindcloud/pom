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
  requestNode: "/api/ui/network/request-node",
  relinquishNode: "/api/ui/network/relinquish-node",
  resign: "/admin/authority/resign",
  demote: (nodeId: string) =>
    `/admin/authority/secondaries/${encodeURIComponent(nodeId)}/demote`,
  promote: (nodeId: string) =>
    `/admin/nodes/${encodeURIComponent(nodeId)}/promote-admin`,
} as const;

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
