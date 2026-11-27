/**
 * What every piece of the POM's "operating system" shares: an app's name and
 * icon, and the memory it uses as text. The apps are the plugins installed on
 * this machine (`/api/ui/plugins/ui`); the store, when it lists one, names and
 * draws it first.
 */
import { Blocks, Cpu } from "lucide-react";
import clsx from "clsx";
import { useState } from "react";
import { pluginAssetUrl, pluginDisplayName, type ActivePluginUi } from "../../lib/pluginUi";
import type { PublicPluginCatalogEntry } from "../../lib/pluginCatalog";

/** `314572800` -> `300 MB`; one decimal above 1 GB. */
export function formatMemory(bytes: number): string {
  const mb = bytes / (1024 * 1024);
  if (mb < 1024) return `${Math.round(mb)} MB`;
  return `${(mb / 1024).toFixed(1)} GB`;
}

/**
 * The app's name: the product name the store lists (the logs plugin is the
 * Business Pack, whatever its first menu entry says), else the manifest's
 * name or menu label, else its code, capitalized.
 */
export function displayName(plugin: ActivePluginUi, locale: string, entry?: PublicPluginCatalogEntry): string {
  if (entry?.display_name) return entry.display_name;
  const name = pluginDisplayName(plugin.code, locale);
  return name === plugin.code ? plugin.code.charAt(0).toUpperCase() + plugin.code.slice(1) : name;
}

/** The app icon: the store's icon, else the manifest's `app_icon` / `icon_image`, else a glyph on a tile. */
export function AppIcon({
  plugin,
  engine,
  dim,
  storeIcon,
  small = false,
}: {
  plugin: ActivePluginUi;
  engine: boolean;
  dim: boolean;
  storeIcon?: string | null;
  small?: boolean;
}) {
  const [broken, setBroken] = useState(false);
  const own = plugin.manifest.app_icon ?? plugin.manifest.icon_image;
  const image = storeIcon ?? (own ? pluginAssetUrl(plugin.code, own) : null);
  return (
    <span
      className={clsx(
        "relative flex items-center justify-center overflow-hidden shadow-[0_6px_16px_rgb(0_0_0/0.25)] ring-1 ring-white/10",
        small ? "h-10 w-10 rounded-[0.7rem]" : "h-16 w-16 rounded-[1.1rem]",
        image && !broken ? "bg-surface" : "bg-gradient-to-br from-iris/80 to-aqua/70 text-white",
        dim && "opacity-80 saturate-50",
      )}
    >
      {image && !broken ? (
        <img src={image} alt="" draggable={false} className="pointer-events-none h-full w-full object-cover" onError={() => setBroken(true)} />
      ) : engine ? (
        <Cpu size={small ? 18 : 28} />
      ) : (
        <Blocks size={small ? 18 : 28} />
      )}
    </span>
  );
}
