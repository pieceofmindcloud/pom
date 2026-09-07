import clsx from "clsx";
import { Trash2, Plus } from "lucide-react";
import { Button } from "./ui";
import { useI18n } from "../i18n/provider";
import { since } from "../lib/format";
import type { ChatSession } from "../lib/chatStore";

export function ChatSessionList({
  sessions,
  activeId,
  onPick,
  onNew,
  onDelete,
  compact = false,
  collapsed = false,
}: {
  sessions: ChatSession[];
  activeId: string;
  onPick: (session: ChatSession) => void;
  onNew: () => void;
  onDelete: (id: string) => void;
  compact?: boolean;
  collapsed?: boolean;
}) {
  const { t } = useI18n();
  const Wrapper = compact ? "div" : "aside";

  return (
    <Wrapper
      className={clsx(
        "flex min-h-0 flex-col border-line bg-surface",
        compact ? "flex-1" : "hidden w-[248px] shrink-0 border-r md:flex",
      )}
    >
      <div className={clsx("shrink-0", compact ? "pb-2" : "p-3")}>
        <Button
          variant="primary"
          size={compact ? "sm" : "md"}
          className={clsx("w-full", collapsed && "px-0")}
          icon={<Plus className="h-4 w-4" />}
          aria-label={t("chat.new")}
          title={t("chat.new")}
          onClick={onNew}
        >
          <span className={collapsed ? "sr-only" : undefined}>{t("chat.new")}</span>
        </Button>
      </div>
      <div
        data-testid="chat-session-list"
        role="list"
        aria-label={t("chat.history")}
        className={clsx(
          "min-h-0 flex-1 overflow-y-auto",
          compact ? "space-y-0.5 px-0.5 pb-2" : "px-2 pb-3",
        )}
      >
        {sessions.length === 0 ? (
          <p className="px-3 py-6 text-center text-xs leading-relaxed text-faint">
            {t("chat.localOnly")}
          </p>
        ) : (
          sessions.map((session) => (
            <div
              key={session.id}
              role="listitem"
              className={clsx(
                "group relative rounded-lg transition-colors duration-150",
                compact ? "mb-0.5" : "mb-0.5",
                session.id === activeId
                  ? "bg-iris/12 ring-1 ring-inset ring-iris/25"
                  : "hover:bg-line/40",
              )}
            >
              <button
                type="button"
                aria-pressed={session.id === activeId}
                title={session.title}
                onClick={() => onPick(session)}
                className={clsx(
                  "block w-full min-w-0 text-left",
                  compact ? "px-2.5 py-2 pr-8" : "px-3 py-2 pr-8",
                )}
              >
                <span className="block truncate text-[0.82rem] text-fg">{session.title}</span>
                <span className="block truncate text-[0.68rem] text-faint">
                  {since(session.updatedAt)} · {session.messages.length}{" "}
                  {t("chat.message", { count: session.messages.length })}
                </span>
              </button>
              <button
                type="button"
                onClick={() => onDelete(session.id)}
                aria-label={t("chat.delete", { title: session.title })}
                className="absolute right-1.5 top-1/2 hidden -translate-y-1/2 rounded p-1.5 text-faint transition-colors hover:text-rose focus-visible:block focus-visible:text-rose group-hover:block"
              >
                <Trash2 className="h-3.5 w-3.5" />
              </button>
            </div>
          ))
        )}
      </div>
    </Wrapper>
  );
}
