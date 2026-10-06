/**
 * A guide from Documentation, opened in a dialog next to the fields it explains.
 *
 * The deployment inspector puts a help button on each category; reading the
 * guide there keeps the operator on the screen being configured. The text is
 * the same `?guia=` page Documentation serves, so it is written once.
 *
 * Nothing in the dialog leaves the screen: the operator may be in the middle
 * of a deployment. A reference link at the end of the guide (`deep`) opens
 * that document inside the same dialog, with a way back to the guide. The
 * documents load on demand, so the inspector does not carry them.
 */
import { useEffect, useMemo, useState } from "react";
import { ArrowLeft, HelpCircle } from "lucide-react";
import { Modal } from "./Confirm";
import { Markdown } from "./Markdown";
import { Button, Skeleton } from "./ui";
import { GuideView } from "../guides/Guide";
import { guidesFor } from "../guides/content";
import { useI18n } from "../i18n/provider";

const DOCUMENTS = import.meta.glob<string>("../docs/**/*.md", {
  query: "?raw",
  import: "default",
});

function withoutLeadingH1(markdown: string): string {
  return markdown.replace(/^\s*#\s+[^\n]*\n/, "");
}

/** One reference document, loaded when it is opened. */
function ReferenceDocument({ doc }: { doc: string }) {
  const { t } = useI18n();
  const [content, setContent] = useState<string | null | undefined>(undefined);
  useEffect(() => {
    let current = true;
    setContent(undefined);
    const load = DOCUMENTS[`../docs/${doc}`];
    if (!load) {
      setContent(null);
      return;
    }
    load()
      .then((text) => current && setContent(text))
      .catch(() => current && setContent(null));
    return () => {
      current = false;
    };
  }, [doc]);
  if (content === undefined) return <Skeleton className="h-40" />;
  if (content === null) return <p className="text-sm text-muted">{t("docs.noDoc")}</p>;
  return <Markdown>{withoutLeadingH1(content)}</Markdown>;
}

function GuideDialogBody({ guideId, onClose }: { guideId: string; onClose: () => void }) {
  const { t, locale } = useI18n();
  const guide = useMemo(() => guidesFor(locale).find((g) => g.id === guideId), [locale, guideId]);
  const [doc, setDoc] = useState<string | null>(null);
  if (!guide) return null;
  return (
    <Modal
      open
      size="xl"
      title={guide.title}
      titleHidden
      onClose={onClose}
      footer={
        <>
          {doc && (
            <Button variant="ghost" icon={<ArrowLeft className="h-4 w-4" />} onClick={() => setDoc(null)}>
              {t("inspector.guideBack")}
            </Button>
          )}
          <Button variant="primary" onClick={onClose}>
            {t("common.close")}
          </Button>
        </>
      }
    >
      {doc ? <ReferenceDocument doc={doc} /> : <GuideView guide={guide} onOpenDoc={setDoc} />}
    </Modal>
  );
}

/** The help dialog itself; rendered only while open. */
export function GuideDialog({
  guideId,
  open,
  onClose,
}: {
  guideId: string;
  open: boolean;
  onClose: () => void;
}) {
  return open ? <GuideDialogBody guideId={guideId} onClose={onClose} /> : null;
}

/** The round "?" button that opens a guide, for a section heading. */
export function GuideHelpButton({
  label,
  onClick,
  className = "",
}: {
  label: string;
  onClick: () => void;
  className?: string;
}) {
  return (
    <button
      type="button"
      aria-label={label}
      title={label}
      onClick={onClick}
      className={`inline-flex h-5 w-5 shrink-0 items-center justify-center rounded-full opacity-80 transition hover:bg-current/10 hover:opacity-100 ${className}`}
    >
      <HelpCircle className="h-3.5 w-3.5" aria-hidden />
    </button>
  );
}
