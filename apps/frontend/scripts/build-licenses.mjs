/**
 * Generates the dependency catalog shown by Settings > About.
 *
 * The catalog lists everything a POM build ships, because the permissive
 * licenses ask for their notice wherever the code is distributed: every
 * frontend package the production bundle can include (the package-lock
 * entries that are not development-only, transitive ones included), every
 * crate a workspace member reaches through normal or build dependencies (on
 * any target), and the native components compiled in that no package manager
 * describes (`NATIVE`). POM's own crates from its private repositories are
 * POM itself and are left out. Re-run `npm run licenses` after changing
 * package.json, package-lock.json, Cargo.toml or Cargo.lock, then commit both
 * this script and `src/lib/licenses.generated.ts`.
 */
import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const frontend = join(here, "..");
const repo = join(frontend, "..", "..");
const packageJsonPath = join(frontend, "package.json");
const packageLockPath = join(frontend, "package-lock.json");
const outputPath = join(frontend, "src", "lib", "licenses.generated.ts");

function readJson(path) {
  return JSON.parse(readFileSync(path, "utf8"));
}

function repositoryUrl(value, fallback) {
  const raw = typeof value === "string" ? value : value?.url;
  if (!raw) return fallback;
  let url = raw.trim();
  if (url.startsWith("git+")) url = url.slice(4);
  if (url.startsWith("git@github.com:")) url = `https://github.com/${url.slice(15)}`;
  if (url.startsWith("git://")) url = `https://${url.slice(6)}`;
  if (url.startsWith("github:")) url = `https://github.com/${url.slice(7)}`;
  if (!/^https?:\/\//.test(url)) url = `https://github.com/${url.replace(/^\/+/, "")}`;
  return url.replace(/\.git$/, "").replace(/\/$/, "");
}

function packageLicense(manifest) {
  if (typeof manifest.license === "string" && manifest.license.trim()) return manifest.license;
  if (Array.isArray(manifest.licenses)) {
    const types = manifest.licenses
      .map((license) => typeof license === "string" ? license : license?.type)
      .filter(Boolean);
    if (types.length > 0) return types.join(" OR ");
  }
  return "Not declared";
}

function frontendEntries() {
  const packageLock = readJson(packageLockPath);
  const entries = new Map();
  for (const [path, lockPackage] of Object.entries(packageLock.packages ?? {})) {
    if (!path || lockPackage.dev || lockPackage.link) continue;
    const name = path.slice(path.lastIndexOf("node_modules/") + "node_modules/".length);
    const manifestPath = join(frontend, path, "package.json");
    let manifest = {};
    try {
      manifest = readJson(manifestPath);
    } catch {
      // An optional package for another platform is not installed here.
    }
    const version = lockPackage.version ?? manifest.version;
    const key = `${name}@${version}`;
    if (entries.has(key)) continue;
    entries.set(key, {
      name,
      version,
      license: lockPackage.license ?? packageLicense(manifest),
      repository: repositoryUrl(
        manifest.repository,
        manifest.homepage ?? `https://www.npmjs.com/package/${name}`,
      ),
      ecosystem: "frontend",
    });
  }
  return [...entries.values()];
}

function cargoEntries() {
  const metadata = JSON.parse(execFileSync("cargo", ["metadata", "--format-version", "1"], {
    cwd: repo,
    encoding: "utf8",
    maxBuffer: 64 * 1024 * 1024,
  }));
  const workspaceMembers = new Set(metadata.workspace_members);
  const packages = new Map(metadata.packages.map((pkg) => [pkg.id, pkg]));
  const nodes = new Map(metadata.resolve.nodes.map((node) => [node.id, node]));
  const shipped = new Map();
  const seen = new Set();
  const pending = [...workspaceMembers];
  while (pending.length > 0) {
    const id = pending.pop();
    if (seen.has(id)) continue;
    seen.add(id);
    for (const dependency of nodes.get(id)?.deps ?? []) {
      const kinds = dependency.dep_kinds.map(({ kind }) => kind ?? "normal");
      if (kinds.length > 0 && kinds.every((kind) => kind === "dev")) continue;
      pending.push(dependency.pkg);
      const pkg = packages.get(dependency.pkg);
      if (!pkg || workspaceMembers.has(dependency.pkg) || ownPackage(pkg)) continue;
      shipped.set(`${pkg.name}@${pkg.version}`, pkg);
    }
  }

  return [...shipped.values()].map((pkg) => ({
    name: pkg.name,
    version: pkg.version,
    license: pkg.license ?? (pkg.license_file ? `license-file: ${pkg.license_file}` : "Not declared"),
    repository: repositoryUrl(pkg.repository, `https://crates.io/crates/${pkg.name}`),
    ecosystem: "rust",
  }));
}

/** A crate from POM's own repositories (the llama.cpp engine wrapper) that declares no license is POM itself. */
function ownPackage(pkg) {
  return !pkg.license && !pkg.license_file && (pkg.source ?? "").includes("github.com/pieceofmindcloud/");
}

/** Compiled into the node binary or shipped beside it, outside npm and crates.io metadata. */
const NATIVE = [
  {
    name: "llama.cpp / ggml",
    version: "fork pinned by pom-plugin-llama.cpp",
    license: "MIT",
    repository: "https://github.com/ggml-org/llama.cpp",
    ecosystem: "native",
  },
  {
    name: "NVIDIA CUDA runtime libraries (cudart, cuBLAS) and CUDA container image",
    version: "CUDA builds and the Docker image",
    license: "NVIDIA CUDA EULA / NVIDIA Deep Learning Container License",
    repository: "https://docs.nvidia.com/cuda/eula/",
    ecosystem: "native",
  },
];

const entries = [...frontendEntries(), ...cargoEntries(), ...NATIVE]
  .sort((a, b) => a.ecosystem.localeCompare(b.ecosystem) || a.name.localeCompare(b.name));

const generated = `/* eslint-disable */
// Generated by apps/frontend/scripts/build-licenses.mjs. Do not edit by hand.
export type LicenseEcosystem = "frontend" | "rust" | "native";

export type LicenseEntry = {
  name: string;
  version: string;
  license: string;
  repository: string;
  ecosystem: LicenseEcosystem;
};

export const POM_LICENSE = {
  status: "published",
  label: "MIT",
} as const;

export const THIRD_PARTY_LICENSES: readonly LicenseEntry[] = ${JSON.stringify(entries, null, 2)};
`;

writeFileSync(outputPath, generated);
console.log(`build-licenses: ${entries.length} dependencies catalogued in src/lib/licenses.generated.ts`);
