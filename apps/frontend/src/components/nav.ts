import {
  BookOpen,
  Boxes,
  FolderTree,
  Gauge,
  History,
  KeyRound,
  Layers,
  ListTree,
  MessagesSquare,
  Network,
  Rocket,
  Scissors,
  Search,
  Send,
  Settings,
  SlidersHorizontal,
  Sparkles,
  Terminal,
  Upload,
  type LucideIcon,
} from "lucide-react";
import { canAccessRoute } from "../lib/access";
import type { Profile } from "../lib/profile";

export interface NavLeaf {
  labelKey: string;
  to: string;
  icon?: LucideIcon;
  /** Matches child routes too — used by the sidebar to mark a section open. */
  end?: boolean;
}

export interface NavSection {
  labelKey: string;
  icon: LucideIcon;
  /** A section either navigates somewhere itself, or holds children. */
  to?: string;
  children?: NavLeaf[];
}

/**
 * The menu, in the order the operator works through it: watch the cluster, talk
 * to it, then feed it models, move files, read what happened, and configure.
 */
export const NAV: NavSection[] = [
  { labelKey: "nav.dashboard", icon: Gauge, to: "/" },
  { labelKey: "nav.chat", icon: MessagesSquare, to: "/chat" },
  {
    labelKey: "nav.models",
    icon: Boxes,
    children: [
      { labelKey: "nav.remoteModels", to: "/modelos/remotos", icon: Search },
      { labelKey: "nav.fullModels", to: "/modelos", icon: Boxes, end: true },
      { labelKey: "nav.sliceModels", to: "/modelos/fatiar", icon: Scissors },
      { labelKey: "nav.shards", to: "/fatias", icon: Layers, end: true },
    ],
  },
  {
    labelKey: "nav.deployments",
    icon: Rocket,
    children: [
      { labelKey: "nav.deploy", to: "/implantar", icon: Rocket },
      // Separada de "implantar" de proposito: uma cria, a outra mostra e
      // ajusta o que ja existe. Sao dois momentos diferentes do mesmo trabalho.
      { labelKey: "nav.history", to: "/implantacoes", icon: History },
      { labelKey: "nav.deploymentLogs", to: "/implantacoes/logs", icon: Terminal },
    ],
  },
  {
    labelKey: "nav.transfers",
    icon: Send,
    children: [
      { labelKey: "nav.sendFile", to: "/transferencias/enviar", icon: Upload },
      { labelKey: "nav.inProgress", to: "/transferencias", icon: Send, end: true },
    ],
  },
  {
    labelKey: "nav.logs",
    icon: ListTree,
    to: "/logs",
  },
  // The repo's own documentation, not a screen that reads the cluster: the
  // folder AGENTS.md files and subsystem READMEs embedded at build time.
  { labelKey: "nav.documentation", icon: BookOpen, to: "/documentacao" },
  {
    labelKey: "nav.settings",
    icon: Settings,
    children: [
      { labelKey: "nav.network", to: "/config/rede", icon: Network },
      { labelKey: "nav.preferences", to: "/config/preferencias", icon: SlidersHorizontal },
      { labelKey: "nav.apiKeys", to: "/config/keys/api", icon: KeyRound },
      { labelKey: "nav.connectionKeys", to: "/config/keys/conexao", icon: SlidersHorizontal },
      { labelKey: "nav.modelFolders", to: "/config/armazenamento", icon: FolderTree },
      { labelKey: "nav.setupWizard", to: "/setup", icon: Sparkles },
    ],
  },
];

/**
 * O menu como este perfil o vê.
 *
 * Uma seção que navega sozinha some quando a rota dela é fechada; uma seção com
 * filhas some quando **nenhuma** filha sobra, e não quando alguma sai - a seção
 * "Logs" inteira desaparece para o perfil `node` porque as duas filhas estão sob
 * `/logs`, mas "Configurações" continuaria de pé se só uma filha fosse fechada.
 *
 * A decisão de quem entra onde não está aqui: está em `lib/access.ts`, a mesma
 * tabela que o roteador consulta.
 */
export function navForProfile(profile: Profile): NavSection[] {
  const visible: NavSection[] = [];
  for (const section of NAV) {
    if (section.to) {
      if (canAccessRoute(section.to, profile)) visible.push(section);
      continue;
    }
    const children = (section.children ?? []).filter((child) =>
      canAccessRoute(child.to, profile),
    );
    if (children.length > 0) visible.push({ ...section, children });
  }
  return visible;
}
