import { FileDown, RotateCcw } from "lucide-react";
import { useEffect, useState } from "react";
import { Modal } from "./Confirm";
import { Button } from "./ui";
import { api } from "../lib/api";
import { useI18n } from "../i18n/provider";

/** Where the GGUF of the deployment is: its path in a machine's catalog. */
export interface ChatTemplateSource {
  path: string;
  /** The machine that holds `path`; absent is the node serving this page. */
  nodeId?: string | null;
}

/** `POST /api/ui/model-chat-template` (`ModelChatTemplateResponse`). */
interface ChatTemplateResponse {
  chat_template: string | null;
}

/** Read the GGUF's `tokenizer.chat_template` from the machine that holds it. */
export async function fetchGgufChatTemplate(source: ChatTemplateSource): Promise<string | null> {
  const reply = await api.post<ChatTemplateResponse>("/api/ui/model-chat-template", {
    path: source.path,
    node_id: source.nodeId ?? null,
  });
  return reply.chat_template;
}

/**
 * The deployment's Jinja chat template (`RuntimeParams.chat_template`). Empty
 * keeps the GGUF's own; a text replaces it, and the engine checks it when the
 * model loads, so a template its Jinja engine cannot run fails the load with
 * the reason instead of the first chat.
 */
export function ChatTemplateDialog({
  open,
  value,
  source,
  readOnly = false,
  onClose,
  onApply,
}: {
  open: boolean;
  value: string | null | undefined;
  source?: ChatTemplateSource | null;
  readOnly?: boolean;
  onClose: () => void;
  onApply?: (next: string | null) => void;
}) {
  const { t } = useI18n();
  const [draft, setDraft] = useState(value ?? "");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!open) return;
    setDraft(value ?? "");
    setError(null);
  }, [open, value]);

  async function loadFromGguf() {
    if (!source) return;
    setLoading(true);
    setError(null);
    try {
      const template = await fetchGgufChatTemplate(source);
      if (template == null) setError(t("chatTemplate.ggufNone"));
      else setDraft(template);
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : t("chatTemplate.ggufFailed"));
    } finally {
      setLoading(false);
    }
  }

  const custom = draft.trim().length > 0;

  return (
    <Modal
      open={open}
      size="xl"
      title={t("chatTemplate.title")}
      onClose={onClose}
      footer={readOnly ? (
        <Button variant="ghost" onClick={onClose}>{t("common.close")}</Button>
      ) : (
        <>
          <Button variant="ghost" onClick={onClose}>{t("common.cancel")}</Button>
          <Button
            variant="primary"
            onClick={() => {
              onApply?.(custom ? draft : null);
              onClose();
            }}
          >
            {t("chatTemplate.apply")}
          </Button>
        </>
      )}
    >
      <div className="space-y-2">
        <p className="text-xs leading-relaxed text-muted">{t("chatTemplate.hint")}</p>
        {!readOnly && (
          <div className="flex flex-wrap gap-2">
            <Button
              variant="outline"
              disabled={!source}
              loading={loading}
              icon={<FileDown className="h-3.5 w-3.5" />}
              onClick={() => void loadFromGguf()}
              title={source ? undefined : t("chatTemplate.ggufUnavailable")}
            >
              {t("chatTemplate.loadGguf")}
            </Button>
            <Button
              variant="ghost"
              disabled={!custom}
              icon={<RotateCcw className="h-3.5 w-3.5" />}
              onClick={() => setDraft("")}
            >
              {t("chatTemplate.useGguf")}
            </Button>
          </div>
        )}
        {error && <p role="alert" className="break-words text-xs text-rose">{error}</p>}
        <textarea
          aria-label={t("chatTemplate.title")}
          className="h-[55vh] w-full resize-y rounded-md border border-line bg-ink p-2 font-mono text-[0.7rem] leading-relaxed text-fg focus:border-iris/60 focus:outline-none"
          spellCheck={false}
          readOnly={readOnly}
          placeholder={t("chatTemplate.placeholder")}
          value={draft}
          onChange={(event) => setDraft(event.target.value)}
        />
        <p className="text-[0.65rem] text-faint">
          {custom ? t("chatTemplate.stateCustom") : t("chatTemplate.stateGguf")}
        </p>
      </div>
    </Modal>
  );
}
