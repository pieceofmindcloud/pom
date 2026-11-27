import { describe, expect, it } from "vitest";
import { sandboxPermissions, sandboxWidens } from "./pluginSandbox";
import type { PluginSandbox } from "./types";

const narrow: PluginSandbox = { network: "loopback", listen: "loopback", gpu: false };

describe("plugin sandbox", () => {
  it("widens when an update asks for more or leaves the sandbox, as the node decides", () => {
    const wider: PluginSandbox[] = [
      { ...narrow, network: "internet" },
      { ...narrow, listen: "network" },
      { ...narrow, gpu: true },
      { ...narrow, read_paths: ["/opt/x"] },
    ];
    expect(sandboxWidens(null, null)).toBe(false);
    expect(sandboxWidens(null, wider[0])).toBe(false);
    expect(sandboxWidens(narrow, null)).toBe(true);
    for (const next of wider) {
      expect(sandboxWidens(narrow, next)).toBe(true);
      expect(sandboxWidens(next, narrow)).toBe(false);
      expect(sandboxWidens(next, next)).toBe(false);
    }
  });

  it("lists one line per permission", () => {
    const t = (key: string, values?: Record<string, string | number>) => `${key}${values ? JSON.stringify(values) : ""}`;
    expect(sandboxPermissions(narrow, t)).toEqual(["plugins.sandboxLoopback"]);
    expect(sandboxPermissions({ network: "internet", listen: "network", gpu: true, read_paths: ["/a", "/b"] }, t)).toEqual([
      "plugins.sandboxInternet",
      "plugins.sandboxListenNetwork",
      "plugins.sandboxGpu",
      'plugins.sandboxReadPaths{"paths":"/a, /b"}',
    ]);
  });
});
