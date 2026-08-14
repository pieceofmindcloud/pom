import {
  Boxes,
  FolderTree,
  Gauge,
  KeyRound,
  Layers,
  ListTree,
  MessagesSquare,
  Network,
  Radio,
  Rocket,
  Scissors,
  Send,
  Settings,
  SlidersHorizontal,
  Upload,
  type LucideIcon,
} from "lucide-react";

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
  { labelKey: "nav.testChat", icon: MessagesSquare, to: "/chat" },
  {
    labelKey: "nav.models",
    icon: Boxes,
    children: [
      { labelKey: "nav.fullModels", to: "/modelos", icon: Boxes, end: true },
      { labelKey: "nav.sliceModels", to: "/modelos/fatiar", icon: Scissors },
    ],
  },
  {
    labelKey: "nav.shardedModels",
    icon: Layers,
    children: [
      { labelKey: "nav.shards", to: "/fatias", icon: Layers, end: true },
      { labelKey: "nav.slicing", to: "/fatias/andamento", icon: ListTree },
    ],
  },
  // Its own entry, not a leaf of the shards section: putting a set of slices
  // into service is the act the whole screen exists for, and it was two clicks
  // deep behind a section that reads as a file listing.
  { labelKey: "nav.deploy", icon: Rocket, to: "/implantar" },
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
    icon: Radio,
    children: [
      { labelKey: "nav.byToken", to: "/logs", icon: ListTree, end: true },
      { labelKey: "nav.live", to: "/logs/ao-vivo", icon: Radio },
    ],
  },
  {
    labelKey: "nav.settings",
    icon: Settings,
    children: [
      { labelKey: "nav.network", to: "/config/rede", icon: Network },
      { labelKey: "nav.apiKeys", to: "/config/keys/api", icon: KeyRound },
      { labelKey: "nav.connectionKeys", to: "/config/keys/conexao", icon: SlidersHorizontal },
      { labelKey: "nav.modelFolders", to: "/config/pastas", icon: FolderTree },
    ],
  },
];
