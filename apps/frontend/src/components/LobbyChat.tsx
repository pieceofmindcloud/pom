import { ArrowLeft, Send } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import type { ReactNode } from "react";
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
export function LobbyChat({ network, endpoint, onClose, layout = "modal", backLabel, headerActions, focusAdmission = false, onUnavailable }: {
  network: { name: string; publisher_key: string; accepted_enrollments?: EnrollmentRole[] } | null;
  endpoint: LobbyEndpoint;
  onClose: () => void;
  /**
   * `page` fills the screen that hosts it, with a back button in place of the
   * window's close, so a screen can keep several lobbies open side by side
   * and switch between them. The wizard keeps the window.
   */
  layout?: "modal" | "page";
  /** The back button's text in `page` layout. */
  backLabel?: string;
  /** Drawn at the right of the `page` header, e.g. the other open lobbies. */
  headerActions?: ReactNode;
  /** Opened to ask for admission: the role picker takes the focus once it shows. */
  focusAdmission?: boolean;
  /**
   * In `page` layout, called when the network answers that its lobby is off:
   * there is nothing to show in a full screen, and the host falls back to a
   * window. A lobby whose Chat is off still fills the screen with the
   * request side.
   */
  onUnavailable?: () => void;
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
  const admissionRef = useRef<HTMLDivElement>(null);
  const page = layout === "page";
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
  // The network Chat is off there: the window is only the request side, with
  // no conversation to show.
  const closed = exchange?.messages_closed === true;
  const latest = messages[messages.length - 1]?.message_id;
  useEffect(() => {
    if (page && !exchange && failure === "lobby.errorDisabled") onUnavailable?.();
  }, [exchange, failure, onUnavailable, page]);
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

  const admissionFormShown = canAsk && Boolean(exchange) && mayAsk && acceptedRoles.length > 0;
  const focused = useRef(false);
  useEffect(() => {
    if (!focusAdmission || focused.current || !admissionFormShown) return;
    focused.current = true;
    const select = admissionRef.current?.querySelector("select");
    select?.focus();
    select?.scrollIntoView?.({ block: "nearest" });
  }, [admissionFormShown, focusAdmission]);

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
  const title = t("lobby.title", { name: network.name });
  const content = (
    <>
      <div className={page
        ? `grid h-full min-h-0 min-w-0 gap-4 ${closed ? "content-start overflow-y-auto py-2" : "md:grid-cols-[minmax(0,1fr)_18rem]"}`
        : closed ? "min-w-0" : "grid min-w-0 gap-4 md:grid-cols-[minmax(0,1fr)_14rem]"}>
        {!closed && <div className={page ? "flex min-h-0 min-w-0 flex-col" : "flex min-w-0 flex-col"}>
          <p className="mb-3 text-xs leading-relaxed text-muted">{t("lobby.detail")}</p>
          <div
            ref={messagesRef}
            data-testid="lobby-messages"
            onScroll={(event) => { autoFollow.current = isNearBottom(event.currentTarget); }}
            className={`${page ? "min-h-0 flex-1" : "h-72"} space-y-2.5 overflow-y-auto overscroll-y-contain rounded-lg border border-line bg-ink p-3`}
          >
            {messages.length === 0 && <p className="py-8 text-center text-sm text-faint">{t(exchange ? "lobby.empty" : failure ? failure : "lobby.connecting")}</p>}
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
              disabled={sending || !exchange}
            />
            <Button variant="primary" className="h-9 w-9 shrink-0 px-0" aria-label={t("networkChat.send")} disabled={!draft.trim() || sending || !exchange} loading={sending} onClick={() => void submit()}>
              <Send className="h-4 w-4" />
            </Button>
          </div>
        </div>}
        <div className={page ? `min-h-0 min-w-0 overflow-y-auto ${closed ? "mx-auto w-full max-w-lg rounded-xl border border-line bg-raised/50 p-4 sm:p-5" : ""}` : "min-w-0"} data-testid="lobby-request">
          <p className="eyebrow mb-2">{t("lobby.yourCard")}</p>
          {exchange ? <LobbyHardwareCard nodeId={exchange.node_id} card={exchange.card} self /> : <p className="text-xs text-faint">{t("lobby.connecting")}</p>}
          <p className="mt-2 text-[0.68rem] leading-relaxed text-faint">{t(closed ? "lobby.cardNoteRequest" : "lobby.cardNote")}</p>
          {closed && failure && <p role="alert" className="mt-2 text-xs text-amber">{t(failure)}</p>}
          {canAsk && exchange && (
            <div className="mt-4 border-t border-line pt-3" data-testid="lobby-admission">
              <p className="eyebrow mb-2">{t("lobby.admission")}</p>
              {admission?.status === "rejected" && waitMinutes === 0 && <p className="mb-2 text-xs leading-relaxed text-muted">{t("lobby.admissionRejectedMayAsk")}</p>}
              {mayAsk && (acceptedRoles.length === 0 ? (
                <p className="text-xs text-faint">{t("publicNetworks.closedDetail")}</p>
              ) : (
                <div ref={admissionRef} className="space-y-2">
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
    </>
  );
  if (page) {
    return (
      <section aria-label={title} className="flex h-full min-h-0 min-w-0 flex-col" data-testid="lobby-page">
        <div className="flex shrink-0 items-center gap-3 border-b border-line bg-raised/60 px-3 py-2 sm:px-4">
          <Button size="sm" variant="ghost" onClick={onClose} icon={<ArrowLeft className="h-3.5 w-3.5" />}>{backLabel ?? t("common.close")}</Button>
          <h2 className="min-w-0 flex-1 truncate font-display text-sm font-semibold text-fg">{title}</h2>
          {headerActions}
        </div>
        <div className="min-h-0 flex-1 overflow-hidden p-3 text-sm leading-relaxed text-muted sm:p-4">{content}</div>
      </section>
    );
  }
  return (
    <Modal open size={closed ? "md" : "lg"} title={title} onClose={onClose} footer={<Button variant="ghost" onClick={onClose}>{t("common.close")}</Button>}>
      {content}
    </Modal>
  );
}
