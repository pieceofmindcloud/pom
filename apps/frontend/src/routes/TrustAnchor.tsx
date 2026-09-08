/**
 * A raiz de confiança desta máquina, e os dois gestos que a envolvem.
 *
 * **Por que esta tela não fica atrás do perfil de administrador.** Antes de
 * ancorar, um nó com `profile: admin` não tem capacidade efetiva de admin: o
 * gateway recusa o escopo `AdminOnly` exatamente nesse estado. Uma tela de
 * ancoragem trancada atrás do perfil seria inalcançável no único momento em
 * que serve para alguma coisa. Por isso ela é livre para os três perfis, e a
 * rota que a alimenta é `CommonOrInternal` - quem recebe a âncora costuma ser
 * um `node` ou um `infer`.
 *
 * **O que a tela decide: nada.** Mostrar a impressão digital é seguro porque
 * ela é pública e não concede nada. Instalar é decidido no servidor, que
 * exige loopback real, nó ainda sem âncora e a impressão digital conferida.
 * `can_install` aqui é só o que evita desenhar um formulário que vai ser
 * recusado; não é a trava.
 */
import { Check, Copy, Fingerprint, Laptop, ShieldAlert, ShieldCheck } from "lucide-react";
import { useState } from "react";
import {
  Badge,
  Button,
  Dot,
  ErrorState,
  Field,
  Footnote,
  Input,
  KeyValue,
  PageHeader,
  Panel,
  PanelHeader,
  Skeleton,
} from "../components/ui";
import { ApiError } from "../lib/api";
import { trustAnchorApi } from "../lib/adminAuthority";
import { useTrustAnchor } from "../lib/queries";
import { useToast } from "../lib/toast";
import type { TrustAnchorRefusal } from "../lib/types";
import { useI18n } from "../i18n/provider";

const REFUSALS: readonly TrustAnchorRefusal[] = [
  "not_loopback",
  "already_anchored",
  "invalid_document",
  "fingerprint_mismatch",
  "install_failed",
];

/**
 * A recusa vem com um código estável ao lado da frase do servidor. A tela usa
 * o código para falar a língua do operador; um código que ela não conhece cai
 * na frase do servidor, que é melhor que uma genérica.
 */
function refusalOf(error: unknown): TrustAnchorRefusal | null {
  if (!(error instanceof ApiError) || typeof error.body !== "object" || error.body === null) {
    return null;
  }
  const reason = (error.body as { reason?: unknown }).reason;
  return REFUSALS.find((known) => known === reason) ?? null;
}

/** A impressão digital em blocos de oito, que é como se confere a olho. */
function grouped(fingerprint: string): string {
  return (fingerprint.match(/.{1,8}/g) ?? [fingerprint]).join(" ");
}

function FingerprintBlock({ fingerprint }: { fingerprint: string }) {
  const { t } = useI18n();
  const [copied, setCopied] = useState(false);

  async function copy() {
    try {
      await navigator.clipboard.writeText(fingerprint);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 2000);
    } catch {
      /* sem área de transferência: o valor continua legível e selecionável */
    }
  }

  return (
    <div className="flex flex-wrap items-start gap-2">
      <code
        data-testid="anchor-fingerprint"
        // `break-words`, não `break-all`: os grupos de oito existem para
        // conferir a olho, e quebrar no meio de um desfaz isso. Com espaços
        // entre eles, a quebra cai na fronteira dos grupos.
        className="min-w-0 flex-1 select-all break-words rounded-lg border border-line bg-ink px-3 py-2 font-mono text-[0.78rem] leading-6 tracking-tight text-fg"
      >
        {grouped(fingerprint)}
      </code>
      <Button
        size="sm"
        icon={copied ? <Check className="h-3.5 w-3.5" /> : <Copy className="h-3.5 w-3.5" />}
        onClick={() => void copy()}
      >
        {copied ? t("trustAnchor.copied") : t("trustAnchor.copy")}
      </Button>
    </div>
  );
}

export function TrustAnchor() {
  const { t } = useI18n();
  const anchor = useTrustAnchor();
  const toast = useToast();

  // Não `document`: o nome sombreia o global do DOM.
  const [chain, setChain] = useState("");
  const [fingerprint, setFingerprint] = useState("");
  const [installing, setInstalling] = useState(false);
  const [refusal, setRefusal] = useState<{ reason: TrustAnchorRefusal | null; message: string } | null>(
    null,
  );

  async function install(event: React.FormEvent) {
    event.preventDefault();
    setInstalling(true);
    setRefusal(null);
    try {
      await trustAnchorApi.install(chain, fingerprint);
      setChain("");
      setFingerprint("");
      toast.success(t("trustAnchor.installed"), t("trustAnchor.installedDetail"));
      await anchor.refetch();
    } catch (error) {
      const reason = refusalOf(error);
      setRefusal({
        reason,
        message: error instanceof Error ? error.message : t("trustAnchor.installFailed"),
      });
    } finally {
      setInstalling(false);
    }
  }

  if (anchor.isLoading) {
    return (
      <>
        <PageHeader title={t("trustAnchor.pageTitle")} subtitle={t("trustAnchor.pageSubtitle")} />
        <div className="space-y-5" aria-label={t("trustAnchor.pageLoading")}>
          <Skeleton className="h-44 w-full" />
        </div>
      </>
    );
  }

  if (anchor.isError || !anchor.data) {
    return (
      <>
        <PageHeader title={t("trustAnchor.pageTitle")} subtitle={t("trustAnchor.pageSubtitle")} />
        <ErrorState
          title={t("trustAnchor.pageFailed")}
          error={anchor.error}
          onRetry={() => void anchor.refetch()}
        />
      </>
    );
  }

  const state = anchor.data;
  const canSubmit = chain.trim().length > 0 && fingerprint.trim().length > 0 && !installing;

  return (
    <>
      <PageHeader title={t("trustAnchor.pageTitle")} subtitle={t("trustAnchor.pageSubtitle")} />
      <div className="space-y-5">
        <Panel>
          <PanelHeader
            eyebrow={t("trustAnchor.stateEyebrow")}
            title={t(state.anchored ? "trustAnchor.anchored" : "trustAnchor.unanchored")}
            subtitle={t(
              state.anchored
                ? state.is_anchor
                  ? "trustAnchor.anchoredHereDetail"
                  : "trustAnchor.anchoredElsewhereDetail"
                : "trustAnchor.unanchoredDetail",
            )}
            actions={
              <Badge
                tone={state.anchored ? "lime" : "amber"}
                data-testid="anchor-state"
              >
                <Dot tone={state.anchored ? "lime" : "amber"} />
                {t(state.anchored ? "trustAnchor.badgeAnchored" : "trustAnchor.badgeUnanchored")}
              </Badge>
            }
          />
          <div className="space-y-4 px-5 py-4">
            {state.anchored && state.fingerprint ? (
              <>
                <div className="min-w-0">
                  <div className="eyebrow mb-1.5">{t("trustAnchor.fingerprintLabel")}</div>
                  <FingerprintBlock fingerprint={state.fingerprint} />
                </div>
                <dl className="grid grid-cols-1 gap-4 sm:grid-cols-2">
                  <KeyValue label={t("trustAnchor.clusterLabel")}>
                    {state.cluster_id ?? "-"}
                  </KeyValue>
                  <KeyValue label={t("trustAnchor.rootNodeLabel")}>
                    {state.anchor_node_id ?? "-"}
                  </KeyValue>
                </dl>
                <Footnote icon={<Fingerprint className="h-3.5 w-3.5" />}>
                  {t("trustAnchor.fingerprintFootnote")}
                </Footnote>
              </>
            ) : (
              <Footnote icon={<ShieldAlert className="h-3.5 w-3.5 text-amber" />}>
                {t("trustAnchor.unanchoredFootnote", { file: state.trust_filename })}
              </Footnote>
            )}
          </div>
        </Panel>

        {!state.anchored && (
          <Panel>
            <PanelHeader
              eyebrow={t("trustAnchor.installEyebrow")}
              title={t("trustAnchor.installTitle")}
              subtitle={t("trustAnchor.installSubtitle", { file: state.trust_filename })}
            />
            {state.loopback ? (
              <form className="space-y-4 px-5 py-4" onSubmit={(event) => void install(event)}>
                <Field
                  label={t("trustAnchor.documentLabel")}
                  hint={t("trustAnchor.documentHint", { file: state.trust_filename })}
                >
                  <textarea
                    data-testid="anchor-document"
                    id="trust-anchor-chain"
                    name="trust-anchor-chain"
                    value={chain}
                    onChange={(event) => setChain(event.target.value)}
                    spellCheck={false}
                    rows={8}
                    placeholder={t("trustAnchor.documentPlaceholder")}
                    className="w-full min-w-0 resize-y rounded-lg border border-line bg-ink px-3 py-2 font-mono text-[0.75rem] leading-5 text-fg placeholder:text-faint transition-colors duration-150 hover:border-line-strong focus:border-iris focus:outline-none focus:ring-2 focus:ring-iris/25"
                  />
                </Field>
                <Field
                  label={t("trustAnchor.typedFingerprintLabel")}
                  hint={t("trustAnchor.typedFingerprintHint")}
                >
                  <Input
                    data-testid="anchor-typed-fingerprint"
                    id="trust-anchor-fingerprint"
                    name="trust-anchor-fingerprint"
                    value={fingerprint}
                    onChange={(event) => setFingerprint(event.target.value)}
                    autoComplete="off"
                    autoCorrect="off"
                    spellCheck={false}
                    placeholder={t("trustAnchor.typedFingerprintPlaceholder")}
                    className="font-mono"
                  />
                </Field>
                {refusal && (
                  <p
                    role="alert"
                    data-testid="anchor-refusal"
                    className="flex items-start gap-2 rounded-lg border border-rose/40 bg-rose/10 px-3 py-2 text-sm leading-relaxed text-rose"
                  >
                    <ShieldAlert className="mt-0.5 h-4 w-4 shrink-0" aria-hidden="true" />
                    <span className="min-w-0">
                      {refusal.reason ? t(`trustAnchor.refusal.${refusal.reason}`) : refusal.message}
                    </span>
                  </p>
                )}
                <div className="flex flex-wrap items-center gap-3">
                  <Button
                    type="submit"
                    variant="primary"
                    loading={installing}
                    disabled={!canSubmit}
                    icon={<ShieldCheck className="h-4 w-4" />}
                  >
                    {t("trustAnchor.installAction")}
                  </Button>
                  <span className="text-xs text-faint">{t("trustAnchor.installCaution")}</span>
                </div>
              </form>
            ) : (
              <div className="px-5 py-4">
                <Footnote icon={<Laptop className="h-3.5 w-3.5 text-amber" />}>
                  {t("trustAnchor.remoteOnly")}
                </Footnote>
              </div>
            )}
          </Panel>
        )}
      </div>
    </>
  );
}
