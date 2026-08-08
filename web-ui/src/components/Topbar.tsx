import clsx from "clsx";
import { AnimatePresence, motion } from "framer-motion";
import {
  ChevronDown,
  Cpu,
  LogOut,
  Menu,
  Moon,
  PowerOff,
  Sun,
  X,
} from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { api } from "../lib/api";
import { useCluster, useInvalidate, useNode } from "../lib/queries";
import { useTheme } from "../lib/theme";
import { useToast } from "../lib/toast";
import { Confirm } from "./Confirm";
import { Notifications } from "./Notifications";
import { SidebarContent } from "./Sidebar";
import { Badge, Dot, Skeleton } from "./ui";

/** One line saying what this node is and whether the pipeline is closed. */
function NodeChip() {
  const node = useNode();
  const cluster = useCluster();

  if (node.isLoading) return <Skeleton className="h-8 w-56" />;
  if (node.isError)
    return (
      <Badge tone="rose">
        <Dot tone="rose" /> Sem contato com o nó
      </Badge>
    );

  const n = node.data!;
  const c = cluster.data;
  const serving = n.status.state === "running";
  const complete = c?.complete ?? false;

  return (
    <div className="flex min-w-0 items-center gap-2">
      <Badge tone={serving ? "aqua" : "neutral"}>
        <Dot tone={serving ? "aqua" : "neutral"} pulse={serving} />
        <span className="truncate">{n.hostname}</span>
      </Badge>
      <Badge mono tone="neutral" className="hidden sm:inline-flex">
        {n.short_id}
      </Badge>
      {c && c.peers.length > 0 && (
        <Badge tone={complete ? "aqua" : "amber"} className="hidden md:inline-flex">
          {c.peers.length} {c.peers.length === 1 ? "nó" : "nós"}
          {c.total_layers > 0 && (
            <span className="font-mono">
              · {c.covered_layers}/{c.total_layers} camadas
            </span>
          )}
        </Badge>
      )}
    </div>
  );
}

type PendingAction = "unload" | "leave" | null;

function NodeActions() {
  const [open, setOpen] = useState(false);
  const [pending, setPending] = useState<PendingAction>(null);
  const [busy, setBusy] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  const node = useNode();
  const toast = useToast();
  const invalidate = useInvalidate();

  useEffect(() => {
    if (!open) return;
    const onClick = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", onClick);
    return () => document.removeEventListener("mousedown", onClick);
  }, [open]);

  const loaded = node.data?.config != null || node.data?.status.state === "running";

  async function run(action: Exclude<PendingAction, null>) {
    setBusy(true);
    try {
      if (action === "unload") {
        await api.post("/api/ui/unload");
        toast.success("Modelo descarregado", "A memória deste nó foi liberada.");
      } else {
        await api.post("/api/ui/leave");
        toast.success(
          "Nó fora da rede",
          "Parou de anunciar e esqueceu os peers. Reconecte pela aba Rede.",
        );
      }
      invalidate();
    } catch (e) {
      toast.fromError(
        action === "unload" ? "O modelo continua carregado" : "O nó continua na rede",
        e,
      );
    } finally {
      setBusy(false);
      setPending(null);
    }
  }

  return (
    <div className="relative" ref={ref}>
      <button
        onClick={() => setOpen((o) => !o)}
        aria-expanded={open}
        className={clsx(
          "flex h-9 items-center gap-1.5 rounded-lg border px-2.5 text-[0.82rem] transition-colors duration-150",
          open
            ? "border-iris/40 bg-iris/12 text-iris"
            : "border-line bg-raised text-muted hover:border-line-strong hover:text-fg",
        )}
      >
        <span className="hidden sm:inline">Este nó</span>
        <ChevronDown
          className={clsx("h-3.5 w-3.5 transition-transform duration-200", open && "rotate-180")}
        />
      </button>

      <AnimatePresence>
        {open && (
          <motion.div
            initial={{ opacity: 0, y: -6, scale: 0.98 }}
            animate={{ opacity: 1, y: 0, scale: 1 }}
            exit={{ opacity: 0, y: -6, scale: 0.98 }}
            transition={{ duration: 0.16, ease: [0.22, 1, 0.36, 1] }}
            className="absolute right-0 top-11 z-50 w-72 origin-top-right overflow-hidden rounded-xl border border-line bg-surface p-1.5 shadow-lift"
          >
            <button
              disabled={!loaded}
              onClick={() => {
                setOpen(false);
                setPending("unload");
              }}
              className="flex w-full items-start gap-3 rounded-lg px-2.5 py-2 text-left transition-colors hover:bg-line/50 disabled:pointer-events-none disabled:opacity-45"
            >
              <PowerOff className="mt-0.5 h-4 w-4 shrink-0 text-amber" />
              <span className="min-w-0">
                <span className="block text-[0.84rem] font-medium text-fg">
                  Descarregar modelo
                </span>
                <span className="block text-xs leading-relaxed text-muted">
                  {loaded
                    ? "Para o servidor de inferência e libera a memória."
                    : "Nenhum modelo carregado neste nó."}
                </span>
              </span>
            </button>
            <button
              onClick={() => {
                setOpen(false);
                setPending("leave");
              }}
              className="flex w-full items-start gap-3 rounded-lg px-2.5 py-2 text-left transition-colors hover:bg-line/50"
            >
              <LogOut className="mt-0.5 h-4 w-4 shrink-0 text-rose" />
              <span className="min-w-0">
                <span className="block text-[0.84rem] font-medium text-fg">
                  Sair da rede
                </span>
                <span className="block text-xs leading-relaxed text-muted">
                  Descarrega o modelo, para de anunciar e esquece os peers.
                </span>
              </span>
            </button>
          </motion.div>
        )}
      </AnimatePresence>

      <Confirm
        open={pending === "unload"}
        title="Descarregar o modelo deste nó?"
        description={
          <>
            O servidor de inferência para e a memória é liberada. Enquanto isso, a rede
            fica sem as camadas{" "}
            <span className="font-mono text-fg">
              {node.data?.config
                ? `${node.data.config.start_layer}–${node.data.config.end_layer}`
                : "atribuídas a este nó"}
            </span>
            , e nenhuma requisição completa.
          </>
        }
        confirmLabel="Descarregar"
        tone="danger"
        busy={busy}
        onConfirm={() => run("unload")}
        onCancel={() => setPending(null)}
      />
      <Confirm
        open={pending === "leave"}
        title="Tirar este nó da rede?"
        description="O nó para de anunciar, esquece os peers que conhece e descarrega o modelo. A interface continua no ar; para voltar, use Configurações › Rede."
        confirmLabel="Sair da rede"
        tone="danger"
        busy={busy}
        onConfirm={() => run("leave")}
        onCancel={() => setPending(null)}
      />
    </div>
  );
}

function ThemeToggle() {
  const { theme, toggle } = useTheme();
  return (
    <button
      onClick={toggle}
      aria-label={theme === "dark" ? "Usar tema claro" : "Usar tema escuro"}
      className="flex h-9 w-9 items-center justify-center rounded-lg border border-line bg-raised text-muted transition-colors duration-150 hover:border-line-strong hover:text-fg"
    >
      <AnimatePresence mode="wait" initial={false}>
        <motion.span
          key={theme}
          initial={{ opacity: 0, rotate: -70, scale: 0.7 }}
          animate={{ opacity: 1, rotate: 0, scale: 1 }}
          exit={{ opacity: 0, rotate: 70, scale: 0.7 }}
          transition={{ duration: 0.18, ease: [0.22, 1, 0.36, 1] }}
        >
          {theme === "dark" ? <Moon className="h-4 w-4" /> : <Sun className="h-4 w-4" />}
        </motion.span>
      </AnimatePresence>
    </button>
  );
}

export function Topbar() {
  const [drawer, setDrawer] = useState(false);

  return (
    <>
      <header className="sticky top-0 z-40 flex h-16 shrink-0 items-center gap-3 border-b border-line bg-surface/85 px-4 backdrop-blur-md sm:px-6">
        <button
          onClick={() => setDrawer(true)}
          aria-label="Abrir menu"
          className="flex h-9 w-9 items-center justify-center rounded-lg border border-line bg-raised text-muted transition-colors hover:text-fg lg:hidden"
        >
          <Menu className="h-4 w-4" />
        </button>

        <div className="min-w-0 flex-1 overflow-hidden">
          <NodeChip />
        </div>

        <div className="flex shrink-0 items-center gap-2">
          <a
            href="/v1/models"
            target="_blank"
            rel="noreferrer"
            className="hidden h-9 items-center gap-1.5 rounded-lg border border-line bg-raised px-2.5 text-[0.82rem] text-muted transition-colors hover:border-line-strong hover:text-fg xl:flex"
            title="API compatível com OpenAI servida por este nó"
          >
            <Cpu className="h-3.5 w-3.5" />
            <span className="font-mono">/v1</span>
          </a>
          <Notifications />
          <NodeActions />
          <ThemeToggle />
        </div>
      </header>

      {/* Menu on narrow screens. The sidebar is the same component. */}
      <AnimatePresence>
        {drawer && (
          <motion.div
            className="fixed inset-0 z-50 lg:hidden"
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            exit={{ opacity: 0 }}
            transition={{ duration: 0.15 }}
          >
            <div
              className="absolute inset-0 bg-black/55 backdrop-blur-[2px]"
              onClick={() => setDrawer(false)}
            />
            <motion.div
              initial={{ x: -280 }}
              animate={{ x: 0 }}
              exit={{ x: -280 }}
              transition={{ duration: 0.24, ease: [0.22, 1, 0.36, 1] }}
              className="relative flex h-full w-[268px] flex-col border-r border-line bg-surface"
            >
              <button
                onClick={() => setDrawer(false)}
                aria-label="Fechar menu"
                className="absolute right-3 top-5 flex h-8 w-8 items-center justify-center rounded-lg text-faint transition-colors hover:text-fg"
              >
                <X className="h-4 w-4" />
              </button>
              <SidebarContent onNavigate={() => setDrawer(false)} />
            </motion.div>
          </motion.div>
        )}
      </AnimatePresence>
    </>
  );
}
