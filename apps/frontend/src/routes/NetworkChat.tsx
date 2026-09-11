import { Download, FileUp, Send, Users, Wifi, WifiOff } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ChatSurfaceTabs } from "../components/ChatSurfaceTabs";
import { Button, Empty, Input, PageHeader } from "../components/ui";
import { downloadNetworkChatFile, postNetworkChat } from "../lib/api";
import { isNearBottom } from "../lib/chatScroll";
import { randomId } from "../lib/id";
import { useCluster, useNetworkChat, useNode } from "../lib/queries";
import { markNetworkChatRead } from "../lib/networkChatUnread";
import { useToast } from "../lib/toast";
import { useI18n } from "../i18n/provider";
import type {
  NetworkChatEvent,
  NetworkChatParticipant,
  NetworkChatRequest,
} from "../lib/types";

const ROOM_ID = "network";
const MAX_FILE_BYTES = 8 * 1024 * 1024;

function id(): string {
  return randomId();
}

function shortNodeId(nodeId: string): string {
  return nodeId.length > 12 ? nodeId.slice(0, 12) : nodeId;
}

function now(): string {
  return new Date().toISOString();
}

function textAction(senderNodeId: string, text: string): NetworkChatRequest {
  return {
    action: {
      kind: "text",
      message_id: id(),
      room_id: ROOM_ID,
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
      room_id: ROOM_ID,
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
      room_id: ROOM_ID,
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

export function NetworkChat() {
  const { t } = useI18n();
  const node = useNode();
  const cluster = useCluster();
  const toast = useToast();
  const networkChat = useNetworkChat();
  const localNodeId = node.data?.node_id ?? "";
  const snapshot = networkChat.data ?? null;
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
    return (id: string) => names.get(id) || shortNodeId(id);
  }, [cluster.data?.peers, localNodeId, node.data?.hostname, snapshot?.participants]);
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
    if (snapshot?.messages.length) markNetworkChatRead(snapshot.messages);
  }, [snapshot?.messages]);

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
    if (!localNodeId) return;
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
  }, [localNodeId, t, toast]);

  async function submit() {
    const text = draft.trim();
    if (!text || !localNodeId) return;
    setDraft("");
    notifyTyping(false);
    await send(textAction(localNodeId, text));
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
    () => snapshot?.typing.filter((event) => event.senderNodeId !== localNodeId) ?? [],
    [localNodeId, snapshot?.typing],
  );
  const messages = snapshot?.messages ?? [];
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
        <ChatSurfaceTabs />
        <div className="flex min-h-0 flex-1 items-center justify-center text-sm text-muted">
          {t("networkChat.loading")}
        </div>
      </div>
    );
  }

  if (snapshot && !snapshot.enabled) {
    return (
      <div className="flex h-full min-h-0 flex-col bg-ink">
        <ChatSurfaceTabs />
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
      <ChatSurfaceTabs />
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
      <div className="grid min-h-0 flex-1 lg:grid-cols-[minmax(0,1fr)_15rem]">
        <div className="flex min-h-0 flex-col">
          <div
            ref={messagesRef}
            data-testid="network-chat-messages"
            onScroll={(event) => {
              autoFollow.current = isNearBottom(event.currentTarget);
            }}
            className="min-h-0 flex-1 space-y-3 overflow-y-auto px-4 py-5 sm:px-6"
          >
            {messages.length ? messages.map((message) => <MessageRow key={message.messageId} message={message} localNodeId={localNodeId} nodeName={nodeName} onDownload={(fileMessage) => { void download(fileMessage); }} />) : <Empty title={t("networkChat.emptyTitle")}>{t("networkChat.emptyDetail")}</Empty>}
            {typing.length > 0 && <p className="text-xs italic text-muted">{t("networkChat.typing", { node: nodeName(typing[0].senderNodeId) })}</p>}
          </div>
          <div className="border-t border-line bg-ink/40 p-3 sm:p-4">
            <div className="flex items-end gap-2">
              <button type="button" className="grid h-9 w-9 shrink-0 place-items-center rounded-lg border border-line bg-raised text-muted transition-colors hover:border-line-strong hover:text-fg" onClick={() => fileInput.current?.click()} aria-label={t("networkChat.attach")}>
                <FileUp className="h-4 w-4" />
              </button>
              <input ref={fileInput} type="file" className="hidden" onChange={(event) => { void onFile(event.target.files?.[0]); event.currentTarget.value = ""; }} />
              <Input
                value={draft}
                placeholder={t("networkChat.placeholder")}
                onChange={(event) => { setDraft(event.target.value); notifyTyping(event.target.value.trim().length > 0); }}
                onBlur={() => notifyTyping(false)}
                onKeyDown={(event) => { if (event.key === "Enter" && !event.shiftKey) { event.preventDefault(); void submit(); } }}
                disabled={sending || !localNodeId}
              />
              <Button variant="primary" className="h-9 w-9 shrink-0 px-0" aria-label={t("networkChat.send")} disabled={!draft.trim() || sending} loading={sending} onClick={() => void submit()}>
                <Send className="h-4 w-4" />
              </Button>
            </div>
            <p className="mt-2 text-[0.68rem] text-faint">{t("networkChat.hint")}</p>
          </div>
        </div>
        <div className={showParticipants ? "block lg:block" : "hidden lg:block"}>
          <ParticipantList participants={snapshot?.participants ?? []} nodeName={nodeName} />
        </div>
      </div>
    </div>
  );
}
