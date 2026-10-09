/**
 * This machine's root of trust, and the two gestures that involve it.
 *
 * **Why this screen does not sit behind the administrator profile.** Before
 * anchoring, a node with `profile: admin` has no effective admin capability: the
 * gateway refuses the `AdminOnly` scope in exactly that state. An anchoring
 * screen locked behind the profile would be unreachable at the only moment it
 * is good for anything. That is why it is free for the three profiles, and the
 * route that feeds it is `CommonOrInternal` - whoever receives the anchor is
 * usually a `node` or an `infer`.
 *
 * **What the screen decides: nothing.** Showing the computed fingerprint is safe
 * because it is public and grants nothing. Installing is decided on the server,
 * which requires real loopback, a node still without an anchor and the fingerprint
 * checked. `can_install` here is only what avoids drawing a form that is going
 * to be refused; it is not the lock.
 */
import { Check, Copy, Download, Fingerprint, Laptop, RefreshCw, ShieldAlert, ShieldCheck } from "lucide-react";
import { useEffect, useState } from "react";
import { copyText } from "../lib/clipboard";
import {
  Badge,
  Button,
  Dot,
  ErrorState,
  Field,
  Footnote,
  KeyValue,
  PageHeader,
  Panel,
  PanelHeader,
  Skeleton,
} from "../components/ui";
import { ApiError, api } from "../lib/api";
import { trustAnchorApi } from "../lib/adminAuthority";
import { useNode, useTrustAnchor } from "../lib/queries";
import { useProfile } from "../lib/profile";
import { Confirm } from "../components/Confirm";
import { useToast } from "../lib/toast";
import type { TrustAnchorRefusal, TrustShareView } from "../lib/types";
import { useI18n } from "../i18n/provider";
import { fingerprintFromTrustDocument, groupFingerprint } from "../lib/trustFingerprint";

const REFUSALS: readonly TrustAnchorRefusal[] = [
  "not_loopback",
  "already_anchored",
  "invalid_document",
  "fingerprint_mismatch",
  "install_failed",
];

/**
 * The refusal comes with a stable code next to the server's sentence. The screen
 * uses the code to speak the operator's language; a code it does not know falls
 * back to the server's sentence, which is better than a generic one.
 */
function refusalOf(error: unknown): TrustAnchorRefusal | null {
  if (!(error instanceof ApiError) || typeof error.body !== "object" || error.body === null) {
    return null;
  }
  const reason = (error.body as { reason?: unknown }).reason;
  return REFUSALS.find((known) => known === reason) ?? null;
}

function FingerprintBlock({ fingerprint }: { fingerprint: string }) {
  const { t } = useI18n();
  const [copied, setCopied] = useState(false);

  async function copy() {
    // Without a clipboard the value stays readable and selectable.
    if (!(await copyText(fingerprint))) return;
    setCopied(true);
    window.setTimeout(() => setCopied(false), 2000);
  }

  return (
    <div className="flex flex-wrap items-start gap-2">
      <code
        data-testid="anchor-fingerprint"
        // `break-words`, not `break-all`: the groups of eight exist to be checked
        // by eye, and breaking in the middle of one undoes that. With spaces
        // between them, the break falls on the group boundary.
        className="min-w-0 flex-1 select-all break-words rounded-lg border border-line bg-ink px-3 py-2 font-mono text-[0.78rem] leading-6 tracking-tight text-fg"
      >
        {groupFingerprint(fingerprint)}
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

export function TrustAnchor({ sharing = false }: { sharing?: boolean }) {
  const { t } = useI18n();
  const anchor = useTrustAnchor();
  const node = useNode();
  const toast = useToast();
  const canShare = node.data?.authority_level === "primary";
  // The cluster's key is the operator's credential; node and infer never see it.
  const { profile } = useProfile();
  const [trustShare, setTrustShare] = useState<TrustShareView | null>(null);
  const [trustShareLoaded, setTrustShareLoaded] = useState(false);
  const [trustCopied, setTrustCopied] = useState<"document" | "key" | null>(null);

  // Not `document`: the name shadows the DOM global.
  const [chain, setChain] = useState("");
  const [fingerprint, setFingerprint] = useState("");
  const [fingerprintPending, setFingerprintPending] = useState(false);
  const [fingerprintConfirmed, setFingerprintConfirmed] = useState(false);
  const [installing, setInstalling] = useState(false);
  const [refusal, setRefusal] = useState<{ reason: TrustAnchorRefusal | null; message: string } | null>(
    null,
  );

  useEffect(() => {
    if (!canShare || trustShareLoaded) return;
    setTrustShareLoaded(true);
    void api.get<TrustShareView>("/api/ui/network/trust-share")
      .then(setTrustShare)
      .catch(() => undefined);
  }, [canShare, trustShareLoaded]);

  useEffect(() => {
    let cancelled = false;
    if (!chain.trim()) {
      setFingerprint("");
      setFingerprintConfirmed(false);
      setFingerprintPending(false);
      return;
    }
    setFingerprintPending(true);
    setFingerprintConfirmed(false);
    void fingerprintFromTrustDocument(chain).then((value) => {
      if (cancelled) return;
      setFingerprint(value ?? "");
      setFingerprintPending(false);
    });
    return () => {
      cancelled = true;
    };
  }, [chain]);

  async function install(event: React.FormEvent) {
    event.preventDefault();
    setInstalling(true);
    setRefusal(null);
    try {
      await trustAnchorApi.install(chain, fingerprint);
      setChain("");
      setFingerprint("");
      setFingerprintConfirmed(false);
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
        <PageHeader title={t(sharing ? "network.sharingTitle" : "trustAnchor.pageTitle")} subtitle={t(sharing ? "network.sharingSubtitle" : "trustAnchor.pageSubtitle")} />
        <div className="space-y-5" aria-label={t("trustAnchor.pageLoading")}>
          <Skeleton className="h-44 w-full" />
        </div>
      </>
    );
  }

  if (anchor.isError || !anchor.data) {
    return (
      <>
        <PageHeader title={t(sharing ? "network.sharingTitle" : "trustAnchor.pageTitle")} subtitle={t(sharing ? "network.sharingSubtitle" : "trustAnchor.pageSubtitle")} />
        <ErrorState
          title={t("trustAnchor.pageFailed")}
          error={anchor.error}
          onRetry={() => void anchor.refetch()}
        />
      </>
    );
  }

  const state = anchor.data;
  const canSubmit = chain.trim().length > 0 && fingerprint.trim().length > 0 && fingerprintConfirmed && !fingerprintPending && !installing;

  async function copySharedValue(kind: "document" | "key", value: string) {
    // The value stays selectable when the copy fails.
    if (!(await copyText(value))) return;
    setTrustCopied(kind);
    window.setTimeout(() => setTrustCopied(null), 1800);
  }

  function downloadSharedValue(value: string, filename: string) {
    const url = URL.createObjectURL(new Blob([value], { type: "application/json;charset=utf-8" }));
    const link = document.createElement("a");
    link.href = url;
    link.download = filename;
    link.click();
    URL.revokeObjectURL(url);
  }

  return (
    <>
      <PageHeader title={t(sharing ? "network.sharingTitle" : "trustAnchor.pageTitle")} subtitle={t(sharing ? "network.sharingSubtitle" : "trustAnchor.pageSubtitle")} />
      <div className="space-y-5">
        <section id="machine" className="scroll-mt-6">
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
        </section>

        {canShare && <section id="share" className="scroll-mt-6"><TrustSharePanel share={trustShare} copied={trustCopied} onCopy={copySharedValue} onDownload={downloadSharedValue} t={t} /></section>}

        {profile === "admin" && <section id="keys" className="scroll-mt-6">
          <ConnectionKeyPanel node={node.data} canRotate={canShare} t={t} />
        </section>}

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
                <div className="rounded-lg border border-line bg-raised px-3 py-3">
                  <p className="text-xs font-medium uppercase tracking-[0.14em] text-faint">{t("trustAnchor.fingerprintCalculated")}</p>
                  {fingerprintPending ? <p className="mt-2 text-sm text-muted">{t("trustAnchor.fingerprintWaiting")}</p> : fingerprint ? <code data-testid="anchor-calculated-fingerprint" className="mt-2 block break-words font-mono text-sm leading-6 text-fg">{groupFingerprint(fingerprint)}</code> : <p className="mt-2 text-sm text-muted">{t("trustAnchor.fingerprintWaiting")}</p>}
                  <label className="mt-3 flex items-start gap-3 text-sm text-fg">
                    <input data-testid="anchor-fingerprint-confirm" type="checkbox" checked={fingerprintConfirmed} disabled={!fingerprint || fingerprintPending} onChange={(event) => setFingerprintConfirmed(event.target.checked)} className="mt-0.5 h-4 w-4 accent-iris" />
                    <span>{t("trustAnchor.fingerprintConfirm")}</span>
                  </label>
                </div>
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

function TrustSharePanel({ share, copied, onCopy, onDownload, t }: { share: TrustShareView | null; copied: "document" | "key" | null; onCopy: (kind: "document" | "key", value: string) => Promise<void>; onDownload: (value: string, filename: string) => void; t: (key: string, values?: Record<string, string | number>) => string }) {
  return <Panel><PanelHeader eyebrow={t("network.shareEyebrow")} title={t("network.shareTitle")} subtitle={t("network.shareSubtitle")} />{!share ? <p className="px-5 py-5 text-sm text-muted">{t("network.shareLoading")}</p> : <div className="space-y-4 px-5 py-5"><ShareBlock label={t("network.trustDocument")} value={share.document} filename={share.filename} kind="document" copied={copied === "document"} onCopy={onCopy} onDownload={onDownload} t={t} />{share.connection_key ? <ShareBlock label={t("network.connectionKey")} value={share.connection_key} filename="pom-connection-key.txt" kind="key" copied={copied === "key"} onCopy={onCopy} onDownload={onDownload} t={t} /> : <p className="text-xs leading-relaxed text-faint">{t("network.publicNoConnectionKey")}</p>}</div>}</Panel>;
}

function ShareBlock({ label, value, filename, kind, copied, onCopy, onDownload, t }: { label: string; value: string; filename: string; kind: "document" | "key"; copied: boolean; onCopy: (kind: "document" | "key", value: string) => Promise<void>; onDownload: (value: string, filename: string) => void; t: (key: string, values?: Record<string, string | number>) => string }) {
  return <div className="min-w-0 rounded-xl border border-line bg-raised"><div className="flex flex-wrap items-center justify-between gap-2 border-b border-line px-4 py-3"><span className="text-sm font-medium text-fg">{label}</span><div className="flex flex-wrap gap-2"><Button size="sm" icon={copied ? <Check className="h-3.5 w-3.5" /> : <Copy className="h-3.5 w-3.5" />} onClick={() => void onCopy(kind, value)}>{copied ? t("network.copied") : t("network.copy")}</Button><Button size="sm" icon={<Download className="h-3.5 w-3.5" />} onClick={() => onDownload(value, filename)}>{t("network.download")}</Button></div></div><pre className="max-h-56 overflow-auto whitespace-pre-wrap break-words px-4 py-3 font-mono text-[0.7rem] leading-5 text-muted">{value}</pre></div>;
}

function ConnectionKeyPanel({ node, canRotate, t }: { node: ReturnType<typeof useNode>["data"]; canRotate: boolean; t: (key: string, values?: Record<string, string | number>) => string }) {
  const toast = useToast();
  const [confirming, setConfirming] = useState(false);
  const [busy, setBusy] = useState(false);
  async function regenerate() {
    setBusy(true);
    try {
      await api.post("/admin/regenerate-key");
      toast.success(t("keys.rotated"), t("keys.rotatedDetail"));
    } catch (error) {
      toast.fromError(t("keys.rotateFailed"), error);
    } finally {
      setBusy(false);
      setConfirming(false);
    }
  }
  if (!node) return <Skeleton className="h-48 w-full" />;
  return <><Panel><PanelHeader eyebrow={node.hostname} title={t("keys.clusterKey")} subtitle={t("keys.clusterKeySubtitle")} actions={canRotate ? <Button variant="danger" icon={<RefreshCw className="h-3.5 w-3.5" />} onClick={() => setConfirming(true)}>{t("keys.generateAnother")}</Button> : undefined} /><div className="space-y-4 px-5 py-4"><code className="block break-all rounded-lg border border-line bg-ink px-3 py-2 font-mono text-[0.78rem] text-fg">{node.connection_key}</code><dl className="grid gap-4 sm:grid-cols-2"><KeyValue label={t("keys.bootstrapAddress")}><span className="font-mono">{node.system.hostname}:{node.p2p_port}</span></KeyValue><KeyValue label={t("keys.apiBaseUrl")}><span className="font-mono">http://{node.system.hostname}:{node.gateway_port}/v1</span></KeyValue></dl></div></Panel><Confirm open={confirming} title={t("keys.rotateTitle")} description={t("keys.rotateDescription")} confirmLabel={t("keys.generateAnother")} tone="danger" busy={busy} onConfirm={() => void regenerate()} onCancel={() => setConfirming(false)} /></>;
}
