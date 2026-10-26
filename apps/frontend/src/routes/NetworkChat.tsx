import { Check, Download, FileUp, Send, UserPlus, Users, Wifi, WifiOff, X } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useSearchParams } from "react-router-dom";
import { LobbyHardwareCard } from "../components/LobbyHardwareCard";
import { Button, Empty, Input, PageHeader, Tabs } from "../components/ui";
import { admissionsApi, useAdmissions, type AdmissionRecord } from "../lib/admissions";
import { downloadNetworkChatFile, postNetworkChat } from "../lib/api";
import { isNearBottom } from "../lib/chatScroll";
import { randomId } from "../lib/id";
import { useCluster, useNetworkChat, useNode } from "../lib/queries";
import { markLobbyRead, markNetworkChatRead } from "../lib/networkChatUnread";
import { useToast } from "../lib/toast";
import { useI18n } from "../i18n/provider";
import type {
  LobbyGuest,
  NetworkChatEvent,
  NetworkChatParticipant,
  NetworkChatRequest,
  NetworkChatRoom,
} from "../lib/types";

const MAX_FILE_BYTES = 8 * 1024 * 1024;

/** Each candidate talks to the admins in a thread of its own (`network_lobby.rs`). */
const THREAD_PREFIX = "lobby/";

function threadCandidate(roomId: string): string | null {
  return roomId.startsWith(THREAD_PREFIX) ? roomId.slice(THREAD_PREFIX.length) || null : null;
}

function id(): string {
  return randomId();
}

function shortNodeId(nodeId: string): string {
  return nodeId.length > 12 ? nodeId.slice(0, 12) : nodeId;
}

function now(): string {
  return new Date().toISOString();
}

function textAction(room: string, senderNodeId: string, text: string): NetworkChatRequest {
  return {
    action: {
      kind: "text",
      message_id: id(),
      room_id: room,
      sender_node_id: senderNodeId,
      recipient_node_id: null,
      text,
      sent_at: now(),
    },
  };
}

function typingAction(senderNodeId: string, isTyping: boolean): NetworkChatRequest {
  return {
    action: {
      kind: "typing",
      message_id: id(),
      room_id: "network",
      sender_node_id: senderNodeId,
      recipient_node_id: null,
      is_typing: isTyping,
      sent_at: now(),
    },
  };
}

function fileAction(senderNodeId: string, file: File, data: number[]): NetworkChatRequest {
  return {
    action: {
      kind: "file",
      message_id: id(),
      room_id: "network",
      sender_node_id: senderNodeId,
      recipient_node_id: null,
      file_name: file.name,
      mime_type: file.type || "application/octet-stream",
      data,
      sent_at: now(),
    },
  };
}

function MessageRow({ message, localNodeId, nodeName, onDownload }: { message: NetworkChatEvent; localNodeId: string; nodeName: (id: string) => string; onDownload: (message: NetworkChatEvent) => void }) {
  const { t } = useI18n();
  const local = message.senderNodeId === localNodeId;
  const when = new Date(message.sentAt).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  return (
    <div className={`flex ${local ? "justify-end" : "justify-start"}`}>
      <div className={`max-w-[min(38rem,88%)] rounded-2xl border px-3.5 py-2.5 ${local ? "border-iris/35 bg-iris/12" : "border-line bg-raised"}`}>
        <div className="mb-1 flex items-center gap-2 text-[0.68rem] text-faint">
          <span className="font-medium text-fg">{nodeName(message.senderNodeId)}</span>
          <span>{when}</span>
        </div>
        {message.kind === "text" && <p className="whitespace-pre-wrap break-words text-sm text-fg">{message.text}</p>}
        {message.kind === "file" && (
          <div className="flex items-center gap-2 text-sm text-fg">
            <FileUp className="h-4 w-4 text-aqua" />
            <button type="button" className="min-w-0 break-all text-left underline decoration-line/70 underline-offset-2 hover:text-aqua" onClick={() => onDownload(message)} title={t("networkChat.download")}>
              {message.fileName}
            </button>
            <span className="text-xs text-faint">{message.fileData.length.toLocaleString()} B</span>
            <button type="button" className="grid h-7 w-7 shrink-0 place-items-center rounded-md text-muted hover:bg-line/50 hover:text-fg" onClick={() => onDownload(message)} aria-label={t("networkChat.download")}>
              <Download className="h-3.5 w-3.5" />
            </button>
          </div>
        )}
        <p className="mt-1 text-[0.65rem] text-faint">{t("networkChat.nodeLabel")}: <span className="font-mono">{shortNodeId(message.senderNodeId)}</span></p>
      </div>
    </div>
  );
}

function ParticipantList({ participants, nodeName }: { participants: NetworkChatParticipant[]; nodeName: (id: string) => string }) {
  const { t } = useI18n();
  return (
    <aside className="border-b border-line px-4 py-4 lg:border-b-0 lg:border-l lg:py-5">
      <div className="mb-3 flex items-center gap-2 text-xs font-semibold uppercase tracking-[0.12em] text-muted">
        <Users className="h-3.5 w-3.5 text-aqua" />
        {t("networkChat.participants", { count: participants.length })}
      </div>
      <div className="space-y-2">
        {participants.map((participant) => (
          <div key={participant.nodeId} className="flex min-w-0 items-center gap-2 rounded-lg px-2 py-1.5 hover:bg-line/30">
            <span className="h-2 w-2 shrink-0 rounded-full bg-aqua shadow-[0_0_0_3px_rgb(45_212_191/0.12)]" />
            <div className="min-w-0">
              <p className="truncate text-sm text-fg">{nodeName(participant.nodeId)}</p>
              <p className="truncate font-mono text-[0.65rem] text-faint">{shortNodeId(participant.nodeId)}</p>
            </div>
          </div>
        ))}
        {!participants.length && <p className="text-sm text-faint">{t("networkChat.noParticipants")}</p>}
      </div>
    </aside>
  );
}

/**
 * The candidates with a thread or present in the lobby; picking one opens
 * its thread. A present candidate shows the hardware it declared.
 */
function LobbyThreadList({ candidates, guests, pending, selected, onSelect, nodeName }: { candidates: string[]; guests: LobbyGuest[]; pending: AdmissionRecord[]; selected: string | null; onSelect: (candidate: string) => void; nodeName: (id: string) => string }) {
  const { t } = useI18n();
  return (
    <aside className="border-b border-line px-4 py-4 lg:border-b-0 lg:border-l lg:py-5">
      <div className="mb-3 flex items-center gap-2 text-xs font-semibold uppercase tracking-[0.12em] text-muted">
        <Users className="h-3.5 w-3.5 text-aqua" />
        {t("lobby.candidates", { count: guests.length })}
      </div>
      <div className="space-y-3">
        {candidates.map((candidate) => {
          const guest = guests.find((item) => item.nodeId === candidate);
          const active = candidate === selected;
          const request = pending.find((record) => record.node_id === candidate);
          return (
            <button
              key={candidate}
              type="button"
              data-testid={`lobby-thread-${candidate}`}
              aria-pressed={active}
              aria-current={active ? "true" : undefined}
              onClick={() => onSelect(candidate)}
              className={`relative block w-full rounded-lg border-2 text-left transition-colors ${active ? "border-iris bg-iris/10 shadow-[0_0_0_3px_rgb(var(--iris)/0.15)]" : "border-transparent opacity-75 hover:border-line-strong hover:opacity-100"}`}
            >
              {guest ? <LobbyHardwareCard nodeId={candidate} card={guest.card} /> : <span className="block px-3 py-2 text-sm text-muted">{nodeName(candidate)}</span>}
              {request && <span className="mx-3 mb-2 inline-flex items-center gap-1 rounded-full bg-amber/15 px-2 py-0.5 text-[0.62rem] text-amber"><UserPlus className="h-3 w-3" />{t("lobby.threadAsks")}</span>}
            </button>
          );
        })}
        {!candidates.length && <p className="text-sm text-faint">{t("lobby.noCandidates")}</p>}
      </div>
      <p className="mt-3 text-[0.68rem] leading-relaxed text-faint">{t("lobby.declaredNote")}</p>
    </aside>
  );
}

/**
 * What the admin is looking at in the lobby: whose thread, and the decision on
 * its admission request, taken here after talking. Either decision erases the
 * thread for every admin.
 */
function LobbyThreadHeader({ candidate, request, nodeName, deciding, onDecide }: { candidate: string; request: AdmissionRecord | undefined; nodeName: (id: string) => string; deciding: boolean; onDecide: (decision: "approve" | "reject") => void }) {
  const { t } = useI18n();
  return (
    <div data-testid="lobby-thread-header" className="flex flex-wrap items-center gap-x-3 gap-y-2 border-b border-line bg-iris/[0.06] px-4 py-2.5 sm:px-6">
      <div className="min-w-0 flex-1">
        <p className="truncate text-sm font-semibold text-fg">{t("lobby.threadWith", { name: nodeName(candidate) })}</p>
        <p className="truncate text-[0.68rem] text-faint">
          <span className="font-mono">{shortNodeId(candidate)}</span>
          {" · "}
          {request ? t("lobby.threadRequest", { role: t(`network.enrollment.${request.role}`) }) : t("lobby.threadNoRequest")}
        </p>
      </div>
      {request && (
        <div className="flex shrink-0 items-center gap-2">
          <Button size="sm" variant="primary" icon={<Check className="h-3.5 w-3.5" />} loading={deciding} onClick={() => onDecide("approve")}>
            {t("notifications.admissionApprove")}
          </Button>
          <Button size="sm" variant="ghost" icon={<X className="h-3.5 w-3.5" />} disabled={deciding} onClick={() => onDecide("reject")}>
            {t("notifications.admissionReject")}
          </Button>
        </div>
      )}
    </div>
  );
}

export function NetworkChat() {
  const { t } = useI18n();
  const node = useNode();
  const cluster = useCluster();
  const toast = useToast();
  const [room, setRoom] = useState<NetworkChatRoom>("network");
  const networkChat = useNetworkChat(room);
  const localNodeId = node.data?.node_id ?? "";
  const snapshot = networkChat.data ?? null;
  const inLobby = room === "lobby";
  const lobbyGuests = snapshot?.lobbyGuests ?? [];
  const [thread, setThread] = useState<string | null>(null);
  const [searchParams, setSearchParams] = useSearchParams();
  const askedThread = searchParams.get("lobby");
  // The bell sends the admin here, to the candidate's thread, to decide.
  useEffect(() => {
    if (!askedThread) return;
    setRoom("lobby");
    setThread(askedThread);
    setSearchParams((params) => { params.delete("lobby"); return params; }, { replace: true });
  }, [askedThread, setSearchParams]);
  const admissions = useAdmissions(inLobby);
  const pending = useMemo(() => (admissions.data ?? []).filter((record) => record.status === "pending"), [admissions.data]);
  const [deciding, setDeciding] = useState(false);
  const candidates = useMemo(() => {
    if (!inLobby) return [];
    const seen = new Set<string>();
    // A pending candidate has a thread even before its first word.
    for (const record of pending) seen.add(record.node_id);
    for (const guest of lobbyGuests) seen.add(guest.nodeId);
    for (const message of snapshot?.messages ?? []) {
      const candidate = threadCandidate(message.roomId);
      if (candidate) seen.add(candidate);
    }
    return [...seen];
  }, [inLobby, lobbyGuests, pending, snapshot?.messages]);
  const selected = thread && candidates.includes(thread) ? thread : candidates[0] ?? null;
  const nodeName = useMemo(() => {
    const names = new Map<string, string>();
    for (const peer of cluster.data?.peers ?? []) {
      names.set(peer.node_id, peer.node?.hostname || peer.short_id);
    }
    if (localNodeId && node.data?.hostname) names.set(localNodeId, node.data.hostname);
    for (const participant of snapshot?.participants ?? []) {
      if (participant.hostname && !names.has(participant.nodeId)) {
        names.set(participant.nodeId, participant.hostname);
      }
    }
    // A candidate is not a peer: its name only exists in the card it sent.
    for (const guest of snapshot?.lobbyGuests ?? []) {
      if (guest.card.hostname && !names.has(guest.nodeId)) names.set(guest.nodeId, guest.card.hostname);
    }
    return (id: string) => names.get(id) || shortNodeId(id);
  }, [cluster.data?.peers, localNodeId, node.data?.hostname, snapshot?.lobbyGuests, snapshot?.participants]);
  const [draft, setDraft] = useState("");
  const [sending, setSending] = useState(false);
  const [showParticipants, setShowParticipants] = useState(false);
  const fileInput = useRef<HTMLInputElement>(null);
  const messagesRef = useRef<HTMLDivElement>(null);
  const autoFollow = useRef(true);
  const typingTimer = useRef<number | undefined>(undefined);
  const typingActive = useRef(false);

  useEffect(() => {
    if (networkChat.error) toast.fromError(t("networkChat.loadFailed"), networkChat.error);
  }, [networkChat.error, t, toast]);

  const refresh = useCallback(async () => {
    await networkChat.refetch();
  }, [networkChat.refetch]);

  useEffect(() => {
    if (!inLobby && snapshot?.messages.length) markNetworkChatRead(snapshot.messages);
    if (inLobby && snapshot?.messages.length) markLobbyRead(snapshot.messages);
  }, [inLobby, snapshot?.messages]);

  // The principal closed the lobby while this tab was on it.
  useEffect(() => {
    if (inLobby && snapshot && snapshot.lobbyEnabled === false) setRoom("network");
  }, [inLobby, snapshot]);

  useEffect(() => () => {
    if (typingTimer.current !== undefined) window.clearTimeout(typingTimer.current);
  }, []);

  const send = useCallback(async (request: NetworkChatRequest) => {
    setSending(true);
    try {
      await postNetworkChat(request);
      await refresh();
    } catch (error) {
      toast.fromError(t("networkChat.sendFailed"), error);
    } finally {
      setSending(false);
    }
  }, [refresh, t, toast]);

  const notifyTyping = useCallback((isTyping: boolean) => {
    // Typing is a signal of the members' room; the lobby does not carry it.
    if (!localNodeId || inLobby) return;
    if (typingTimer.current !== undefined) window.clearTimeout(typingTimer.current);
    if (isTyping && typingActive.current) {
      typingTimer.current = window.setTimeout(() => {
        typingActive.current = false;
        void postNetworkChat(typingAction(localNodeId, false));
      }, 1800);
      return;
    }
    if (!isTyping && !typingActive.current) return;
    typingActive.current = isTyping;
    void postNetworkChat(typingAction(localNodeId, isTyping)).catch((error) => {
      toast.fromError(t("networkChat.sendFailed"), error);
    });
    if (isTyping) {
      typingTimer.current = window.setTimeout(() => {
        typingActive.current = false;
        void postNetworkChat(typingAction(localNodeId, false));
      }, 1800);
    }
  }, [inLobby, localNodeId, t, toast]);

  async function submit() {
    const text = draft.trim();
    if (!text || !localNodeId) return;
    setDraft("");
    notifyTyping(false);
    if (inLobby && !selected) return;
    await send(textAction(inLobby ? `${THREAD_PREFIX}${selected}` : room, localNodeId, text));
  }

  async function decide(candidate: string, decision: "approve" | "reject") {
    setDeciding(true);
    try {
      await admissionsApi.decide(candidate, decision);
      toast.success(t(decision === "approve" ? "notifications.admissionApproved" : "notifications.admissionRejected"));
      // Either decision erases the thread: the next one takes its place.
      setThread(null);
      await Promise.all([admissions.refetch(), refresh()]);
    } catch (error) {
      toast.fromError(t("notifications.offerFailed"), error);
    } finally {
      setDeciding(false);
    }
  }

  async function onFile(file: File | undefined) {
    if (!file || !localNodeId) return;
    if (file.size > MAX_FILE_BYTES) {
      toast.error(t("networkChat.fileTooLarge"), t("networkChat.fileLimit"));
      return;
    }
    const bytes = Array.from(new Uint8Array(await file.arrayBuffer()));
    await send(fileAction(localNodeId, file, bytes));
  }

  async function download(message: NetworkChatEvent) {
    if (message.kind !== "file" || !message.fileName) return;
    try {
      const blob = await downloadNetworkChatFile(message.messageId);
      const url = URL.createObjectURL(blob);
      const anchor = document.createElement("a");
      anchor.href = url;
      anchor.download = message.fileName;
      anchor.click();
      window.setTimeout(() => URL.revokeObjectURL(url), 0);
    } catch (error) {
      toast.fromError(t("networkChat.downloadFailed"), error);
    }
  }

  const typing = useMemo(
    () => inLobby ? [] : snapshot?.typing.filter((event) => event.senderNodeId !== localNodeId) ?? [],
    [inLobby, localNodeId, snapshot?.typing],
  );
  const allMessages = snapshot?.messages ?? [];
  const messages = useMemo(
    () => inLobby ? allMessages.filter((message) => threadCandidate(message.roomId) === selected) : allMessages,
    [allMessages, inLobby, selected],
  );
  const latestMessageId = messages[messages.length - 1]?.messageId;

  useEffect(() => {
    const element = messagesRef.current;
    if (element && autoFollow.current) {
      if (typeof element.scrollTo === "function") {
        element.scrollTo({ top: element.scrollHeight, behavior: "auto" });
      } else {
        element.scrollTop = element.scrollHeight;
      }
    }
  }, [latestMessageId, messages.length]);

  if (networkChat.isLoading && !snapshot) {
    return (
      <div className="flex h-full min-h-0 flex-col bg-ink">
          <div className="flex min-h-0 flex-1 items-center justify-center text-sm text-muted">
          {t("networkChat.loading")}
        </div>
      </div>
    );
  }

  if (snapshot && !snapshot.enabled) {
    return (
      <div className="flex h-full min-h-0 flex-col bg-ink">
          <PageHeader
          title={t("networkChat.title")}
          subtitle={t("networkChat.subtitle")}
          className="!mb-0 shrink-0 border-b border-line bg-surface/75 px-3 py-3 sm:px-4"
        />
        <div className="flex flex-1 flex-col items-center justify-center gap-3 p-8 text-center">
          <WifiOff className="h-8 w-8 text-amber" />
          <p className="font-display text-lg font-semibold text-fg">{t("networkChat.disabledTitle")}</p>
          <p className="max-w-md text-sm leading-relaxed text-muted">{t("networkChat.disabledDetail")}</p>
        </div>
      </div>
    );
  }

  return (
    <div className="flex h-full min-h-0 flex-col overflow-hidden bg-ink">
      <PageHeader
        title={t("networkChat.title")}
        subtitle={t("networkChat.subtitle")}
        actions={(
          <div className="flex flex-wrap items-center gap-2">
            <span className="inline-flex items-center gap-1.5 rounded-full border border-aqua/30 bg-aqua/10 px-2.5 py-1 text-xs text-aqua">
              <Wifi className="h-3.5 w-3.5" />{t("networkChat.online")}
            </span>
            <button
              type="button"
              aria-expanded={showParticipants}
              onClick={() => setShowParticipants((visible) => !visible)}
              className="inline-flex items-center gap-1.5 rounded-full border border-line bg-raised px-2.5 py-1 text-xs text-muted transition-colors hover:border-line-strong hover:text-fg lg:hidden"
            >
              <Users className="h-3.5 w-3.5" />
              {showParticipants ? t("networkChat.hideOnline") : t("networkChat.showOnline")}
            </button>
          </div>
        )}
        className="!mb-0 shrink-0 border-b border-line bg-surface/75 px-3 py-3 sm:px-4"
      />
      {snapshot?.lobbyEnabled && (
        <Tabs
          label={t("lobby.rooms")}
          value={room}
          onChange={setRoom}
          items={[
            { value: "network", label: t("lobby.roomNetwork") },
            {
              value: "lobby",
              label: t("lobby.roomLobby"),
              badge: lobbyGuests.length > 0 && <span className="rounded-full bg-aqua/20 px-1.5 py-0.5 text-[0.62rem] text-aqua">{lobbyGuests.length}</span>,
            },
          ]}
        />
      )}
      <div className={`grid min-h-0 flex-1 ${inLobby ? "lg:grid-cols-[minmax(0,1fr)_18rem]" : "lg:grid-cols-[minmax(0,1fr)_15rem]"}`}>
        <div className="flex min-h-0 flex-col">
          {inLobby && selected && (
            <LobbyThreadHeader
              candidate={selected}
              request={pending.find((record) => record.node_id === selected)}
              nodeName={nodeName}
              deciding={deciding}
              onDecide={(decision) => void decide(selected, decision)}
            />
          )}
          <div
            ref={messagesRef}
            data-testid="network-chat-messages"
            onScroll={(event) => {
              autoFollow.current = isNearBottom(event.currentTarget);
            }}
            className="min-h-0 flex-1 space-y-3 overflow-y-auto px-4 py-5 sm:px-6"
          >
            {messages.length ? messages.map((message) => <MessageRow key={message.messageId} message={message} localNodeId={localNodeId} nodeName={nodeName} onDownload={(fileMessage) => { void download(fileMessage); }} />) : <Empty title={t("networkChat.emptyTitle")}>{t(inLobby ? "lobby.emptyMember" : "networkChat.emptyDetail")}</Empty>}
            {typing.length > 0 && <p className="text-xs italic text-muted">{t("networkChat.typing", { node: nodeName(typing[0].senderNodeId) })}</p>}
          </div>
          <div className="border-t border-line bg-ink/40 p-3 sm:p-4">
            <div className="flex items-end gap-2">
              {!inLobby && (
                <button type="button" className="grid h-9 w-9 shrink-0 place-items-center rounded-lg border border-line bg-raised text-muted transition-colors hover:border-line-strong hover:text-fg" onClick={() => fileInput.current?.click()} aria-label={t("networkChat.attach")}>
                  <FileUp className="h-4 w-4" />
                </button>
              )}
              <input ref={fileInput} type="file" className="hidden" onChange={(event) => { void onFile(event.target.files?.[0]); event.currentTarget.value = ""; }} />
              <Input
                value={draft}
                placeholder={t(inLobby ? "lobby.placeholderMember" : "networkChat.placeholder")}
                onChange={(event) => { setDraft(event.target.value); notifyTyping(event.target.value.trim().length > 0); }}
                onBlur={() => notifyTyping(false)}
                onKeyDown={(event) => { if (event.key === "Enter" && !event.shiftKey) { event.preventDefault(); void submit(); } }}
                disabled={sending || !localNodeId || (inLobby && !selected)}
              />
              <Button variant="primary" className="h-9 w-9 shrink-0 px-0" aria-label={t("networkChat.send")} disabled={!draft.trim() || sending} loading={sending} onClick={() => void submit()}>
                <Send className="h-4 w-4" />
              </Button>
            </div>
            <p className="mt-2 text-[0.68rem] text-faint">{t(inLobby ? "lobby.hintMember" : "networkChat.hint")}</p>
          </div>
        </div>
        <div className={showParticipants ? "block lg:block" : "hidden lg:block"}>
          {inLobby ? <LobbyThreadList candidates={candidates} guests={lobbyGuests} pending={pending} selected={selected} onSelect={setThread} nodeName={nodeName} /> : <ParticipantList participants={snapshot?.participants ?? []} nodeName={nodeName} />}
        </div>
      </div>
    </div>
  );
}
