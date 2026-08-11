import clsx from "clsx";
import { HelpCircle, Laptop, Monitor, Server } from "lucide-react";
import type { SystemLoad } from "../lib/types";

/**
 * What kind of machine this is, from what the node reports about itself
 * (`src/sysmon.rs`): the OS it runs and whether it has a discrete GPU. The
 * cluster is deliberately heterogeneous, so telling a laptop from a GPU box at
 * a glance is part of reading the pipeline.
 */
export function machineKind(system?: SystemLoad | null) {
  if (!system) return { label: "Desconhecida", icon: HelpCircle, accent: "faint" as const };
  const os = system.os.toLowerCase();
  if (system.gpu_kind === "nvidia") {
    const gpu = system.gpus[0]?.name?.replace(/^NVIDIA\s+/i, "");
    return {
      label: gpu ? `NVIDIA · ${gpu}` : "NVIDIA",
      icon: Server,
      accent: "lime" as const,
    };
  }
  if (os.includes("mac") || os.includes("darwin")) {
    return {
      label: system.arch.includes("aarch") ? "MacBook · Apple Silicon" : "Mac",
      icon: Laptop,
      accent: "sky" as const,
    };
  }
  if (os.includes("windows")) return { label: "Windows", icon: Monitor, accent: "sky" as const };
  return { label: `Linux · ${system.arch}`, icon: Server, accent: "muted" as const };
}

const accentClass = {
  lime: "text-lime",
  sky: "text-sky",
  muted: "text-muted",
  faint: "text-faint",
} as const;

export function MachineIcon({
  system,
  className,
  size = "md",
}: {
  system?: SystemLoad | null;
  className?: string;
  size?: "sm" | "md";
}) {
  const kind = machineKind(system);
  const Icon = kind.icon;
  return (
    <Icon
      className={clsx(
        size === "sm" ? "h-4 w-4" : "h-5 w-5",
        accentClass[kind.accent],
        className,
      )}
      aria-label={kind.label}
    />
  );
}
