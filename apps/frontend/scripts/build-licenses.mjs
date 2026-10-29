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
 * package.json, package-lock.json, Cargo.toml or Cargo.lock, then commit this
 * script, `src/lib/licenses.generated.ts` and `THIRD-PARTY-NOTICES.txt`.
 *
 * `THIRD-PARTY-NOTICES.txt` (repository root) is the same catalog with the
 * full text of each license file the packages carry (LICENSE, COPYING,
 * NOTICE...), identical texts printed once, plus POM's own license and the
 * NVIDIA terms. Every release package carries it: the Windows zip, the
 * AppImage, the macOS app and the Docker image. The texts come from the
 * installed `node_modules` and the Cargo registry, so run this where both
 * are present (`npm ci` and a Cargo build or `cargo fetch`).
 */
import { execFileSync } from "node:child_process";
import { existsSync, readdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const frontend = join(here, "..");
const repo = join(frontend, "..", "..");
const packageJsonPath = join(frontend, "package.json");
const packageLockPath = join(frontend, "package-lock.json");
const outputPath = join(frontend, "src", "lib", "licenses.generated.ts");
const noticesPath = join(repo, "THIRD-PARTY-NOTICES.txt");

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
      dir: join(frontend, path),
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
    dir: dirname(pkg.manifest_path),
  }));
}

/** A crate from POM's own repositories (the llama.cpp engine wrapper) that declares no license is POM itself. */
function ownPackage(pkg) {
  return !pkg.license && !pkg.license_file && (pkg.source ?? "").includes("github.com/pieceofmindcloud/");
}

const LLAMA_CPP_LICENSE = `MIT License

Copyright (c) 2023-2026 The ggml authors

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.`;

const NVIDIA_NOTICE = `The CUDA builds of POM include NVIDIA CUDA runtime components: the CUDA
runtime (cudart), cuBLAS and cuBLASLt. They are linked into the Linux
AppImage's executable, shipped as DLLs beside pom.exe in the Windows
cuda-vulkan package, and taken from the NVIDIA CUDA base image in the Docker
image. They are NVIDIA's property, are not covered by POM's MIT license, and
are distributed under, and their use is subject to, the NVIDIA CUDA Toolkit
End User License Agreement:

  https://docs.nvidia.com/cuda/eula/

The Docker image is derived from an NVIDIA CUDA container image and keeps
NVIDIA's license at /NGC-DL-CONTAINER-LICENSE (the NVIDIA Deep Learning
Container License). The NVIDIA driver (libcuda) is not part of POM: it is
installed on the machine that runs it. You may not reverse engineer,
decompile or disassemble these NVIDIA components, except as the EULA allows.`;

/** Compiled into the node binary or shipped beside it, outside npm and crates.io metadata. */
const NATIVE = [
  {
    name: "llama.cpp / ggml",
    version: "fork pinned by pom-plugin-llama.cpp",
    license: "MIT",
    repository: "https://github.com/ggml-org/llama.cpp",
    ecosystem: "native",
    text: LLAMA_CPP_LICENSE,
  },
  {
    name: "NVIDIA CUDA runtime libraries (cudart, cuBLAS) and CUDA container image",
    version: "CUDA builds and the Docker image",
    license: "NVIDIA CUDA EULA / NVIDIA Deep Learning Container License",
    repository: "https://docs.nvidia.com/cuda/eula/",
    ecosystem: "native",
    text: NVIDIA_NOTICE,
  },
];

const entries = [...frontendEntries(), ...cargoEntries(), ...NATIVE]
  .sort((a, b) => a.ecosystem.localeCompare(b.ecosystem) || a.name.localeCompare(b.name));

/** License, copyright and notice files at the root of a package directory. */
const LICENSE_FILE = /^(licen[cs]e|copying|copyright|notice|unlicense|authors)([._-].*)?$/i;

function licenseTexts(dir) {
  if (!dir || !existsSync(dir)) return [];
  return readdirSync(dir)
    .filter((name) => LICENSE_FILE.test(name) && !/\.(rs|js|ts|json|html|py|sh)$/i.test(name))
    .sort()
    .flatMap((name) => {
      const path = join(dir, name);
      try {
        if (!statSync(path).isFile()) return [];
        return [{ name, text: readFileSync(path, "utf8").replace(/\r\n/g, "\n").trim() }];
      } catch {
        return [];
      }
    });
}

function noticesFile() {
  const rule = "=".repeat(78);
  const thin = "-".repeat(78);
  const texts = new Map();
  const missing = [];
  for (const entry of entries) {
    const found = entry.text ? [{ name: "LICENSE", text: entry.text }] : licenseTexts(entry.dir);
    if (found.length === 0) missing.push(entry);
    for (const { text } of found) {
      const key = text.replace(/\s+/g, " ");
      const holder = texts.get(key) ?? { text, packages: [] };
      holder.packages.push(`${entry.name} ${entry.version}`);
      texts.set(key, holder);
    }
  }
  const lines = [
    "THIRD-PARTY NOTICES",
    "",
    "POM is licensed under the MIT License (LICENSE, reproduced at the end of this",
    "file). It includes the third-party software listed below, each under its own",
    "license. This file is generated by apps/frontend/scripts/build-licenses.mjs from",
    "the packages a POM build ships; the same list is on the Settings > About screen.",
    "",
    rule,
    `PACKAGES (${entries.length})`,
    rule,
    "",
    ...entries.map((entry) => `${entry.name} ${entry.version} - ${entry.license} - ${entry.repository}`),
    "",
    rule,
    "LICENSE TEXTS",
    rule,
  ];
  for (const { text, packages } of texts.values()) {
    lines.push("", thin, `Used by: ${packages.join(", ")}`, thin, "", text);
  }
  if (missing.length > 0) {
    lines.push("", rule, "PACKAGES WITHOUT A LICENSE FILE", rule, "",
      "These packages declare their license but ship no license file; the standard",
      "text of the license named applies (the MIT, Apache-2.0, BSD, ISC and Zlib texts",
      "are reproduced above, in other packages' license files), and the source is at",
      "the address given.", "",
      ...missing.map((entry) => `${entry.name} ${entry.version} - ${entry.license} - ${entry.repository}`));
  }
  lines.push("", rule, "POM", rule, "", readFileSync(join(repo, "LICENSE"), "utf8").trim(), "");
  return { body: lines.join("\n"), texts: texts.size, missing: missing.length };
}

const notices = noticesFile();
writeFileSync(noticesPath, notices.body);
console.log(`build-licenses: THIRD-PARTY-NOTICES.txt with ${notices.texts} license texts (${notices.missing} packages ship none)`);

const catalog = entries.map(({ name, version, license, repository, ecosystem }) => ({ name, version, license, repository, ecosystem }));

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

export const THIRD_PARTY_LICENSES: readonly LicenseEntry[] = ${JSON.stringify(catalog, null, 2)};
`;

writeFileSync(outputPath, generated);
console.log(`build-licenses: ${entries.length} dependencies catalogued in src/lib/licenses.generated.ts`);
