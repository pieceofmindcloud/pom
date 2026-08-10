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
  label: string;
  to: string;
  icon?: LucideIcon;
  /** Matches child routes too — used by the sidebar to mark a section open. */
  end?: boolean;
}

export interface NavSection {
  label: string;
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
  { label: "Painel", icon: Gauge, to: "/" },
  { label: "Chat de teste", icon: MessagesSquare, to: "/chat" },
  {
    label: "Modelos",
    icon: Boxes,
    children: [
      { label: "Modelos inteiros", to: "/modelos", icon: Boxes, end: true },
      { label: "Fatiar modelos", to: "/modelos/fatiar", icon: Scissors },
    ],
  },
  {
    label: "Modelos fatiados",
    icon: Layers,
    children: [
      { label: "Fatias", to: "/fatias", icon: Layers, end: true },
      { label: "Implantar na rede", to: "/fatias/implantar", icon: Rocket },
      { label: "Fatiamentos em andamento", to: "/fatias/andamento", icon: ListTree },
    ],
  },
  {
    label: "Transferências",
    icon: Send,
    children: [
      { label: "Enviar um arquivo", to: "/transferencias/enviar", icon: Upload },
      { label: "Em andamento", to: "/transferencias", icon: Send, end: true },
    ],
  },
  {
    label: "Logs",
    icon: Radio,
    children: [
      { label: "Por token e por nó", to: "/logs", icon: ListTree, end: true },
      { label: "Ao vivo", to: "/logs/ao-vivo", icon: Radio },
    ],
  },
  {
    label: "Configurações",
    icon: Settings,
    children: [
      { label: "Rede", to: "/config/rede", icon: Network },
      { label: "API keys", to: "/config/keys/api", icon: KeyRound },
      { label: "Chaves de conexão", to: "/config/keys/conexao", icon: SlidersHorizontal },
      { label: "Pastas de modelos", to: "/config/pastas", icon: FolderTree },
    ],
  },
];
