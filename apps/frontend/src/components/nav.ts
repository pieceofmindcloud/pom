import {
  Activity,
  Blocks,
  BookOpen,
  Boxes,
  ChartNoAxesColumn,
  Cpu,
  FolderTree,
  Gauge,
  Globe2,
  History,
  Info,
  KeyRound,
  Laptop,
  ListTree,
  MessagesSquare,
  Network,
  Puzzle,
  Rocket,
  Search,
  Send,
  Settings,
  ShieldCheck,
  SlidersHorizontal,
  Terminal,
  type LucideIcon,
} from "lucide-react";
import { canAccessRoute } from "../lib/access";
import { pluginAssetUrl, pluginLabelKey, pluginMenuItems, resolvePluginRoute } from "../lib/pluginUi";
import type { Profile } from "../lib/profile";

export interface NavLeaf {
  labelKey: string;
  to: string;
  icon?: LucideIcon;
  iconImage?: string;
  children?: NavLeaf[];
  /** Promotes this child to a standalone main-menu item for a profile. */
  promoteProfiles?: readonly Profile[];
  /** Matches child routes too — used by the sidebar to mark a section open. */
  end?: boolean;
  /** Plugin that provides this item; the menu uses it to open/close the on-demand process. */
  pluginCode?: string;
}

export interface NavSection {
  labelKey: string;
  icon: LucideIcon;
  iconImage?: string;
  /** A section either navigates somewhere itself, or holds children. */
  to?: string;
  /** Plugin that provides this item; see `NavLeaf.pluginCode`. */
  pluginCode?: string;
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

/** This node's token budget screen, shown only where a logs plugin keeps budgets. */
export const USAGE_ROUTE = "/config/usage";

/**
 * The menu, in the order the operator works through it: watch the cluster, talk
 * to it, then feed it models, move files, read what happened, and configure.
 */
export const NAV: NavSection[] = [
  { labelKey: "nav.publicNetworks", icon: Globe2, to: "/public-networks" },
  { labelKey: "nav.dashboard", icon: Gauge, to: "/" },
  { labelKey: "nav.chat", icon: MessagesSquare, to: "/chat", chatPanelProfiles: ["visitor"] },
  { labelKey: "nav.networkChat", icon: MessagesSquare, to: "/network-chat" },
  {
    labelKey: "nav.models",
    icon: Boxes,
    children: [
      { labelKey: "nav.remoteModels", to: "/models/remote", icon: Search },
      { labelKey: "nav.fullModels", to: "/models", icon: Boxes, end: true },
    ],
  },
  {
    labelKey: "nav.deployments",
    icon: Rocket,
    children: [
      { labelKey: "nav.deploy", to: "/deploy", icon: Rocket },
      // Separate from "deploy" on purpose: one creates, the other shows and
      // adjusts what already exists. They are two different moments of the same work.
      { labelKey: "nav.history", to: "/deployments", icon: History },
    ],
  },
  // One screen: send a file, and everything under way or just finished.
  { labelKey: "nav.transfers", icon: Send, to: "/transfers" },
  {
    labelKey: "nav.logs",
    icon: ListTree,
    children: [
      { labelKey: "nav.requestLogs", to: "/logs", icon: ListTree },
      { labelKey: "nav.deploymentLogs", to: "/deployments/logs", icon: Terminal },
    ],
  },
  { labelKey: "nav.statistics", icon: ChartNoAxesColumn, to: "/statistics" },
  // A signed-in person's own keys; only their session opens it (`lib/access.ts`).
  { labelKey: "nav.myKeys", icon: KeyRound, to: "/config/my-keys" },
  // The plugin catalog is a storefront with a place of its own in the menu,
  // not a setting. The menus of the installed plugins are inserted right
  // after it, before Documentation (`withPluginMenus`).
  { labelKey: "nav.plugins", icon: Puzzle, to: "/plugins" },
  // The repo's own documentation, not a screen that reads the cluster: the
  // folder AGENTS.md files and subsystem READMEs embedded at build time.
  {
    labelKey: "nav.documentation",
    icon: BookOpen,
    to: "/documentation",
    footerProfiles: ["visitor"],
  },
  {
    labelKey: "nav.network",
    icon: Network,
    children: [
      { labelKey: "nav.networkInfo", to: "/config/network", icon: Network },
      { labelKey: "nav.networkParticipants", to: "/config/network/participants", icon: Network },
      { labelKey: "nav.myMachine", to: "/config/my-machine", icon: Laptop },
      { labelKey: "nav.administrators", to: "/config/administration", icon: ShieldCheck },
      { labelKey: "nav.sharing", to: "/config/sharing", icon: ShieldCheck },
      { labelKey: "nav.apiKeys", to: "/config/keys/api", icon: KeyRound },
    ],
  },
  {
    labelKey: "nav.settings",
    icon: Settings,
    footerProfiles: ["visitor"],
    footerTo: "/config/preferences",
    children: [
      { labelKey: "nav.preferences", to: "/config/preferences", icon: SlidersHorizontal },
      { labelKey: "nav.storage", to: "/config/storage", icon: FolderTree },
      { labelKey: "nav.administrativePreferences", to: "/config/preferences/admin", icon: SlidersHorizontal },
      { labelKey: "nav.about", to: "/config/about", icon: Info },
    ],
  },
];

/** Icons a plugin manifest may ask for by name; the rest becomes `Blocks`. */
const PLUGIN_ICONS: Readonly<Record<string, LucideIcon>> = {
  "list-tree": ListTree,
  "chart-no-axes-column": ChartNoAxesColumn,
  blocks: Blocks,
  gauge: Gauge,
  terminal: Terminal,
  history: History,
  settings: Settings,
  network: Network,
  boxes: Boxes,
  "shield-check": ShieldCheck,
  "book-open": BookOpen,
  "folder-tree": FolderTree,
  send: Send,
  "messages-square": MessagesSquare,
  cpu: Cpu,
  activity: Activity,
};

function pluginIcon(name?: string | null): LucideIcon {
  return (name && PLUGIN_ICONS[name]) || Blocks;
}

/**
 * Applies the menu items of active plugins: an item whose `to` already exists
 * replaces that native destination; each new item goes in at the root, right
 * after the entries that plugin already holds, or before the documentation when
 * it holds none, in manifest order. An item with `children` becomes a section with
 * submenus.
 */
function withPluginMenu(sections: NavSection[], profile: Profile): NavSection[] {
  const result: NavSection[] = sections.map((section) => {
    const copy: NavSection = { ...section };
    if (section.children) copy.children = section.children.map((child) => ({ ...child }));
    return copy;
  });
  // Without a plugin serving them, Request Logs and Statistics are the same
  // advertising page of one purchase (the Business Pack plugin, `logs`, also
  // brings Quota): one menu entry, so it does not look like three things to buy.
  if (!resolvePluginRoute("/logs") && !resolvePluginRoute("/statistics")) {
    // The deployment logs are kept by the same plugin, so without it the whole
    // Logs section is that one entry: the Business Pack plugin's page.
    const logs = result.findIndex((section) => section.labelKey === "nav.logs");
    if (logs >= 0) {
      result[logs] = { labelKey: "nav.businessPack", icon: result[logs].icon, to: "/logs" };
    }
    const statistics = result.findIndex((section) => section.to === "/statistics");
    if (statistics >= 0) result.splice(statistics, 1);
  }
  const replace = (
    target: { labelKey: string; icon?: LucideIcon; iconImage?: string; pluginCode?: string },
    item: ReturnType<typeof pluginMenuItems>[number],
  ) => {
    target.labelKey = pluginLabelKey(item.code, item.id);
    target.icon = pluginIcon(item.icon);
    target.iconImage = item.iconImage ? pluginAssetUrl(item.code, item.iconImage) : undefined;
    target.pluginCode = item.code;
  };
  for (const item of pluginMenuItems(profile)) {
    const key = pluginLabelKey(item.code, item.id);
    let placed = false;
    for (const section of result) {
      if (section.to === item.to) {
        section.labelKey = key;
        section.icon = pluginIcon(item.icon);
        section.iconImage = item.iconImage ? pluginAssetUrl(item.code, item.iconImage) : undefined;
        section.pluginCode = item.code;
        placed = true;
      }
      for (const child of section.children ?? []) {
        if (child.to === item.to) {
          replace(child, item);
          placed = true;
        }
      }
    }
    if (placed) continue;
    // Next to the entries this plugin already took (Quota after Statistics,
    // both the logs plugin's), else at the root before the documentation.
    const own = result.map((s) => s.pluginCode === item.code || s.children?.some((c) => c.pluginCode === item.code));
    const last = own.lastIndexOf(true);
    const docs = last >= 0 ? last + 1 : result.findIndex((s) => s.labelKey === "nav.documentation");
    const iconImage = item.iconImage ? pluginAssetUrl(item.code, item.iconImage) : undefined;
    // With submenus, the item becomes an expandable section; the children use the
    // named icon of each one, and the plugin's image stays only on the section.
    const entry: NavSection = item.children
      ? {
          labelKey: key,
          icon: pluginIcon(item.icon),
          iconImage,
          pluginCode: item.code,
          children: item.children.map((child) => ({
            labelKey: pluginLabelKey(child.code, child.id),
            to: child.to,
            icon: pluginIcon(child.icon),
            end: true,
            pluginCode: child.code,
          })),
        }
      : {
          labelKey: key,
          icon: pluginIcon(item.icon),
          iconImage,
          to: item.to,
          pluginCode: item.code,
        };
    result.splice(docs < 0 ? result.length : docs, 0, entry);
  }
  return result;
}

/**
 * The menu as this profile sees it.
 *
 * A section that navigates by itself disappears when its route is closed; a section with
 * children disappears when **none** of the children remains, and not when one goes out. The screens
 * served by plugins (Request Logs, Statistics) are deliberately
 * discoverable and stay in the menu even without a license; the screen itself shows the
 * activation invitation, and "Plugins" lists the whole catalog.
 *
 * The decision of who goes where is not here: it is in `lib/access.ts`, the same
 * table the router consults.
 */
export interface NavOptions {
  /**
   * Whether this node takes part in the network Chat. A visitor does only
   * when the network opens it (`NetworkSettings.chat_visitors`); `false`
   * drops the entry. Left out, it stays.
   */
  networkChat?: boolean;
}

export function navForProfile(profile: Profile, _enterpriseActive = true, options: NavOptions = {}): NavSection[] {
  const filterLeaf = (leaf: NavLeaf): NavLeaf | null => {
    const children = (leaf.children ?? [])
      .map(filterLeaf)
      .filter((child): child is NavLeaf => child !== null);
    if (!canAccessRoute(leaf.to, profile) && children.length === 0) return null;
    return { ...leaf, ...(children.length > 0 ? { children } : {}) };
  };
  const visible: NavSection[] = [];
  for (const section of withPluginMenu(NAV, profile)) {
    if (section.chatPanelProfiles?.includes(profile)) continue;
    if (section.footerProfiles?.includes(profile)) {
      for (const child of section.children ?? []) {
        if (!child.promoteProfiles?.includes(profile)) continue;
        if (!canAccessRoute(child.to, profile)) continue;
        visible.push({
          labelKey: child.labelKey,
          icon: child.icon ?? section.icon,
          iconImage: child.iconImage ?? section.iconImage,
          to: child.to,
          children: (child.children ?? []).map(filterLeaf).filter((item): item is NavLeaf => item !== null),
        });
      }
      continue;
    }
    if (section.to) {
      if (options.networkChat === false && section.to === "/network-chat") continue;
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
