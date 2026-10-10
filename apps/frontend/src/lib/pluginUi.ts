/**
 * Interface provided by plugins (`pom-plugin-ui/v1` contract).
 *
 * The node validates the manifest and serves `GET /api/ui/plugins/ui`; this module keeps
 * a validated registry in which routes and menu destinations get the same
 * `/<plugin_code>` namespace, except for native points claimed without collision.
 * That way, the link and the matcher use the same path, and different plugins can
 * declare the same relative route. Plugin UIs are local to the node and available
 * to every profile; the manifest's `roles`/`profiles` are kept for compatibility,
 * but do not restrict this interface.
 */
import type { Profile } from "./profile";

export interface PluginMenuItem {
  id: string;
  label: Record<string, string>;
  icon?: string | null;
  to: string;
  /** Role metadata retained for compatibility; current POM exposes plugin UIs to every profile. */
  roles?: Profile[];
  group?: string | null;
  /** Legacy role metadata; validated and preserved, but not used to gate plugin UI. */
  profiles?: Profile[];
  /** Submenus: the item becomes an expandable section of the side menu. */
  children?: PluginMenuChild[];
}

export interface PluginMenuChild {
  id: string;
  label: Record<string, string>;
  icon?: string | null;
  to: string;
  /** Role metadata retained for compatibility; it does not gate plugin UI. */
  roles?: Profile[];
}

export interface PluginRoute {
  path: string;
  screen: string;
  /** Role metadata retained for compatibility; current POM exposes plugin UIs to every profile. */
  roles?: Profile[];
  /** Legacy role metadata; validated and preserved, but not used to gate plugin UI. */
  profiles?: Profile[];
  full_bleed?: boolean;
}

export interface PluginScreenSpec {
  module: string;
  /** module export that is the screen's component; defaults to `default` */
  export?: string;
  styles?: string[];
}

export interface PluginUiManifest {
  schema: "pom-plugin-ui/v1";
  plugin_code: string;
  /** Optional PNG asset; the icon declared in the menu remains the fallback. */
  icon_image?: string;
  /** Description per language, shown on the Plugins screen. */
  description?: Record<string, string>;
  menu: PluginMenuItem[];
  routes: PluginRoute[];
  screens: Record<string, PluginScreenSpec>;
  /** Extension slots inside native screens: slot id -> key of `screens`. */
  slots?: Record<string, string>;
  /** language -> asset of the plugin's text catalog */
  i18n?: Record<string, string>;
  /** The plugin's Markdown documentation, split into guides and reference. */
  documentation?: PluginDocumentation;
  assets: string[];
}

export interface PluginDocumentation {
  guides: PluginDocumentationEntry[];
  reference: PluginDocumentationEntry[];
}

export interface PluginDocumentationEntry {
  id: string;
  title: Record<string, string>;
  asset: string;
}

export interface ActivePluginUi {
  code: string;
  version?: string | null;
  manifest: PluginUiManifest;
  /** `lifecycle: "on_demand"` from the manifest: the process starts when opened and can be stopped. */
  on_demand?: boolean;
  /** The host process is running now. */
  running?: boolean;
}

/** A plugin component mounted inside a native screen (see `pluginSlot`). */
export interface ResolvedPluginSlot {
  code: string;
  slot: string;
  screen: PluginScreenSpec;
  version: string | null;
}

export interface ResolvedPluginRoute {
  code: string;
  route: PluginRoute;
  screen: PluginScreenSpec;
  version: string | null;
}

interface RegisteredMenuItem extends Omit<PluginMenuItem, "children"> {
  code: string;
  roles: Profile[];
  iconImage?: string;
  children?: RegisteredMenuItem[];
}

interface RegisteredPlugin {
  plugin: ActivePluginUi;
  routes: ResolvedPluginRoute[];
  menu: RegisteredMenuItem[];
}

const PLUGIN_CODE_PATTERN = /^[A-Za-z0-9](?:[A-Za-z0-9._-]{0,126}[A-Za-z0-9])?$/;
const ALL_PROFILES: readonly Profile[] = ["admin", "node", "infer"];
/** POM native routes that a manifest can provide as a replacement. */
const POM_PLUGIN_EXTENSION_ROUTES = new Set(["/logs", "/statistics"]);

let registry: RegisteredPlugin[] = [];
let snapshot: ActivePluginUi[] = [];
let previousInput: readonly ActivePluginUi[] | null | undefined;

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isNonEmptyString(value: unknown): value is string {
  return typeof value === "string" && value.length > 0 && value.trim() === value;
}

function isStringArray(value: unknown): value is string[] {
  return Array.isArray(value) && value.every(isNonEmptyString);
}

function isProfileArray(value: unknown): value is Profile[] {
  return (
    Array.isArray(value) &&
    value.length > 0 &&
    value.every((profile) => profile === "admin" || profile === "node" || profile === "infer")
  );
}

function isMenuRoleArray(value: unknown): value is Profile[] {
  return (
    Array.isArray(value) &&
    value.every((role) => role === "admin" || role === "node" || role === "infer")
  );
}

/** Plugin UIs are local to this node; manifest role metadata does not gate them. */
function effectiveRouteProfiles(_route: PluginRoute): Profile[] {
  return [...ALL_PROFILES];
}

/**
 * Canonicalizes a path without decoding separators or accepting segments
 * that could leave the plugin's scope. Location paths may contain
 * query/hash; values coming from the manifest may not.
 */
function normalizePathname(value: unknown, stripSuffix = false): string | null {
  if (typeof value !== "string" || !value || value.trim() !== value) return null;
  if (/[\\\u0000-\u001f\u007f]/.test(value)) return null;

  const pathname = stripSuffix ? value.split(/[?#]/, 1)[0] : value;
  if (!pathname || (!stripSuffix && /[?#]/.test(pathname))) return null;

  const segments = pathname.split("/").filter(Boolean);
  const normalized: string[] = [];
  for (const segment of segments) {
    let decoded = segment;
    try {
      while (true) {
        const next = decodeURIComponent(decoded);
        if (next === decoded) break;
        decoded = next;
      }
    } catch {
      return null;
    }
    if (decoded === "." || decoded === ".." || /[\/\\\u0000-\u001f\u007f?#]/.test(decoded)) {
      return null;
    }
    normalized.push(segment);
  }
  return normalized.length ? `/${normalized.join("/")}` : "/";
}

/** Applies the namespace to the path declared by a plugin, without duplicating it. */
function pluginPath(code: string, value: unknown, extensionRoutes: ReadonlySet<string>): string | null {
  const path = normalizePathname(value);
  if (path === null) return null;
  if ([...extensionRoutes].some((extensionRoute) => isUnder(path, extensionRoute))) return path;
  if (path === "/") return `/${code}`;
  const firstSegment = path.slice(1).split("/", 1)[0];
  return firstSegment === code ? path : `/${code}${path}`;
}

function isUnder(path: string, prefix: string): boolean {
  return path === prefix || path.startsWith(`${prefix}/`);
}

function normalizePlugin(
  value: unknown,
  extensionRoutes: ReadonlySet<string>,
): RegisteredPlugin | null {
  if (!isRecord(value) || !isNonEmptyString(value.code) || value.code.length > 128) return null;
  const code = value.code;
  if (!PLUGIN_CODE_PATTERN.test(code) || !isRecord(value.manifest)) return null;
  if (value.version !== undefined && value.version !== null && !isNonEmptyString(value.version)) return null;

  const manifest = value.manifest;
  if (
    manifest.schema !== "pom-plugin-ui/v1" ||
    manifest.plugin_code !== code ||
    !Array.isArray(manifest.menu) ||
    !Array.isArray(manifest.routes) ||
    !isRecord(manifest.screens) ||
    !isStringArray(manifest.assets)
  ) {
    return null;
  }
  const iconImage = manifest.icon_image;
  const documentation = manifest.documentation;
  if (
    (iconImage !== undefined &&
      (!isNonEmptyString(iconImage) ||
        !iconImage.endsWith(".png") ||
        !manifest.assets.includes(iconImage))) ||
    (documentation !== undefined && !isRecord(documentation))
  ) {
    return null;
  }
  const documentationRecord = isRecord(documentation) ? documentation : undefined;
  const documentationEntries = documentationRecord
    ? [documentationRecord.guides, documentationRecord.reference]
    : [];
  if (documentationEntries.some((entries) => !Array.isArray(entries))) return null;
  for (const entries of documentationEntries as unknown[][]) {
    for (const entry of entries) {
      if (
        !isRecord(entry) ||
        !isNonEmptyString(entry.id) ||
        !isRecord(entry.title) ||
        !isNonEmptyString(entry.title.en) ||
        !Object.values(entry.title).every(isNonEmptyString) ||
        !isNonEmptyString(entry.asset) ||
        !entry.asset.startsWith("docs/") ||
        !entry.asset.endsWith(".md") ||
        !manifest.assets.includes(entry.asset)
      ) {
        return null;
      }
    }
  }
  if (
    manifest.i18n !== undefined &&
    (!isRecord(manifest.i18n) || !Object.values(manifest.i18n).every(isNonEmptyString))
  ) {
    return null;
  }

  const screens: Record<string, PluginScreenSpec> = Object.create(null);
  for (const [name, candidate] of Object.entries(manifest.screens)) {
    if (
      !name ||
      !isRecord(candidate) ||
      !isNonEmptyString(candidate.module) ||
      (candidate.export !== undefined && !isNonEmptyString(candidate.export)) ||
      (candidate.styles !== undefined && !isStringArray(candidate.styles))
    ) {
      return null;
    }
    screens[name] = {
      module: candidate.module,
      ...(candidate.export !== undefined ? { export: candidate.export as string } : {}),
      ...(candidate.styles !== undefined ? { styles: [...(candidate.styles as string[])] } : {}),
    };
  }

  const SLOT_ID = /^[a-z][a-z0-9._-]{0,63}$/;
  let slots: Record<string, string> | undefined;
  if (manifest.slots !== undefined) {
    if (!isRecord(manifest.slots)) return null;
    slots = Object.create(null) as Record<string, string>;
    for (const [slot, screen] of Object.entries(manifest.slots)) {
      if (
        !SLOT_ID.test(slot) ||
        !isNonEmptyString(screen) ||
        !Object.prototype.hasOwnProperty.call(screens, screen)
      ) {
        return null;
      }
      slots[slot] = screen;
    }
  }

  const routes: ResolvedPluginRoute[] = [];
  const routePaths = new Set<string>();
  const rawRoutes: PluginRoute[] = [];
  for (const candidate of manifest.routes) {
    if (
      !isRecord(candidate) ||
      !isNonEmptyString(candidate.screen) ||
      !Object.prototype.hasOwnProperty.call(screens, candidate.screen) ||
      (candidate.roles !== undefined && !isMenuRoleArray(candidate.roles)) ||
      (candidate.profiles !== undefined && !isProfileArray(candidate.profiles)) ||
      (candidate.full_bleed !== undefined && typeof candidate.full_bleed !== "boolean")
    ) {
      return null;
    }
    const path = pluginPath(code, candidate.path, extensionRoutes);
    const screen = screens[candidate.screen];
    if (!path || !screen || routePaths.has(path)) return null;
    routePaths.add(path);
    const profiles = effectiveRouteProfiles(candidate as unknown as PluginRoute);
    const route: PluginRoute = {
      path,
      screen: candidate.screen,
      profiles,
      ...(candidate.full_bleed !== undefined ? { full_bleed: candidate.full_bleed } : {}),
    };
    rawRoutes.push({
      path: candidate.path as string,
      screen: candidate.screen,
      ...(candidate.roles !== undefined ? { roles: [...(candidate.roles as Profile[])] } : {}),
      ...(candidate.profiles !== undefined ? { profiles: [...(candidate.profiles as Profile[])] } : {}),
      ...(candidate.full_bleed !== undefined ? { full_bleed: candidate.full_bleed } : {}),
    });
    routes.push({ code, route, screen, version: (value.version as string | null | undefined) ?? null });
  }

  const menu: RegisteredMenuItem[] = [];
  const menuIds = new Set<string>();
  const rawMenu: PluginMenuItem[] = [];
  const registerChild = (candidate: unknown): RegisteredMenuItem | null => {
    if (
      !isRecord(candidate) ||
      !isNonEmptyString(candidate.id) ||
      menuIds.has(candidate.id) ||
      !isRecord(candidate.label) ||
      !isNonEmptyString(candidate.label.en) ||
      !Object.values(candidate.label).every((label) => typeof label === "string") ||
      (candidate.roles !== undefined && !isMenuRoleArray(candidate.roles)) ||
      (candidate.icon !== undefined && candidate.icon !== null && typeof candidate.icon !== "string")
    ) {
      return null;
    }
    const to = pluginPath(code, candidate.to, extensionRoutes);
    if (!to || !routes.some(({ route }) => isUnder(to, route.path))) return null;
    menuIds.add(candidate.id);
    const roles = [...ALL_PROFILES];
    return {
      code,
      id: candidate.id,
      label: { ...(candidate.label as Record<string, string>) },
      to,
      roles,
      ...(isNonEmptyString(iconImage) ? { iconImage } : {}),
      ...(candidate.icon !== undefined ? { icon: candidate.icon as string | null } : {}),
    };
  };
  for (const candidate of manifest.menu) {
    if (
      !isRecord(candidate) ||
      !isNonEmptyString(candidate.id) ||
      menuIds.has(candidate.id) ||
      !isRecord(candidate.label) ||
      !isNonEmptyString(candidate.label.en) ||
      !Object.values(candidate.label).every((label) => typeof label === "string") ||
      (candidate.roles !== undefined && !isMenuRoleArray(candidate.roles)) ||
      (candidate.profiles !== undefined && !isProfileArray(candidate.profiles)) ||
      (candidate.icon !== undefined && candidate.icon !== null && typeof candidate.icon !== "string") ||
      (candidate.group !== undefined && candidate.group !== null && typeof candidate.group !== "string")
    ) {
      return null;
    }
    const to = pluginPath(code, candidate.to, extensionRoutes);
    if (!to || !routes.some(({ route }) => isUnder(to, route.path))) return null;
    const roles = [...ALL_PROFILES];
    menuIds.add(candidate.id);
    if (candidate.children !== undefined && !Array.isArray(candidate.children)) return null;
    const children: RegisteredMenuItem[] = [];
    for (const child of (candidate.children as unknown[] | undefined) ?? []) {
      const registered = registerChild(child);
      if (!registered) return null;
      children.push(registered);
    }
    menu.push({
      code,
      id: candidate.id,
      label: { ...(candidate.label as Record<string, string>) },
      to,
      roles,
      ...(children.length > 0 ? { children } : {}),
      ...(isNonEmptyString(iconImage) ? { iconImage } : {}),
      ...(candidate.icon !== undefined ? { icon: candidate.icon as string | null } : {}),
      ...(candidate.group !== undefined ? { group: candidate.group as string | null } : {}),
    });
    rawMenu.push({
      id: candidate.id,
      label: { ...(candidate.label as Record<string, string>) },
      to: candidate.to as string,
      ...(candidate.roles !== undefined ? { roles: [...(candidate.roles as Profile[])] } : {}),
      ...(candidate.profiles !== undefined ? { profiles: [...(candidate.profiles as Profile[])] } : {}),
      ...(candidate.icon !== undefined ? { icon: candidate.icon as string | null } : {}),
      ...(candidate.group !== undefined ? { group: candidate.group as string | null } : {}),
      ...(children.length > 0
        ? {
            children: (candidate.children as Record<string, unknown>[]).map((child) => ({
              id: child.id as string,
              label: { ...(child.label as Record<string, string>) },
              to: child.to as string,
              ...(child.icon !== undefined ? { icon: child.icon as string | null } : {}),
              ...(child.roles !== undefined ? { roles: [...(child.roles as Profile[])] } : {}),
            })),
          }
        : {}),
    });
  }

  const description = isRecord(manifest.description) &&
    Object.values(manifest.description).every((text) => typeof text === "string")
    ? { ...(manifest.description as Record<string, string>) }
    : undefined;

  const plugin: ActivePluginUi = {
    code,
    ...(value.version !== undefined ? { version: value.version as string | null } : {}),
    manifest: {
      schema: "pom-plugin-ui/v1",
      plugin_code: code,
      ...(isNonEmptyString(iconImage) ? { icon_image: iconImage } : {}),
      ...(description ? { description } : {}),
      menu: rawMenu,
      routes: rawRoutes,
      screens,
      ...(slots ? { slots: { ...slots } } : {}),
      assets: [...manifest.assets],
      ...(manifest.i18n !== undefined
        ? { i18n: { ...(manifest.i18n as Record<string, string>) } }
        : {}),
      ...(documentationRecord !== undefined
        ? {
            documentation: {
              guides: [...(documentationRecord.guides as PluginDocumentationEntry[])],
              reference: [...(documentationRecord.reference as PluginDocumentationEntry[])],
            },
          }
        : {}),
    },
  };
  return { plugin, routes, menu };
}

/** Swaps the registry. Invalid or ambiguous manifests are ignored. */
export function setPluginUi(plugins: readonly ActivePluginUi[] | null | undefined): void {
  if (plugins === previousInput) return;
  previousInput = plugins;

  const validPlugins = Array.isArray(plugins)
    ? plugins
        .map((plugin) => normalizePlugin(plugin, new Set()))
        .filter((plugin): plugin is RegisteredPlugin => plugin !== null)
    : [];
  const claims = new Map<string, Set<RegisteredPlugin>>();
  for (const plugin of validPlugins) {
    for (const route of plugin.plugin.manifest.routes) {
      const path = normalizePathname(route.path);
      if (!path || !POM_PLUGIN_EXTENSION_ROUTES.has(path)) continue;
      const owners = claims.get(path) ?? new Set<RegisteredPlugin>();
      owners.add(plugin);
      claims.set(path, owners);
    }
  }
  const extensionRoutesByPlugin = new Map<RegisteredPlugin, Set<string>>();
  for (const [path, owners] of claims) {
    if (owners.size !== 1) continue;
    const owner = [...owners][0];
    const routes = extensionRoutesByPlugin.get(owner) ?? new Set<string>();
    routes.add(path);
    extensionRoutesByPlugin.set(owner, routes);
  }
  const normalized = validPlugins.flatMap((plugin) => {
    const normalizedPlugin = normalizePlugin(
      plugin.plugin,
      extensionRoutesByPlugin.get(plugin) ?? new Set(),
    );
    return normalizedPlugin ? [normalizedPlugin] : [];
  });
  const codeCounts = new Map<string, number>();
  for (const plugin of normalized) {
    codeCounts.set(plugin.plugin.code, (codeCounts.get(plugin.plugin.code) ?? 0) + 1);
  }
  registry = normalized.filter((plugin) => codeCounts.get(plugin.plugin.code) === 1);
  snapshot = registry.map(({ plugin }) => plugin);
}

export function pluginUiSnapshot(): readonly ActivePluginUi[] {
  return snapshot;
}

/**
 * The component an active plugin mounts in a native extension slot, or `null`.
 * With several claimants the first plugin in the node's order wins, so the
 * choice is stable across renders.
 */
export function pluginSlot(slot: string): ResolvedPluginSlot | null {
  for (const { plugin } of registry) {
    const screenName = plugin.manifest.slots?.[slot];
    const screen = screenName ? plugin.manifest.screens[screenName] : undefined;
    if (screen) return { code: plugin.code, slot, screen, version: plugin.version ?? null };
  }
  return null;
}

/** The most specific plugin route that covers this path, or `null`. */
export function resolvePluginRoute(path: string): ResolvedPluginRoute | null {
  const pathname = normalizePathname(path, true);
  if (!pathname) return null;

  let best: ResolvedPluginRoute | null = null;
  for (const plugin of registry) {
    for (const route of plugin.routes) {
      if (!isUnder(pathname, route.route.path)) continue;
      if (best && best.route.path.length >= route.route.path.length) continue;
      best = route;
    }
  }
  return best;
}

function flatMenu(items: readonly RegisteredMenuItem[]): RegisteredMenuItem[] {
  return items.flatMap((item) => [item, ...(item.children ?? [])]);
}

/** Profiles that may open a plugin route; `null` when the path is not a plugin route. */
export function pluginRouteProfiles(path: string): readonly Profile[] | null {
  const route = resolvePluginRoute(path);
  return route ? effectiveRouteProfiles(route.route) : null;
}

/**
 * Menu items visible to the profile. An item with submenus survives when
 * at least one child is visible, and carries only the visible children.
 */
export function pluginMenuItems(profile: Profile): RegisteredMenuItem[] {
  return registry.flatMap((plugin) => {
    const visible = (item: RegisteredMenuItem) => {
      if (!item.roles.includes(profile)) return false;
      const route = plugin.routes
        .filter(({ route }) => isUnder(item.to, route.path))
        .reduce<ResolvedPluginRoute | null>(
          (best, candidate) => !best || candidate.route.path.length > best.route.path.length ? candidate : best,
          null,
        );
      return route !== null && effectiveRouteProfiles(route.route).includes(profile);
    };
    return plugin.menu.flatMap((item) => {
      if (!item.children) return visible(item) ? [item] : [];
      const children = item.children.filter(visible);
      return children.length > 0 ? [{ ...item, children }] : [];
    });
  });
}

export const pluginLabelKey = (code: string, id: string) => `plugin:${code}:${id}`;

/** Label of a plugin menu item in the active language; `en` is the floor. */
export function pluginLabel(key: string, locale: string): string | null {
  const [, code, id] = key.split(":");
  const item = flatMenu(registry.find((plugin) => plugin.plugin.code === code)?.menu ?? [])
    .find((entry) => entry.id === id);
  if (!item) return null;
  return localized(item.label, locale);
}

function localized(texts: Record<string, string>, locale: string): string | null {
  return texts[locale] ?? texts[locale.split("-")[0]] ?? texts.en ?? null;
}

/**
 * Human name of an active plugin: the label of the menu item whose id equals
 * the code, otherwise the first item, otherwise the code itself.
 */
export function pluginDisplayName(code: string, locale: string): string {
  const plugin = registry.find((entry) => entry.plugin.code === code);
  const item = plugin?.menu.find((entry) => entry.id === code) ?? plugin?.menu[0];
  return (item && localized(item.label, locale)) || code;
}

export const pluginAssetUrl = (code: string, asset: string) =>
  `/api/ui/plugins/${encodeURIComponent(code)}/assets/${asset}`;
