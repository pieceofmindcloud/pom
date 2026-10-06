import { useQuery } from "@tanstack/react-query";
import { api } from "./api";
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
}

/** Requests waiting for this network's admin. Admin-only on the node. */
export function useAdmissions(enabled: boolean) {
  return useQuery({
    queryKey: ["network-admissions"],
    queryFn: () => api.get<AdmissionRecord[]>("/api/ui/network/admissions"),
    enabled,
    refetchInterval: 5000,
    retry: false,
  });
}

/** What became of the requests this node made elsewhere. */
export function useAdmissionRequests() {
  return useQuery({
    queryKey: ["network-admission-requests"],
    queryFn: () => api.get<AdmissionRequest[]>("/api/ui/network-chat/lobby/requests"),
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
