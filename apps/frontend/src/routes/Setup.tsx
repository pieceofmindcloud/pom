/**
 * First run: the screen a node shows when it has no `pom.json` yet.
 *
 * The node serves this from the same bundle as the rest of the panel, so there
 * is one interface to maintain rather than a separate first-run page that drifts.
 * What tells them apart is the router the node is running: in setup mode only
 * `/api/setup/*` exists, which is also the reason this screen cannot show
 * anything about the cluster — there isn't one yet.
 *
 * The two roles differ in exactly one thing, and the copy says so plainly: the
 * Principal *mints* the connection key, a Nó is *given* it. Everything else on
 * both paths is the same node.
 */
import { useEffect, useState } from "react";
import { AnimatePresence, motion } from "framer-motion";
import { ArrowRight, Check, Copy, KeyRound, Landmark, Network, Server } from "lucide-react";
import { api } from "../lib/api";
import { LogoFull } from "../components/Logo";
import { Button, Field, Footnote, Input, Panel, PanelHeader, Skeleton } from "../components/ui";
import { useToast } from "../lib/toast";
import { LanguageSelect, useI18n } from "../i18n/provider";

type Role = "principal" | "node";

interface SetupKey {
  connection_key: string;
  addresses: string[];
}

export function Setup() {
  const { t } = useI18n();
  const [role, setRole] = useState<Role | null>(null);
  const [key, setKey] = useState<SetupKey | null>(null);
  const [keyError, setKeyError] = useState<string | null>(null);
  const [bootstrap, setBootstrap] = useState("");
  const [pastedKey, setPastedKey] = useState("");
  const [saving, setSaving] = useState(false);
  const [saved, setSaved] = useState(false);
  const [copied, setCopied] = useState(false);
  const toast = useToast();

  // Minting the key is what choosing "Principal" means, so it happens on the
  // choice rather than behind a second button.
  useEffect(() => {
    if (role !== "principal" || key) return;
    let cancelled = false;
    api
      .get<SetupKey>("/api/setup/key")
      .then((k) => !cancelled && setKey(k))
      .catch((e) => !cancelled && setKeyError(e instanceof Error ? e.message : String(e)));
    return () => {
      cancelled = true;
    };
  }, [role, key]);

  async function save() {
    setSaving(true);
    try {
      await api.post("/api/setup/save", {
        role: role === "principal" ? "principal" : "node",
        connection_key: role === "node" ? pastedKey.trim() : "",
        bootstrap: role === "node" ? bootstrap.trim() : "",
      });
      // The node writes the config, respawns itself and exits, so there is no
      // second response to wait for — the panel comes back on its own.
      setSaved(true);
      setTimeout(() => window.location.reload(), 4000);
    } catch (e) {
      toast.fromError(t("setup.saveFailed"), e);
      setSaving(false);
    }
  }

  const canSave =
    role === "principal" ||
    (role === "node" && pastedKey.trim().length > 0 && bootstrap.trim().length > 0);

  return (
    <div className="min-h-full bg-ink px-4 py-10 sm:px-6">
      <div className="mx-auto w-full max-w-2xl">
        <div className="flex justify-end">
          <LanguageSelect />
        </div>
        <motion.div
          initial={{ opacity: 0, y: 12 }}
          animate={{ opacity: 1, y: 0 }}
          transition={{ duration: 0.35, ease: [0.22, 1, 0.36, 1] }}
          className="mb-8 flex flex-col items-center text-center"
        >
          <LogoFull width={240} />
          <h1 className="mt-5 font-display text-2xl font-bold tracking-tight text-fg">
            {t("setup.title")}
          </h1>
          <p className="mt-2 max-w-md text-sm leading-relaxed text-muted">
            {t("setup.subtitle")}
          </p>
        </motion.div>

        <AnimatePresence mode="wait">
          {saved ? (
            <motion.div
              key="saved"
              initial={{ opacity: 0, scale: 0.98 }}
              animate={{ opacity: 1, scale: 1 }}
              transition={{ duration: 0.3 }}
            >
              <Panel>
                <div className="flex flex-col items-center px-6 py-12 text-center">
                  <div className="mb-4 flex h-12 w-12 items-center justify-center rounded-xl border border-aqua/35 bg-aqua/12 text-aqua">
                    <Check className="h-6 w-6" />
                  </div>
                  <p className="font-display text-base font-semibold text-fg">
                    {t("setup.saved")}
                  </p>
                  <p className="mt-2 max-w-sm text-sm leading-relaxed text-muted">
                    {t("setup.savedDetail")}
                  </p>
                  <div className="mt-5 w-40">
                    <Skeleton className="h-1.5 w-full" />
                  </div>
                </div>
              </Panel>
            </motion.div>
          ) : (
            <motion.div key="form" className="space-y-5">
              <div className="grid gap-3 sm:grid-cols-2">
                <RoleCard
                  icon={<Landmark className="h-5 w-5" />}
                  title={t("setup.principal")}
                  detail={t("setup.principalDetail")}
                  selected={role === "principal"}
                  onSelect={() => setRole("principal")}
                />
                <RoleCard
                  icon={<Server className="h-5 w-5" />}
                  title={t("setup.node")}
                  detail={t("setup.nodeDetail")}
                  selected={role === "node"}
                  onSelect={() => setRole("node")}
                />
              </div>

              <AnimatePresence mode="wait">
                {role === "principal" && (
                  <Reveal key="principal">
                    <Panel>
                      <PanelHeader
                        eyebrow={t("setup.keepBeforeContinue")}
                        title={t("setup.connectionKey")}
                        subtitle={t("setup.connectionKeyDetail")}
                      />
                      <div className="space-y-4 px-5 py-4">
                        {keyError ? (
                          <p className="text-sm text-rose">
                            {t("setup.keyFailed", { error: keyError })}
                          </p>
                        ) : !key ? (
                          <Skeleton className="h-9 w-full" />
                        ) : (
                          <>
                            <div className="flex items-center gap-2">
                              <code className="break-path min-w-0 flex-1 rounded-lg border border-line bg-ink px-3 py-2 font-mono text-[0.78rem] text-fg">
                                {key.connection_key}
                              </code>
                              <Button
                                size="sm"
                                variant="outline"
                                icon={
                                  copied ? (
                                    <Check className="h-3.5 w-3.5 text-aqua" />
                                  ) : (
                                    <Copy className="h-3.5 w-3.5" />
                                  )
                                }
                                onClick={() => {
                                  void navigator.clipboard.writeText(key.connection_key);
                                  setCopied(true);
                                  setTimeout(() => setCopied(false), 1800);
                                }}
                              >
                                {copied ? t("setup.copied") : t("setup.copy")}
                              </Button>
                            </div>

                            {key.addresses.length > 0 && (
                              <div>
                                <div className="eyebrow mb-2">
                                  {t("setup.addresses")}
                                </div>
                                <ul className="space-y-1">
                                  {key.addresses.map((a) => (
                                    <li
                                      key={a}
                                      className="break-path rounded-md border border-line bg-raised px-2.5 py-1.5 font-mono text-[0.76rem] text-muted"
                                    >
                                      {a}
                                    </li>
                                  ))}
                                </ul>
                              </div>
                            )}
                          </>
                        )}
                        <Footnote icon={<KeyRound className="h-3.5 w-3.5" />}>
                          {t("setup.keyNote")}
                        </Footnote>
                      </div>
                    </Panel>
                  </Reveal>
                )}

                {role === "node" && (
                  <Reveal key="node">
                    <Panel>
                      <PanelHeader
                        title={t("setup.wherePrincipal")}
                        subtitle={t("setup.wherePrincipalDetail")}
                      />
                      <div className="space-y-4 px-5 py-4">
                        <Field
                          label={t("setup.principalAddress")}
                          hint={t("setup.principalAddressHint")}
                        >
                          <Input
                            value={bootstrap}
                            spellCheck={false}
                            placeholder="192.168.1.5:42777"
                            onChange={(e) => setBootstrap(e.target.value)}
                          />
                        </Field>
                        <Field
                          label={t("setup.connectionKey")}
                          hint={t("setup.connectionKeyHint")}
                        >
                          <Input
                            type="password"
                            value={pastedKey}
                            spellCheck={false}
                            placeholder={t("setup.pasteKey")}
                            onChange={(e) => setPastedKey(e.target.value)}
                          />
                        </Field>
                      </div>
                    </Panel>
                  </Reveal>
                )}
              </AnimatePresence>

              {role && (
                <Reveal key="action">
                  <div className="flex justify-end">
                    <Button
                      variant="primary"
                      loading={saving}
                      disabled={!canSave}
                      icon={<ArrowRight className="h-4 w-4" />}
                      onClick={() => void save()}
                    >
                      {t("setup.saveStart")}
                    </Button>
                  </div>
                </Reveal>
              )}
            </motion.div>
          )}
        </AnimatePresence>

        <Footnote icon={<Network className="h-3.5 w-3.5" />} className="mt-8 justify-center">
          {t("setup.restartNote")}
        </Footnote>
      </div>
    </div>
  );
}

function Reveal({ children }: { children: React.ReactNode }) {
  return (
    <motion.div
      initial={{ opacity: 0, y: -6, height: 0 }}
      animate={{ opacity: 1, y: 0, height: "auto" }}
      exit={{ opacity: 0, y: -6, height: 0 }}
      transition={{ duration: 0.25, ease: [0.22, 1, 0.36, 1] }}
      className="overflow-hidden"
    >
      {children}
    </motion.div>
  );
}

function RoleCard({
  icon,
  title,
  detail,
  selected,
  onSelect,
}: {
  icon: React.ReactNode;
  title: string;
  detail: string;
  selected: boolean;
  onSelect: () => void;
}) {
  return (
    <button
      onClick={onSelect}
      aria-pressed={selected}
      className={[
        "panel flex flex-col items-start gap-2 px-4 py-4 text-left",
        "transition-[border-color,background-color,transform] duration-200 ease-swift",
        "hover:border-line-strong active:scale-[0.99]",
        selected ? "border-iris/50 bg-iris/[0.07]" : "",
      ].join(" ")}
    >
      <span
        className={[
          "flex h-9 w-9 items-center justify-center rounded-lg border",
          selected ? "border-iris/40 bg-iris/12 text-iris" : "border-line bg-raised text-muted",
        ].join(" ")}
      >
        {icon}
      </span>
      <span className="font-display text-[0.95rem] font-semibold text-fg">{title}</span>
      <span className="text-xs leading-relaxed text-muted">{detail}</span>
    </button>
  );
}
