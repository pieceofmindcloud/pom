/**
 * The `sandbox` section of a plugin's release manifest as the Plugins screen
 * reads it (`enterprise_protocol::sandbox` in the node). `null` is a legacy
 * plugin, which runs without a sandbox.
 */
import type { PluginSandbox } from "./types";

type Translate = (key: string, values?: Record<string, string | number>) => string;

const NETWORK_ORDER = { loopback: 0, internet: 1 } as const;
const LISTEN_ORDER = { loopback: 0, network: 1 } as const;

/**
 * Whether moving from the installed release's sandbox to the next one asks for
 * more, so the operator approves it again. Leaving the sandbox widens;
 * entering it does not. Mirrors `enterprise_protocol::sandbox_widens`.
 */
export function sandboxWidens(installed: PluginSandbox | null | undefined, next: PluginSandbox | null | undefined): boolean {
  if (!next) return Boolean(installed);
  if (!installed) return false;
  return (NETWORK_ORDER[next.network] ?? 0) > (NETWORK_ORDER[installed.network] ?? 0)
    || (LISTEN_ORDER[next.listen] ?? 0) > (LISTEN_ORDER[installed.listen] ?? 0)
    || (next.gpu && !installed.gpu)
    || (next.read_paths ?? []).some((path) => !(installed.read_paths ?? []).includes(path));
}

/** One line per permission the sandbox asks for. */
export function sandboxPermissions(sandbox: PluginSandbox, t: Translate): string[] {
  const lines = [t(sandbox.network === "internet" ? "plugins.sandboxInternet" : "plugins.sandboxLoopback")];
  if (sandbox.listen === "network") lines.push(t("plugins.sandboxListenNetwork"));
  if (sandbox.gpu) lines.push(t("plugins.sandboxGpu"));
  if (sandbox.read_paths?.length) lines.push(t("plugins.sandboxReadPaths", { paths: sandbox.read_paths.join(", ") }));
  return lines;
}

/** The card badge's tooltip: what the sandbox grants, or why a legacy plugin has none. */
export function sandboxSummary(
  sandbox: PluginSandbox | null | undefined,
  enforced: boolean,
  t: Translate,
  level?: "baseline" | "pid_namespace" | null,
): string {
  if (!sandbox) return t("plugins.unsandboxedHint");
  if (!enforced) return [t("plugins.sandboxDeclaredHint"), ...sandboxPermissions(sandbox, t)].join("\n");
  return [
    t("plugins.sandboxedHint"),
    t(level === "pid_namespace" ? "plugins.sandboxLevelPid" : "plugins.sandboxLevelBaseline"),
    t("plugins.sandboxNetworkOpen"),
    ...sandboxPermissions(sandbox, t),
  ].join("\n");
}
