import { Send } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { Confirm, Modal } from "./Confirm";
import { LobbyHardwareCard } from "./LobbyHardwareCard";
import { Button, Input, Select } from "./ui";
import { api } from "../lib/api";
import { isNearBottom } from "../lib/chatScroll";
import { randomId } from "../lib/id";
import { useI18n } from "../i18n/provider";
import { admissionsApi, retryWaitMinutes } from "../lib/admissions";
import { ENROLLMENT_ROLES } from "../lib/types";
import type { EnrollmentRole, LobbyExchange } from "../lib/types";

const POLL_MS = 3000;
const MAX_TEXT_BYTES = 2 * 1024;

/**
 * Where the node speaks to another network's lobby from. A configured node
 * uses its operator session; the setup wizard has no session yet and uses the
 * loopback-only setup router.
 */
export type LobbyEndpoint = "/api/ui/network-chat/lobby" | "/api/setup/lobby";

/** The refusal codes of the principal that deserve their own sentence. */
const ERROR_KEYS: Record<string, string> = {
  lobby_disabled: "lobby.errorDisabled",
  lobby_messages_closed: "lobby.messagesClosed",
  node_blocked: "lobby.errorBlocked",
  already_member: "lobby.errorMember",
  rate_limited: "lobby.errorRate",
  role_not_accepted: "lobby.errorRole",
  admission_rejected: "lobby.errorRejected",
  admissions_full: "lobby.errorFull",
  lobby_full: "lobby.errorFull",
  text_invalid: "lobby.errorText",
};

function errorKey(code: string | null | undefined): string {
  if (!code) return "lobby.errorUnreachable";
  return ERROR_KEYS[code] ?? "lobby.errorUnreachable";
}

/**
 * Chat with a network this machine has **not** joined. Everything goes
 * through this node, which reaches the network's principal by its key; the
 * browser never talks to the other network. Pull only: the history is asked
 * again every few seconds while the window is open, and that same request is
 * what keeps this machine listed in the lobby.
 */
export function LobbyChat({ network, endpoint, onClose }: {
  network: { name: string; publisher_key: string; accepted_enrollments?: EnrollmentRole[] } | null;
  endpoint: LobbyEndpoint;
  onClose: () => void;
}) {
  const { t } = useI18n();
  const [exchange, setExchange] = useState<LobbyExchange | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const [draft, setDraft] = useState("");
  const [sending, setSending] = useState(false);
  const [role, setRole] = useState<EnrollmentRole>("node");
  const [asking, setAsking] = useState(false);
  const [joinConfirm, setJoinConfirm] = useState(false);
  const [joining, setJoining] = useState(false);
  const messagesRef = useRef<HTMLDivElement>(null);
  const autoFollow = useRef(true);
  const key = network?.publisher_key ?? "";
  // The setup router has no session to present, and asking for one would
  // bounce the wizard to a login that does not exist yet.
  const auth = endpoint !== "/api/setup/lobby";

  const adopt = useCallback((next: LobbyExchange) => {
    if (next.accepted) {
      setExchange(next);
      setFailure(null);
    } else {
      setFailure(errorKey(next.error));
    }
  }, []);

  useEffect(() => {
    setExchange(null);
    setFailure(null);
    setDraft("");
    if (!key) return;
    let cancelled = false;
    const poll = () => {
      void api.get<LobbyExchange>(`${endpoint}?key=${encodeURIComponent(key)}`, { auth })
        .then((next) => { if (!cancelled) adopt(next); })
        .catch(() => { if (!cancelled) setFailure("lobby.errorUnreachable"); });
    };
    poll();
    const timer = window.setInterval(poll, POLL_MS);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [adopt, auth, endpoint, key]);

  const messages = exchange?.messages ?? [];
  // The network Chat is off there: asking to join still works, talking does not.
  const closed = exchange?.messages_closed === true;
  const latest = messages[messages.length - 1]?.message_id;
  useEffect(() => {
    const element = messagesRef.current;
    if (element && autoFollow.current) element.scrollTop = element.scrollHeight;
  }, [latest, messages.length]);

  async function submit() {
    const text = draft.trim();
    if (!text || !key || sending) return;
    if (new TextEncoder().encode(text).length > MAX_TEXT_BYTES) {
      setFailure("lobby.errorText");
      return;
    }
    setSending(true);
    try {
      const next = await api.post<LobbyExchange>(endpoint, { key, message_id: randomId(), text }, { auth });
      adopt(next);
      if (next.accepted) setDraft("");
    } catch {
      setFailure("lobby.errorUnreachable");
    } finally {
      setSending(false);
    }
  }

  // Asking to be admitted is for a configured node: the wizard has no
  // network to leave and nothing to keep a request in.
  const canAsk = endpoint === "/api/ui/network-chat/lobby";
  const acceptedRoles = ENROLLMENT_ROLES.filter((option) => !network?.accepted_enrollments || network.accepted_enrollments.includes(option));
  const askRole = acceptedRoles.includes(role) ? role : acceptedRoles[0];
  const admission = exchange?.admission ?? null;
  // A decline holds for a while. Once it is over the machine may ask again,
  // so the form comes back; the lobby poll re-renders this every few seconds.
  const waitMinutes = admission?.status === "rejected" ? retryWaitMinutes(admission.retry_at) : 0;
  const mayAsk = !admission || (admission.status === "rejected" && waitMinutes === 0);

  async function requestAdmission() {
    if (!key || !askRole || asking) return;
    setAsking(true);
    try {
      adopt(await api.post<LobbyExchange>(`${endpoint}/admission`, { key, role: askRole, name: network?.name ?? "" }));
    } catch {
      setFailure("lobby.errorUnreachable");
    } finally {
      setAsking(false);
    }
  }

  async function join() {
    setJoining(true);
    try {
      await admissionsApi.join(key);
      window.setTimeout(() => window.location.assign("/admin-ui/"), 1500);
    } catch {
      setFailure("lobby.joinFailed");
      setJoining(false);
      setJoinConfirm(false);
    }
  }

  if (!network) return null;
  return (
    <Modal open size="lg" title={t("lobby.title", { name: network.name })} onClose={onClose} footer={<Button variant="ghost" onClick={onClose}>{t("common.close")}</Button>}>
      <div className="grid min-w-0 gap-4 md:grid-cols-[minmax(0,1fr)_14rem]">
        <div className="flex min-w-0 flex-col">
          <p className="mb-3 text-xs leading-relaxed text-muted">{t("lobby.detail")}</p>
          <div
            ref={messagesRef}
            data-testid="lobby-messages"
            onScroll={(event) => { autoFollow.current = isNearBottom(event.currentTarget); }}
            className="h-72 space-y-2.5 overflow-y-auto rounded-lg border border-line bg-ink p-3"
          >
            {messages.length === 0 && <p className="py-8 text-center text-sm text-faint">{t(closed ? "lobby.messagesClosed" : exchange ? "lobby.empty" : failure ? failure : "lobby.connecting")}</p>}
            {messages.map((message) => {
              const own = message.sender_node_id === exchange?.node_id;
              return (
                <div key={message.message_id} className={`flex ${own ? "justify-end" : "justify-start"}`}>
                  <div className={`max-w-[88%] rounded-2xl border px-3 py-2 ${own ? "border-iris/35 bg-iris/12" : "border-line bg-raised"}`}>
                    <div className="mb-0.5 flex items-center gap-2 text-[0.66rem] text-faint">
                      <span className="font-medium text-fg">{message.sender_name || message.sender_node_id.slice(0, 12)}</span>
                      <span className={message.member ? "text-aqua" : undefined}>{t(message.member ? "lobby.member" : "lobby.candidate")}</span>
                    </div>
                    <p className="whitespace-pre-wrap break-words text-sm text-fg">{message.text}</p>
                  </div>
                </div>
              );
            })}
          </div>
          {failure && messages.length > 0 && <p role="alert" className="mt-2 text-xs text-amber">{t(failure)}</p>}
          <div className="mt-3 flex items-end gap-2">
            <Input
              value={draft}
              aria-label={t("lobby.placeholder")}
              placeholder={t("lobby.placeholder")}
              onChange={(event) => setDraft(event.target.value)}
              onKeyDown={(event) => { if (event.key === "Enter" && !event.shiftKey) { event.preventDefault(); void submit(); } }}
              disabled={sending || !exchange || closed}
            />
            <Button variant="primary" className="h-9 w-9 shrink-0 px-0" aria-label={t("networkChat.send")} disabled={!draft.trim() || sending || !exchange || closed} loading={sending} onClick={() => void submit()}>
              <Send className="h-4 w-4" />
            </Button>
          </div>
        </div>
        <div className="min-w-0">
          <p className="eyebrow mb-2">{t("lobby.yourCard")}</p>
          {exchange ? <LobbyHardwareCard nodeId={exchange.node_id} card={exchange.card} self /> : <p className="text-xs text-faint">{t("lobby.connecting")}</p>}
          <p className="mt-2 text-[0.68rem] leading-relaxed text-faint">{t("lobby.cardNote")}</p>
          {canAsk && exchange && (
            <div className="mt-4 border-t border-line pt-3" data-testid="lobby-admission">
              <p className="eyebrow mb-2">{t("lobby.admission")}</p>
              {admission?.status === "rejected" && waitMinutes === 0 && <p className="mb-2 text-xs leading-relaxed text-muted">{t("lobby.admissionRejectedMayAsk")}</p>}
              {mayAsk && (acceptedRoles.length === 0 ? (
                <p className="text-xs text-faint">{t("publicNetworks.closedDetail")}</p>
              ) : (
                <div className="space-y-2">
                  <Select compact aria-label={t("lobby.admissionRole")} value={askRole} onChange={(event) => setRole(event.target.value as EnrollmentRole)}>
                    {acceptedRoles.map((option) => <option key={option} value={option}>{t(`network.enrollment.${option}`)}</option>)}
                  </Select>
                  <Button size="sm" variant="primary" loading={asking} onClick={() => void requestAdmission()}>{t("lobby.admissionAsk")}</Button>
                  <p className="text-[0.68rem] leading-relaxed text-faint">{t("lobby.admissionAskDetail")}</p>
                </div>
              ))}
              {admission?.status === "pending" && <p className="text-xs leading-relaxed text-amber">{t("lobby.admissionPending", { role: t(`network.enrollment.${admission.role}`) })}</p>}
              {admission?.status === "rejected" && waitMinutes > 0 && (
                <p className="text-xs leading-relaxed text-rose" data-testid="lobby-admission-wait">
                  {t("lobby.admissionRejectedWait", { minutes: waitMinutes })}
                </p>
              )}
              {admission?.status === "approved" && !admission.standing && (
                <div className="space-y-2">
                  <p className="text-xs leading-relaxed text-lime">{t("lobby.admissionApproved", { role: t(`network.enrollment.${admission.role}`) })}</p>
                  <Button size="sm" variant="primary" onClick={() => setJoinConfirm(true)}>{t("lobby.joinNow")}</Button>
                </div>
              )}
              {/* Admitted before: there is nothing to wait for or to be told
                  later, and the button switches at once. */}
              {admission?.status === "approved" && admission.standing && (
                <div className="space-y-2">
                  <p className="text-xs leading-relaxed text-lime">{t("lobby.admissionStanding", { role: t(`network.enrollment.${admission.role}`) })}</p>
                  <Button size="sm" variant="primary" loading={joining} onClick={() => void join()}>{t("lobby.switchNow")}</Button>
                  <p className="text-[0.68rem] leading-relaxed text-faint">{t("lobby.joinDetail")}</p>
                </div>
              )}
            </div>
          )}
        </div>
      </div>
      <Confirm open={joinConfirm} title={t("lobby.joinTitle", { name: network.name })} description={t("lobby.joinDetail")} confirmLabel={t("lobby.joinNow")} tone="danger" busy={joining} onConfirm={() => void join()} onCancel={() => !joining && setJoinConfirm(false)} />
    </Modal>
  );
}
