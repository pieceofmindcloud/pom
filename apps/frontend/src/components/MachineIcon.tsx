import clsx from "clsx";
import { HelpCircle, Laptop, Monitor, Server } from "lucide-react";
import type { SystemLoad } from "../lib/types";

/**
 * What kind of machine this is, from what the node reports about itself
 * (`src/sysmon.rs`): the OS it runs and whether it has a discrete GPU. The
 * cluster is deliberately heterogeneous, so telling a laptop from a GPU box at
 * a glance is part of reading the pipeline.
 */
/**
 * O nome da placa como cabe num rótulo.
 *
 * O prefixo do fabricante sai porque ele já está no contexto e come a largura
 * que interessa - o modelo. A Radeon vem do sysfs sem nome comercial (só o id
 * PCI), então o que se pode limpar ali é diferente do que se limpa numa NVIDIA.
 */
export function gpuLabel(gpu: { name: string; vendor?: string }) {
  if (gpu.vendor === "amd") return gpu.name.replace(/^AMD\s+/i, "");
  return gpu.name.replace(/^NVIDIA\s+/i, "");
}

/**
 * O mesmo nome, com o índice quando a máquina tem duas placas iguais.
 *
 * Uma caixa com duas RTX 5060 Ti mostrava dois rótulos idênticos - duas
 * temperaturas, duas barras de VRAM, e nada dizendo qual era qual. O índice é
 * o número pelo qual a `pom.json` e o painel já falam de uma placa, então é
 * ele que entra, e só onde é preciso: numa máquina de placa única o sufixo
 * seria ruído.
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
  if (!system) return { label: "Desconhecida", icon: HelpCircle, accent: "faint" as const };
  const os = system.os.toLowerCase();
  if (system.gpu_kind === "nvidia" || system.gpu_kind === "amd" || system.gpu_kind === "mixed") {
    // Numa máquina mista o rótulo conta quantas placas há, em vez de eleger
    // uma: o número é o que muda a decisão de quem implanta.
    if (system.gpu_kind === "mixed") {
      return {
        label: `${system.gpus.length} GPUs · NVIDIA + AMD`,
        icon: Server,
        accent: "lime" as const,
      };
    }
    const marca = system.gpu_kind === "amd" ? "AMD" : "NVIDIA";
    const gpu = system.gpus[0] ? gpuLabel(system.gpus[0]) : undefined;
    return {
      label: gpu ? `${marca} · ${gpu}` : marca,
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
