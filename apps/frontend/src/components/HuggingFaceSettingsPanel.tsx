import { KeyRound, Save, Trash2 } from "lucide-react";
import { useState } from "react";
import { useI18n } from "../i18n/provider";
import { saveHuggingFaceToken } from "../lib/api";
import { useHuggingFaceTokenConfig, useInvalidate } from "../lib/queries";
import { useToast } from "../lib/toast";
import {
  Button,
  ErrorState,
  Field,
  Footnote,
  Input,
  Mono,
  Panel,
  PanelHeader,
  Skeleton,
} from "./ui";

/**
 * Local configuration of the provider. The field starts empty on purpose: the API only
 * returns a mask, and we never use the persisted value to fill a
 * password input.
 */
export function HuggingFaceSettingsPanel() {
  const { t } = useI18n();
  const config = useHuggingFaceTokenConfig();
  const invalidate = useInvalidate();
  const toast = useToast();
  const [draft, setDraft] = useState("");
  const [saving, setSaving] = useState(false);

  async function saveToken() {
    const token = draft.trim();
    if (!token) return;
    setSaving(true);
    try {
      await saveHuggingFaceToken(token);
      setDraft("");
      toast.success(t("huggingFace.saved"));
      invalidate();
    } catch (error) {
      toast.fromError(t("huggingFace.saveFailed"), error);
    } finally {
      setSaving(false);
    }
  }

  async function clearToken() {
    setSaving(true);
    try {
      await saveHuggingFaceToken(null);
      setDraft("");
      toast.success(t("huggingFace.cleared"));
      invalidate();
    } catch (error) {
      toast.fromError(t("huggingFace.saveFailed"), error);
    } finally {
      setSaving(false);
    }
  }

  return (
    <Panel id="hugging-face">
      <PanelHeader
        title={t("huggingFace.title")}
        subtitle={t("huggingFace.subtitle")}
        actions={
          <div className="flex flex-wrap items-center gap-2">
            <Button
              variant="primary"
              icon={<Save className="h-3.5 w-3.5" />}
              loading={saving}
              disabled={!draft.trim()}
              onClick={() => void saveToken()}
            >
              {t("huggingFace.save")}
            </Button>
            <Button
              variant="ghost"
              icon={<Trash2 className="h-3.5 w-3.5" />}
              loading={saving}
              disabled={!config.data?.configured}
              onClick={() => void clearToken()}
            >
              {t("huggingFace.clear")}
            </Button>
          </div>
        }
      />
      <div className="space-y-3 px-5 py-4">
        {config.isError ? (
          <ErrorState
            title={t("huggingFace.saveFailed")}
            error={config.error}
            onRetry={() => void config.refetch()}
          />
        ) : (
          <>
            <Field label={t("huggingFace.tokenLabel")} hint={t("huggingFace.tokenHint")}>
              <Input
                type="password"
                name="hugging-face-token"
                autoComplete="new-password"
                spellCheck={false}
                value={draft}
                placeholder={config.data?.masked ?? t("huggingFace.tokenPlaceholder")}
                aria-describedby="hugging-face-token-detail"
                onChange={(event) => setDraft(event.target.value)}
                onKeyDown={(event) => {
                  if (event.key === "Enter") void saveToken();
                }}
              />
            </Field>
            <div id="hugging-face-token-detail" className="flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-muted">
              <KeyRound className="h-3.5 w-3.5 text-faint" />
              {config.isLoading ? (
                <Skeleton className="h-4 w-48" />
              ) : (
                <>
                  <span>
                    {t("huggingFace.source", {
                      source: config.data?.masked ? config.data.source : "none",
                    })}
                  </span>
                  {config.data?.masked && <Mono>{config.data.masked}</Mono>}
                </>
              )}
            </div>
            <Footnote icon={<KeyRound className="h-3.5 w-3.5" />}>
              {t("huggingFace.precedence")}
            </Footnote>
          </>
        )}
      </div>
    </Panel>
  );
}
