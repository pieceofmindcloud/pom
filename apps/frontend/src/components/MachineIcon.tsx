import clsx from "clsx";
import { HelpCircle, Laptop, Monitor, Server } from "lucide-react";
import type { SystemLoad } from "../lib/types";
import { formatMessage, getActiveLocale } from "../i18n/locales";

/**
 * What kind of machine this is, from what the node reports about itself
 * (`src/utils/sysmon.rs`): the OS it runs and whether it has a discrete GPU. The
 * cluster is deliberately heterogeneous, so telling a laptop from a GPU box at
 * a glance is part of reading the pipeline.
 */
/**
 * The card's name as it fits in a label.
 *
 * The manufacturer prefix goes away because it is already in context and eats the width
 * that matters - the model. The Radeon comes from sysfs with no commercial name (only the PCI
 * id), so what can be cleaned up there is different from what is cleaned up on an NVIDIA.
 */
export function gpuLabel(gpu: { name: string; vendor?: string }) {
  if (gpu.vendor === "amd") return gpu.name.replace(/^AMD\s+/i, "");
  return gpu.name.replace(/^NVIDIA\s+/i, "");
}

/**
 * The same name, with the index when the machine has two identical cards.
 *
 * A box with two RTX 5060 Ti showed two identical labels - two
 * temperatures, two VRAM bars, and nothing saying which was which. The index is
 * the number by which `pom.json` and the panel already refer to a card, so it is
 * the one that goes in, and only where needed: on a single-card machine the suffix
 * would be noise.
 */
export function gpuDisplayName(
  gpu: { index: number; name: string; vendor?: string },
  all: { name: string }[],
): string {
  const label = gpuLabel(gpu);
  const repeated = all.filter((other) => other.name === gpu.name).length > 1;
  return repeated ? `${label} #${gpu.index}` : label;
}

export function machineKind(system?: SystemLoad | null) {
  if (!system) return { label: formatMessage(getActiveLocale(), "machine.kindUnknown"), icon: HelpCircle, accent: "faint" as const };
  const os = system.os.toLowerCase();
  if (system.gpu_kind === "nvidia" || system.gpu_kind === "amd" || system.gpu_kind === "mixed") {
    // On a mixed machine the label counts how many cards there are, instead of picking
    // one: the number is what changes the decision of whoever deploys.
    if (system.gpu_kind === "mixed") {
      return {
        label: `${system.gpus.length} GPUs · NVIDIA + AMD`,
        icon: Server,
        accent: "lime" as const,
      };
    }
    const brand = system.gpu_kind === "amd" ? "AMD" : "NVIDIA";
    const gpu = system.gpus[0] ? gpuLabel(system.gpus[0]) : undefined;
    return {
      label: gpu ? `${brand} · ${gpu}` : brand,
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
