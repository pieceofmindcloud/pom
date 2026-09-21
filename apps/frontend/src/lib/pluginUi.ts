/**
 * Interface fornecida por plugins (contrato `pom-plugin-ui/v1`).
 *
 * O nó valida o manifesto e serve `GET /api/ui/plugins/ui`; este módulo só
 * guarda o que o navegador recebeu e responde três perguntas genéricas, sem
 * conhecer nenhum plugin: que itens de menu entram, que rota é de plugin e que
 * perfis entram nela. `access.ts` e `nav.ts` consultam este registro, então o
 * menu e o roteador continuam lendo uma tabela só. Rotas sem plugin caem nos
 * fallbacks de propaganda que o POM mantém (ver `EnterpriseFeatureShowcase`).
 */
import type { Profile } from "./profile";

export interface PluginMenuItem {
  id: string;
  label: Record<string, string>;
  icon?: string | null;
  to: string;
  group?: string | null;
  profiles: Profile[];
}

export interface PluginRoute {
  path: string;
  screen: string;
  profiles: Profile[];
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
  menu: PluginMenuItem[];
  routes: PluginRoute[];
  screens: Record<string, PluginScreenSpec>;
  /** idioma -> asset do catálogo de textos do plugin */
  i18n?: Record<string, string>;
  assets: string[];
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

let registry: ActivePluginUi[] = [];

/** Troca o registro. Chamado pelo provider, antes de qualquer filho ler. */
export function setPluginUi(plugins: readonly ActivePluginUi[]): void {
  registry = [...plugins];
}

export function pluginUiSnapshot(): readonly ActivePluginUi[] {
  return registry;
}

function isUnder(path: string, prefix: string): boolean {
  const pathname = path.split(/[?#]/, 1)[0];
  return pathname === prefix || pathname.startsWith(prefix + "/");
}

/** A rota de plugin mais específica que cobre este caminho, ou `null`. */
export function resolvePluginRoute(path: string): ResolvedPluginRoute | null {
  let best: ResolvedPluginRoute | null = null;
  for (const plugin of registry) {
    for (const route of plugin.manifest.routes) {
      const screen = plugin.manifest.screens[route.screen];
      if (!screen || !isUnder(path, route.path)) continue;
      if (best && best.route.path.length >= route.path.length) continue;
      best = { code: plugin.code, route, screen, version: plugin.version ?? null };
    }
  }
  return best;
}

/** Perfis que um plugin admite numa rota dele; `null` se a rota não é de plugin. */
export function pluginRouteProfiles(path: string): readonly Profile[] | null {
  return resolvePluginRoute(path)?.route.profiles ?? null;
}

export function pluginMenuItems(profile: Profile): Array<PluginMenuItem & { code: string }> {
  return registry.flatMap((plugin) =>
    plugin.manifest.menu
      .filter((item) => item.profiles.includes(profile))
      .map((item) => ({ ...item, code: plugin.code })),
  );
}

export const pluginLabelKey = (code: string, id: string) => `plugin:${code}:${id}`;

/** Rótulo de um item de menu de plugin no idioma ativo; `en` é o piso. */
export function pluginLabel(key: string, locale: string): string | null {
  const [, code, id] = key.split(":");
  const item = registry
    .find((plugin) => plugin.code === code)
    ?.manifest.menu.find((entry) => entry.id === id);
  if (!item) return null;
  return item.label[locale] ?? item.label[locale.split("-")[0]] ?? item.label.en ?? null;
}

export const pluginAssetUrl = (code: string, asset: string) =>
  `/api/ui/plugins/${encodeURIComponent(code)}/assets/${asset}`;
