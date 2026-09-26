/**
 * Interface fornecida por plugins (contrato `pom-plugin-ui/v1`).
 *
 * O nó valida o manifesto e serve `GET /api/ui/plugins/ui`; este módulo mantém
 * um registro validado em que rotas e destinos de menu recebem o mesmo
 * namespace `/<plugin_code>`, salvo pontos nativos reivindicados sem colisão.
 * Assim, o link, o matcher e o controle de roles usam o mesmo caminho e
 * plugins diferentes podem declarar a mesma rota relativa.
 */
import type { Profile } from "./profile";

export interface PluginMenuItem {
  id: string;
  label: Record<string, string>;
  icon?: string | null;
  to: string;
  /** Perfis novos do manifesto; na ausência, consulta `profiles` legado. */
  roles?: Profile[];
  /** Campo legado de menus. Continua aceito quando `roles` não foi declarado. */
  group?: string | null;
  profiles?: Profile[];
}

export interface PluginRoute {
  path: string;
  screen: string;
  /** Roles do contrato atual; na ausência, consulta `profiles` legado. */
  roles?: Profile[];
  /** Campo legado de rotas. Continua aceito quando `roles` não foi declarado. */
  profiles?: Profile[];
  full_bleed?: boolean;
}

export interface PluginScreenSpec {
  module: string;
  /** export do módulo que é o componente da tela; padrão `default` */
  export?: string;
  styles?: string[];
}

export interface PluginUiManifest {
  schema: "pom-plugin-ui/v1";
  plugin_code: string;
  /** Asset PNG opcional; o ícone declarado no menu continua sendo o fallback. */
  icon_image?: string;
  menu: PluginMenuItem[];
  routes: PluginRoute[];
  screens: Record<string, PluginScreenSpec>;
  /** idioma -> asset do catálogo de textos do plugin */
  i18n?: Record<string, string>;
  /** Documentação Markdown do plugin, separada em guias e referência. */
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
}

export interface ResolvedPluginRoute {
  code: string;
  route: PluginRoute;
  screen: PluginScreenSpec;
  version: string | null;
}

interface RegisteredMenuItem extends PluginMenuItem {
  code: string;
  roles: Profile[];
  iconImage?: string;
}

interface RegisteredPlugin {
  plugin: ActivePluginUi;
  routes: ResolvedPluginRoute[];
  menu: RegisteredMenuItem[];
}

const PLUGIN_CODE_PATTERN = /^[A-Za-z0-9](?:[A-Za-z0-9._-]{0,126}[A-Za-z0-9])?$/;
const ALL_PROFILES: readonly Profile[] = ["admin", "node", "infer"];
/** Rotas nativas do POM que um manifesto pode fornecer como substituição. */
const POM_PLUGIN_EXTENSION_ROUTES = new Set(["/logs", "/estatisticas"]);

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

function effectiveRouteProfiles(route: PluginRoute): Profile[] {
  return route.roles !== undefined
    ? [...route.roles]
    : route.profiles !== undefined
      ? [...route.profiles]
      : [...ALL_PROFILES];
}

/**
 * Canonicaliza um caminho sem decodificar separadores nem aceitar segmentos
 * que possam sair do escopo do plugin. Caminhos de localização podem conter
 * query/hash; valores vindos do manifesto não podem.
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

/** Aplica o namespace ao caminho declarado por um plugin, sem duplicá-lo. */
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
      ...(candidate.roles !== undefined ? { roles: [...(candidate.roles as Profile[])] } : {}),
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
    const roles = candidate.roles !== undefined
      ? [...(candidate.roles as Profile[])]
      : candidate.profiles !== undefined
        ? [...(candidate.profiles as Profile[])]
        : [...ALL_PROFILES];
    menuIds.add(candidate.id);
    menu.push({
      code,
      id: candidate.id,
      label: { ...(candidate.label as Record<string, string>) },
      to,
      roles,
      ...(isNonEmptyString(iconImage) ? { iconImage } : {}),
      ...(candidate.profiles !== undefined ? { profiles: [...(candidate.profiles as Profile[])] } : {}),
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
    });
  }

  const plugin: ActivePluginUi = {
    code,
    ...(value.version !== undefined ? { version: value.version as string | null } : {}),
    manifest: {
      schema: "pom-plugin-ui/v1",
      plugin_code: code,
      ...(isNonEmptyString(iconImage) ? { icon_image: iconImage } : {}),
      menu: rawMenu,
      routes: rawRoutes,
      screens,
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

/** Troca o registro. Manifestos inválidos ou ambíguos são ignorados. */
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

/** A rota de plugin mais específica que cobre este caminho, ou `null`. */
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

function profilesForPluginPath(
  plugin: RegisteredPlugin,
  path: string,
  route: ResolvedPluginRoute,
): readonly Profile[] {
  const applicableMenus = plugin.menu.filter((item) => isUnder(path, item.to));
  if (applicableMenus.length === 0) return effectiveRouteProfiles(route.route);
  const menuRoles = new Set(applicableMenus.flatMap((item) => item.roles));
  return effectiveRouteProfiles(route.route).filter((profile) => menuRoles.has(profile));
}

/** Perfis que um plugin admite numa rota dele; `null` se a rota não é de plugin. */
export function pluginRouteProfiles(path: string): readonly Profile[] | null {
  const route = resolvePluginRoute(path);
  if (!route) return null;
  const plugin = registry.find(({ plugin }) => plugin.code === route.code);
  if (!plugin) return null;
  const pathname = normalizePathname(path, true);
  return pathname ? profilesForPluginPath(plugin, pathname, route) : null;
}

export function pluginMenuItems(profile: Profile): RegisteredMenuItem[] {
  return registry.flatMap((plugin) => plugin.menu.filter((item) => {
    if (!item.roles.includes(profile)) return false;
    const route = plugin.routes
      .filter(({ route }) => isUnder(item.to, route.path))
      .reduce<ResolvedPluginRoute | null>(
        (best, candidate) => !best || candidate.route.path.length > best.route.path.length ? candidate : best,
        null,
      );
    return route !== null && profilesForPluginPath(plugin, item.to, route).includes(profile);
  }));
}

export const pluginLabelKey = (code: string, id: string) => `plugin:${code}:${id}`;

/** Rótulo de um item de menu de plugin no idioma ativo; `en` é o piso. */
export function pluginLabel(key: string, locale: string): string | null {
  const [, code, id] = key.split(":");
  const item = registry
    .find((plugin) => plugin.plugin.code === code)
    ?.plugin.manifest.menu.find((entry) => entry.id === id);
  if (!item) return null;
  return item.label[locale] ?? item.label[locale.split("-")[0]] ?? item.label.en ?? null;
}

export const pluginAssetUrl = (code: string, asset: string) =>
  `/api/ui/plugins/${encodeURIComponent(code)}/assets/${asset}`;
