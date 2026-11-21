import { AlertTriangle, ArrowLeft, Bot, ChevronDown, ChevronRight, Loader2, Send, Wrench } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { useI18n } from "../../i18n/provider";
import { openAcp, type AcpConnection, type AcpSessionInfo, type ModelChoice } from "../../lib/acp/connection";
import {
  answerPermission,
  applyUpdate,
  emptyConversation,
  pushPermission,
  pushUser,
  type AcpConversation,
  type AcpItem,
  type AcpToolCall,
} from "../../lib/acp/session";
import { openPlugin } from "../../lib/api";
import type { ChatAgent } from "../../lib/types";
import { Markdown } from "../Markdown";
import { Badge, Button } from "../ui";

type Phase = "starting" | "connecting" | "ready" | "lost" | "failed";

const TOOL_TONE: Record<AcpToolCall["status"], "neutral" | "sky" | "aqua" | "rose"> = {
  pending: "neutral",
  in_progress: "sky",
  completed: "aqua",
  failed: "rose",
};

/** One tool call: its title and status, with what it got and returned on demand. */
function ToolCard({ call }: { call: AcpToolCall }) {
  const { t } = useI18n();
  const [open, setOpen] = useState(false);
  const detail = call.input !== undefined || call.output;
  return (
    <div className="rounded-lg border border-line bg-raised text-[0.8rem]" data-testid="acp-tool-card">
      <button
        type="button"
        className="flex w-full items-center gap-2 px-3 py-2 text-left"
        onClick={() => setOpen((value) => !value)}
        disabled={!detail}
        aria-expanded={open}
      >
        {detail ? open ? <ChevronDown className="h-3.5 w-3.5" /> : <ChevronRight className="h-3.5 w-3.5" /> : <Wrench className="h-3.5 w-3.5 text-faint" />}
        <span className="min-w-0 flex-1 truncate font-mono">{call.title}</span>
        <Badge tone={TOOL_TONE[call.status]}>{t(`chat.agent.tool.${call.status}`)}</Badge>
      </button>
      {open && detail && (
        <div className="space-y-2 border-t border-line px-3 py-2 font-mono text-[0.72rem] text-muted">
          {call.input !== undefined && <pre className="whitespace-pre-wrap break-all">{JSON.stringify(call.input, null, 2)}</pre>}
          {call.output && <pre className="max-h-64 overflow-auto whitespace-pre-wrap break-all">{call.output}</pre>}
        </div>
      )}
    </div>
  );
}

/** A permission the agent asks for, with the options it offers. */
function PermissionCard({
  item,
  onAnswer,
}: {
  item: Extract<AcpItem, { kind: "permission" }>;
  onAnswer: (optionId: string) => void;
}) {
  const { t } = useI18n();
  const chosen = item.request.options.find((option) => option.optionId === item.answered);
  return (
    <div className="rounded-lg border border-amber/40 bg-amber/5 px-3 py-2 text-[0.82rem]" data-testid="acp-permission-card">
      <div className="mb-2 flex items-center gap-2 font-medium">
        <AlertTriangle className="h-3.5 w-3.5 text-amber" />
        {t("chat.agent.permission", { title: item.request.toolCall.title ?? "" })}
      </div>
      {item.answered === null ? (
        <div className="flex flex-wrap gap-2">
          {item.request.options.map((option) => (
            <Button
              key={option.optionId}
              size="sm"
              variant={option.kind.startsWith("allow") ? "primary" : "outline"}
              onClick={() => onAnswer(option.optionId)}
            >
              {option.name}
            </Button>
          ))}
        </div>
      ) : (
        <p className="text-muted">{t("chat.agent.permissionAnswered", { choice: chosen?.name ?? item.answered })}</p>
      )}
    </div>
  );
}

function Item({ item, onAnswer }: { item: AcpItem; onAnswer: (requestId: string, optionId: string) => void }) {
  switch (item.kind) {
    case "user":
      return (
        <div className="ml-auto max-w-[85%] whitespace-pre-wrap rounded-2xl bg-iris/10 px-4 py-2.5 text-[0.9rem]">{item.text}</div>
      );
    case "agent":
      return (
        <div className="text-[0.92rem]">
          <Markdown>{item.text}</Markdown>
        </div>
      );
    case "thought":
      return <div className="border-l-2 border-line pl-3 text-[0.8rem] italic text-faint">{item.text}</div>;
    case "tool":
      return <ToolCard call={item.call} />;
    case "plan":
      return (
        <ul className="space-y-1 rounded-lg border border-line px-3 py-2 text-[0.8rem]">
          {item.entries.map((entry, index) => (
            <li key={index} className={entry.status === "completed" ? "text-faint line-through" : ""}>
              {entry.content}
            </li>
          ))}
        </ul>
      );
    case "permission":
      return <PermissionCard item={item} onAnswer={(optionId) => onAnswer(item.requestId, optionId)} />;
  }
}

/**
 * The Chat talking to an ACP agent a plugin of this machine serves (Goose
 * is the first). A stopped on-demand plugin is opened first; the parent
 * passes the agent again as its status changes, and `null` when the plugin
 * no longer serves it.
 */
export function AgentChat({ agent, onLeave }: { agent: ChatAgent | null; onLeave: () => void }) {
  const { t } = useI18n();
  const [phase, setPhase] = useState<Phase>("starting");
  const [error, setError] = useState<string | null>(null);
  const [conversation, setConversation] = useState<AcpConversation>(emptyConversation);
  const [sessionId, setSessionId] = useState<string | null>(null);
  const [model, setModel] = useState<ModelChoice | null>(null);
  const [earlier, setEarlier] = useState<AcpSessionInfo[]>([]);
  const [draft, setDraft] = useState("");
  const [busy, setBusy] = useState(false);
  const [attempt, setAttempt] = useState(0);
  const connection = useRef<AcpConnection | null>(null);
  const resolvers = useRef(new Map<string, (optionId: string | null) => void>());
  const opened = useRef(false);
  const ready = agent?.status === "ready";
  const agentName = agent?.name ?? "";

  // A stopped plugin is opened once; the parent's polling brings it to ready.
  useEffect(() => {
    if (!agent || ready || opened.current) return;
    opened.current = true;
    openPlugin(agent.plugin_code).catch((reason: unknown) => {
      setPhase("failed");
      setError(reason instanceof Error ? reason.message : String(reason));
    });
  }, [agent, ready]);

  useEffect(() => {
    if (!agent || !ready) return;
    let cancelled = false;
    setPhase("connecting");
    setError(null);
    setConversation(emptyConversation());
    const pending = resolvers.current;
    let opened: AcpConnection | null = null;
    openAcp(agent.proxy_path, {
      onUpdate: (notification) => {
        if (!cancelled) setConversation((state) => applyUpdate(state, notification.update));
      },
      onPermission: (requestId, request) =>
        new Promise<string | null>((resolve) => {
          pending.set(requestId, resolve);
          setConversation((state) => pushPermission(state, requestId, request));
        }),
    })
      .then(async (conn) => {
        opened = conn;
        if (cancelled) {
          conn.close();
          return;
        }
        connection.current = conn;
        void conn.closed.then(() => {
          if (!cancelled) setPhase("lost");
        });
        const session = await conn.newSession(agent.cwd);
        if (cancelled) return;
        setSessionId(session.sessionId);
        setModel(session.model);
        setPhase("ready");
        setEarlier(await conn.listSessions(agent.cwd).catch(() => []));
      })
      .catch((reason: unknown) => {
        if (cancelled) return;
        setPhase("failed");
        setError(reason instanceof Error ? reason.message : String(reason));
      });
    return () => {
      cancelled = true;
      for (const resolve of pending.values()) resolve(null);
      pending.clear();
      opened?.close();
      connection.current = null;
    };
    // The proxy path and workspace identify the agent; status changes past ready do not reconnect.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [agent?.proxy_path, agent?.cwd, ready, attempt]);

  const answer = useCallback((requestId: string, optionId: string) => {
    resolvers.current.get(requestId)?.(optionId);
    resolvers.current.delete(requestId);
    setConversation((state) => answerPermission(state, requestId, optionId));
  }, []);

  const send = async () => {
    const text = draft.trim();
    const conn = connection.current;
    if (!text || !conn || !sessionId || busy) return;
    setDraft("");
    setBusy(true);
    setConversation((state) => pushUser(state, text));
    try {
      await conn.prompt(sessionId, text);
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : String(reason));
    } finally {
      setBusy(false);
    }
  };

  const reopen = async (id: string) => {
    const conn = connection.current;
    if (!conn || !agent || !id) return;
    setConversation(emptyConversation());
    setSessionId(id);
    const loaded = await conn.loadSession(id, agent.cwd).catch((reason: unknown) => {
      setError(reason instanceof Error ? reason.message : String(reason));
      return null;
    });
    if (loaded) setModel(loaded.model);
  };

  const chooseModel = (value: string) => {
    const conn = connection.current;
    if (!conn || !sessionId || !model) return;
    setModel({ ...model, current: value });
    void conn.setModel(sessionId, model.configId, value).catch((reason: unknown) =>
      setError(reason instanceof Error ? reason.message : String(reason)),
    );
  };

  if (!agent) {
    return (
      <div className="flex h-full flex-col items-center justify-center gap-3 p-6 text-center">
        <p className="text-sm text-muted">{t("chat.agent.gone")}</p>
        <Button size="sm" variant="outline" icon={<ArrowLeft className="h-3.5 w-3.5" />} onClick={onLeave}>
          {t("chat.agent.backToModel")}
        </Button>
      </div>
    );
  }

  const header = (
    <div className="flex h-14 shrink-0 items-center gap-3 border-b border-line px-4 sm:px-6">
      <Bot className="h-4 w-4 text-faint" />
      <span className="font-medium">{agent.name}</span>
      {agent.version && <span className="text-xs text-faint">{agent.version}</span>}
      <div className="ml-auto flex items-center gap-2">
        {earlier.length > 0 && (
          <select
            aria-label={t("chat.agent.earlier")}
            className="h-8 max-w-[12rem] truncate rounded-lg border border-line bg-raised px-2 text-[0.78rem]"
            value=""
            onChange={(event) => void reopen(event.target.value)}
          >
            <option value="">{t("chat.agent.earlier")}</option>
            {earlier.map((session) => (
              <option key={session.sessionId} value={session.sessionId}>
                {session.title || session.sessionId}
              </option>
            ))}
          </select>
        )}
        {model && (
          <select
            aria-label={t("chat.agent.modelLabel")}
            className="h-8 max-w-[12rem] truncate rounded-lg border border-line bg-raised px-2 text-[0.78rem]"
            value={model.current}
            onChange={(event) => chooseModel(event.target.value)}
          >
            {model.options.map((option) => (
              <option key={option.value} value={option.value}>
                {option.name}
              </option>
            ))}
          </select>
        )}
        <Button size="sm" variant="ghost" icon={<ArrowLeft className="h-3.5 w-3.5" />} onClick={onLeave}>
          {t("chat.agent.backToModel")}
        </Button>
      </div>
    </div>
  );

  if (phase === "starting" || phase === "connecting") {
    return (
      <div className="flex h-full min-h-0 flex-col">
        {header}
        <div className="flex flex-1 items-center justify-center gap-2 text-sm text-muted">
          <Loader2 className="h-4 w-4 animate-spin" />
          {t(phase === "starting" ? "chat.agent.starting" : "chat.agent.connecting", { name: agentName })}
        </div>
      </div>
    );
  }

  if (phase === "lost" || phase === "failed") {
    return (
      <div className="flex h-full min-h-0 flex-col">
        {header}
        <div className="flex flex-1 flex-col items-center justify-center gap-3 p-6 text-center">
          <p className="text-sm text-muted">{t(phase === "lost" ? "chat.agent.lost" : "chat.agent.failed", { name: agentName })}</p>
          {error && <p className="max-w-lg font-mono text-xs text-faint">{error}</p>}
          <Button
            size="sm"
            variant="outline"
            onClick={() => {
              opened.current = false;
              setAttempt((value) => value + 1);
            }}
          >
            {t("chat.agent.retry")}
          </Button>
        </div>
      </div>
    );
  }

  return (
    <div className="flex h-full min-h-0 flex-col">
      {header}
      <div className="min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto w-full max-w-3xl space-y-4 px-4 py-6 sm:px-6">
          {conversation.items.map((item, index) => (
            <Item key={index} item={item} onAnswer={answer} />
          ))}
          {busy && (
            <div className="flex items-center gap-2 text-xs text-faint">
              <Loader2 className="h-3.5 w-3.5 animate-spin" />
              {t("chat.agent.working", { name: agentName })}
            </div>
          )}
          {error && <p className="font-mono text-xs text-rose">{error}</p>}
        </div>
      </div>
      <div className="shrink-0 border-t border-line px-4 py-3 sm:px-6">
        <form
          className="mx-auto flex w-full max-w-3xl items-end gap-2"
          onSubmit={(event) => {
            event.preventDefault();
            void send();
          }}
        >
          <textarea
            aria-label={t("chat.agent.message", { name: agentName })}
            className="min-h-[2.5rem] flex-1 resize-none rounded-lg border border-line bg-raised px-3 py-2 text-[0.9rem] focus:border-iris focus:outline-none"
            rows={1}
            value={draft}
            onChange={(event) => setDraft(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter" && !event.shiftKey) {
                event.preventDefault();
                void send();
              }
            }}
          />
          <Button type="submit" size="sm" icon={<Send className="h-3.5 w-3.5" />} disabled={busy || !draft.trim()} aria-label={t("chat.agent.send")}>
            {t("chat.agent.send")}
          </Button>
        </form>
      </div>
    </div>
  );
}
