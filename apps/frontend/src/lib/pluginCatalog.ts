export const PUBLIC_PLUGIN_CATALOG_URL = "https://license.pieceofmind.cloud/v1/plugins";
const PUBLIC_PLUGIN_ORIGIN = new URL(PUBLIC_PLUGIN_CATALOG_URL).origin;
const MAX_SCREENSHOTS = 4;

export interface PublicPluginRelease {
  os: string;
  arch: string;
  channel: string;
  version: string;
  published_at: string | null;
}

export interface PublicPluginPrice {
  amount: number | null;
  currency: string | null;
  interval: string | null;
}

export interface PublicPluginCatalogEntry {
  code: string;
  display_name: string | null;
  description: string | null;
  description_pt_br: string | null;
  requires_license: boolean | null;
  icon_url: string | null;
  screenshot_urls: string[];
  pricing: {
    monthly: PublicPluginPrice | null;
    annual: PublicPluginPrice | null;
  };
  releases: PublicPluginRelease[];
  /** Shown first in the store (destaque). */
  featured: boolean;
  /** Made by the POM team; false for a community plugin, null from an older license server. */
  pom_team: boolean | null;
  /** Free plugin installed from this public GitHub repository instead of the license server. */
  github_url: string | null;
}

export interface PublicPluginCatalog {
  plugins: PublicPluginCatalogEntry[];
}

/** Pick a release only when the catalog identifies the current node target. */
export function releaseForTarget(
  releases: PublicPluginRelease[],
  os: string | null | undefined,
  arch: string | null | undefined,
): PublicPluginRelease | null {
  if (!os || !arch) return null;
  const targetOs = os.trim().toLowerCase();
  const targetArch = arch.trim().toLowerCase();
  if (!targetOs || !targetArch) return null;
  return releases.find((release) =>
    release.os.toLowerCase() === targetOs && release.arch.toLowerCase() === targetArch) ?? null;
}

type UnknownRecord = Record<string, unknown>;

function isRecord(value: unknown): value is UnknownRecord {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function nonEmptyString(value: unknown): string | null {
  return typeof value === "string" && value.trim() ? value.trim() : null;
}

function publicImageUrl(value: unknown): string | null {
  const candidate = nonEmptyString(value);
  if (!candidate) return null;

  try {
    const url = new URL(candidate);
    if (url.protocol !== "https:" || url.origin !== PUBLIC_PLUGIN_ORIGIN || url.username || url.password) return null;
    return url.toString();
  } catch {
    return null;
  }
}

function validRelease(value: unknown): PublicPluginRelease | null {
  if (!isRecord(value)) return null;
  const os = nonEmptyString(value.os);
  const arch = nonEmptyString(value.arch);
  const channel = nonEmptyString(value.channel);
  const version = nonEmptyString(value.version);
  const publishedAt = value.published_at;
  const validPublishedAt = publishedAt === null
    || (typeof publishedAt === "string" && Number.isFinite(Date.parse(publishedAt)));

  if (!os || !arch || !channel || !version || !validPublishedAt) return null;

  return {
    os,
    arch,
    channel,
    version,
    published_at: publishedAt as string | null,
  };
}

function compareReleaseDates(left: PublicPluginRelease, right: PublicPluginRelease): number {
  const leftTime = left.published_at === null ? Number.NEGATIVE_INFINITY : Date.parse(left.published_at);
  const rightTime = right.published_at === null ? Number.NEGATIVE_INFINITY : Date.parse(right.published_at);
  return rightTime - leftTime || left.os.localeCompare(right.os) || left.arch.localeCompare(right.arch);
}

function publicPrice(value: unknown): PublicPluginPrice | null {
  if (!isRecord(value)) return null;
  const amount = typeof value.amount === "number" && Number.isFinite(value.amount) && value.amount >= 0
    ? value.amount
    : null;
  const currency = nonEmptyString(value.currency);
  const interval = nonEmptyString(value.interval);
  if (amount === null || !currency || !/^[a-z]{3}$/i.test(currency)) return null;
  return { amount, currency: currency.toUpperCase(), interval };
}

function githubUrl(value: unknown): string | null {
  const candidate = nonEmptyString(value);
  if (!candidate) return null;
  try {
    const url = new URL(candidate);
    const path = url.pathname.split("/").filter(Boolean);
    if (url.protocol !== "https:" || url.hostname !== "github.com" || path.length !== 2 || url.username || url.password) return null;
    return `https://github.com/${path[0]}/${path[1]}`;
  } catch {
    return null;
  }
}

function catalogEntry(value: unknown): PublicPluginCatalogEntry | null {
  if (!isRecord(value)) return null;
  const code = nonEmptyString(value.code);
  if (!code) return null;

  const releases = Array.isArray(value.releases)
    ? value.releases.map(validRelease).filter((release): release is PublicPluginRelease => release !== null)
    : [];
  releases.sort(compareReleaseDates);

  const pricing = isRecord(value.pricing) ? value.pricing : {};
  const requiresLicense = typeof value.requires_license === "boolean" ? value.requires_license : null;
  if (releases.length === 0) return null;

  return {
    code,
    display_name: nonEmptyString(value.display_name),
    description: nonEmptyString(value.description),
    description_pt_br: nonEmptyString(value.description_pt_br),
    requires_license: requiresLicense,
    icon_url: publicImageUrl(value.icon_url),
    screenshot_urls: Array.isArray(value.screenshot_urls)
      ? value.screenshot_urls.map(publicImageUrl).filter((url): url is string => url !== null).slice(0, MAX_SCREENSHOTS)
      : [],
    pricing: {
      monthly: publicPrice(pricing.monthly),
      annual: publicPrice(pricing.annual),
    },
    releases,
    featured: value.featured === true,
    pom_team: typeof value.pom_team === "boolean" ? value.pom_team : null,
    github_url: githubUrl(value.github_url),
  };
}

export function normalizePublicPluginCatalog(value: unknown): PublicPluginCatalog {
  if (!isRecord(value) || !Array.isArray(value.plugins)) {
    throw new Error("Invalid public plugin catalog");
  }

  const plugins: PublicPluginCatalogEntry[] = [];
  const seen = new Set<string>();
  for (const item of value.plugins) {
    const entry = catalogEntry(item);
    if (!entry || seen.has(entry.code)) continue;
    seen.add(entry.code);
    plugins.push(entry);
  }
  return { plugins };
}

/** Public storefront read. It intentionally bypasses the authenticated node API client. */
export async function fetchPublicPluginCatalog(signal?: AbortSignal): Promise<PublicPluginCatalog> {
  const response = await fetch(PUBLIC_PLUGIN_CATALOG_URL, {
    credentials: "omit",
    signal,
  });
  if (!response.ok) throw new Error(`HTTP ${response.status}`);
  return normalizePublicPluginCatalog(await response.json());
}
