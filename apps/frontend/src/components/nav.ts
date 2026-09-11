import {
  BookOpen,
  Boxes,
  ChartNoAxesColumn,
  FolderTree,
  Gauge,
  Globe2,
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
  ShieldCheck,
  SlidersHorizontal,
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
  children?: NavLeaf[];
  /** Promotes this child to a standalone main-menu item for a profile. */
  promoteProfiles?: readonly Profile[];
  /** Matches child routes too — used by the sidebar to mark a section open. */
  end?: boolean;
}

export interface NavSection {
  labelKey: string;
  icon: LucideIcon;
  /** A section either navigates somewhere itself, or holds children. */
  to?: string;
  children?: NavLeaf[];
  /** Profiles for which this destination is rendered in the utility footer. */
  footerProfiles?: readonly Profile[];
  /** Footer target for a section whose normal destination is a child route. */
  footerTo?: string;
  /** Profiles that get the conversation panel instead of a normal Chat link. */
  chatPanelProfiles?: readonly Profile[];
}

export interface NavFooterItem {
  labelKey: string;
  to: string;
  icon: LucideIcon;
}

/**
 * The menu, in the order the operator works through it: watch the cluster, talk
 * to it, then feed it models, move files, read what happened, and configure.
 */
export const NAV: NavSection[] = [
  { labelKey: "nav.publicNetworks", icon: Globe2, to: "/redes-publicas" },
  { labelKey: "nav.dashboard", icon: Gauge, to: "/" },
  { labelKey: "nav.chat", icon: MessagesSquare, to: "/chat", chatPanelProfiles: ["infer"] },
  { labelKey: "nav.networkChat", icon: MessagesSquare, to: "/network-chat" },
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
    children: [
      { labelKey: "nav.requestLogs", to: "/logs", icon: ListTree },
      { labelKey: "nav.deploymentLogs", to: "/implantacoes/logs", icon: Terminal },
    ],
  },
  { labelKey: "nav.statistics", icon: ChartNoAxesColumn, to: "/estatisticas" },
  // The repo's own documentation, not a screen that reads the cluster: the
  // folder AGENTS.md files and subsystem READMEs embedded at build time.
  {
    labelKey: "nav.documentation",
    icon: BookOpen,
    to: "/documentacao",
    footerProfiles: ["infer"],
  },
  {
    labelKey: "nav.network",
    icon: Network,
    children: [
      { labelKey: "nav.networkInfo", to: "/config/rede", icon: Network },
      { labelKey: "nav.networkParticipants", to: "/config/rede/participantes", icon: Network },
      { labelKey: "nav.administrators", to: "/config/administracao", icon: ShieldCheck },
      { labelKey: "nav.sharing", to: "/config/compartilhamento", icon: ShieldCheck },
      { labelKey: "nav.apiKeys", to: "/config/keys/api", icon: KeyRound },
    ],
  },
  {
    labelKey: "nav.settings",
    icon: Settings,
    footerProfiles: ["infer"],
    footerTo: "/config/preferencias",
    children: [
      { labelKey: "nav.preferences", to: "/config/preferencias", icon: SlidersHorizontal },
      { labelKey: "nav.storage", to: "/config/armazenamento", icon: FolderTree },
      { labelKey: "nav.administrativePreferences", to: "/config/preferencias/admin", icon: SlidersHorizontal },
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
  const filterLeaf = (leaf: NavLeaf): NavLeaf | null => {
    const children = (leaf.children ?? [])
      .map(filterLeaf)
      .filter((child): child is NavLeaf => child !== null);
    if (!canAccessRoute(leaf.to, profile) && children.length === 0) return null;
    return { ...leaf, ...(children.length > 0 ? { children } : {}) };
  };
  const visible: NavSection[] = [];
  for (const section of NAV) {
    if (section.chatPanelProfiles?.includes(profile)) continue;
    if (section.footerProfiles?.includes(profile)) {
      for (const child of section.children ?? []) {
        if (!child.promoteProfiles?.includes(profile)) continue;
        if (!canAccessRoute(child.to, profile)) continue;
        visible.push({ labelKey: child.labelKey, icon: child.icon ?? section.icon, to: child.to, children: (child.children ?? []).map(filterLeaf).filter((item): item is NavLeaf => item !== null) });
      }
      continue;
    }
    if (section.to) {
      if (canAccessRoute(section.to, profile)) visible.push(section);
      continue;
    }
    const children = (section.children ?? [])
      .map(filterLeaf)
      .filter((child): child is NavLeaf => child !== null);
    if (children.length > 0) visible.push({ ...section, children });
  }
  return visible;
}

/** Destinations kept beside the language and theme controls for a profile. */
export function navFooterForProfile(profile: Profile): NavFooterItem[] {
  return NAV.flatMap((section) => {
    if (!section.footerProfiles?.includes(profile)) return [];
    const to = section.footerTo ?? section.to;
    if (!to || !canAccessRoute(to, profile)) return [];
    return [{ labelKey: section.labelKey, to, icon: section.icon }];
  });
}
