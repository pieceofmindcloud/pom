/**
 * First run: the screen a node shows when it has no `rdma-p2p.json` yet.
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
import { Logo } from "../components/Logo";
import { Button, Field, Footnote, Input, Panel, PanelHeader, Skeleton } from "../components/ui";
import { useToast } from "../lib/toast";

type Role = "principal" | "node";

interface SetupKey {
  connection_key: string;
  addresses: string[];
}

export function Setup() {
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
      toast.fromError("A configuração não foi salva", e);
      setSaving(false);
    }
  }

  const canSave =
    role === "principal" ||
    (role === "node" && pastedKey.trim().length > 0 && bootstrap.trim().length > 0);

  return (
    <div className="min-h-full bg-ink px-4 py-10 sm:px-6">
      <div className="mx-auto w-full max-w-2xl">
        <motion.div
          initial={{ opacity: 0, y: 12 }}
          animate={{ opacity: 1, y: 0 }}
          transition={{ duration: 0.35, ease: [0.22, 1, 0.36, 1] }}
          className="mb-8 flex flex-col items-center text-center"
        >
          <Logo size={48} />
          <h1 className="mt-5 font-display text-2xl font-bold tracking-tight text-fg">
            Configuração inicial
          </h1>
          <p className="mt-2 max-w-md text-sm leading-relaxed text-muted">
            Esta máquina ainda não faz parte de nenhuma rede. Escolha o papel dela
            para começar.
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
                    Configuração salva
                  </p>
                  <p className="mt-2 max-w-sm text-sm leading-relaxed text-muted">
                    O nó está reiniciando para valer a configuração nova. Esta tela
                    volta sozinha para o painel em alguns segundos.
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
                  title="Principal"
                  detail="Cria a chave do cluster e é o endereço que os outros procuram."
                  selected={role === "principal"}
                  onSelect={() => setRole("principal")}
                />
                <RoleCard
                  icon={<Server className="h-5 w-5" />}
                  title="Nó da rede"
                  detail="Entra numa rede que já existe, com a chave que o principal gerou."
                  selected={role === "node"}
                  onSelect={() => setRole("node")}
                />
              </div>

              <AnimatePresence mode="wait">
                {role === "principal" && (
                  <Reveal key="principal">
                    <Panel>
                      <PanelHeader
                        eyebrow="Guarde antes de seguir"
                        title="Chave de conexão do cluster"
                        subtitle="É a mesma string em todas as máquinas: credencial de operador e chave da API. Cada nó novo entra com ela."
                      />
                      <div className="space-y-4 px-5 py-4">
                        {keyError ? (
                          <p className="text-sm text-rose">
                            Não deu para gerar a chave: {keyError}
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
                                {copied ? "Copiada" : "Copiar"}
                              </Button>
                            </div>

                            {key.addresses.length > 0 && (
                              <div>
                                <div className="eyebrow mb-2">
                                  Endereços que os outros nós devem usar
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
                          A chave pode ser trocada depois em Configurações › Chaves de
                          conexão, mas trocar numa máquina só derruba as outras até
                          serem atualizadas.
                        </Footnote>
                      </div>
                    </Panel>
                  </Reveal>
                )}

                {role === "node" && (
                  <Reveal key="node">
                    <Panel>
                      <PanelHeader
                        title="Onde fica o principal"
                        subtitle="Peça os dois valores a quem configurou a primeira máquina."
                      />
                      <div className="space-y-4 px-5 py-4">
                        <Field
                          label="Endereço do principal"
                          hint="Host e porta P2P, como aparece na tela dele. Ex.: 192.168.1.5:42777"
                        >
                          <Input
                            value={bootstrap}
                            spellCheck={false}
                            placeholder="192.168.1.5:42777"
                            onChange={(e) => setBootstrap(e.target.value)}
                          />
                        </Field>
                        <Field
                          label="Chave de conexão"
                          hint="A mesma string gerada na máquina principal."
                        >
                          <Input
                            type="password"
                            value={pastedKey}
                            spellCheck={false}
                            placeholder="Cole a chave aqui"
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
                      Salvar e iniciar
                    </Button>
                  </div>
                </Reveal>
              )}
            </motion.div>
          )}
        </AnimatePresence>

        <Footnote icon={<Network className="h-3.5 w-3.5" />} className="mt-8 justify-center">
          O nó reinicia sozinho depois de salvar. Nada é enviado para fora desta
          rede.
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
