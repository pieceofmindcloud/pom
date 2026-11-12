import { useQuery } from "@tanstack/react-query";
import { api } from "./api";
import { isUserSession } from "./session";
import type { EnrollmentRole, LobbyCard } from "./types";

export type AdmissionStatus = "pending" | "approved" | "rejected";

/** An admission request this network received, as its admin reads it. */
export interface AdmissionRecord {
  node_id: string;
  /** Declared hardware, the latency the candidate measured and how it connects. */
  card: LobbyCard;
  role: EnrollmentRole;
  status: AdmissionStatus;
  requested_at: string;
  decided_at?: string | null;
}

/** An admission request this node made to another network. Never the grant. */
export interface AdmissionRequest {
  /** `pk:` key of the network's principal. */
  key: string;
  name: string;
  role: EnrollmentRole;
  status: AdmissionStatus;
  /**
   * An approval this node did not just ask for: it was admitted to that
   * network before and the approval still stands. Not an answer to anything,
   * so it is not announced.
   */
  standing?: boolean;
  /** When declined: the Unix second from which this node may ask again. */
  retry_at?: number | null;
}

/**
 * Whole minutes a declined machine still has to wait, rounded up so that
 * "1 min" never means "now"; 0 when it may ask again.
 */
export function retryWaitMinutes(retryAt: number | null | undefined, nowMs: number = Date.now()): number {
  if (!retryAt) return 0;
  const seconds = retryAt - nowMs / 1000;
  return seconds > 0 ? Math.ceil(seconds / 60) : 0;
}

/** Requests waiting for this network's admin. Admin-only on the node. */
export function useAdmissions(enabled: boolean) {
  return useQuery({
    queryKey: ["network-admissions"],
    queryFn: () => api.get<AdmissionRecord[]>("/api/ui/network/admissions"),
    // A signed-in person never decides admissions: the node refuses the route.
    enabled: enabled && !isUserSession(),
    refetchInterval: 5000,
    retry: false,
  });
}

/** What became of the requests this node made elsewhere. */
export function useAdmissionRequests() {
  return useQuery({
    queryKey: ["network-admission-requests"],
    queryFn: () => api.get<AdmissionRequest[]>("/api/ui/network-chat/lobby/requests"),
    enabled: !isUserSession(),
    refetchInterval: 10000,
    retry: false,
  });
}

export const admissionsApi = {
  decide: (nodeId: string, decision: "approve" | "reject" | "forget") =>
    api.post<AdmissionRecord[]>("/api/ui/network/admissions", { node_id: nodeId, decision }),
  /** Leaves the current network for the one that approved this machine. */
  join: (key: string) => api.post<{ joined: boolean }>("/api/ui/network/join", { key }),
  forget: (key: string) => api.post("/api/ui/network-chat/lobby/requests/forget", { key }),
};
