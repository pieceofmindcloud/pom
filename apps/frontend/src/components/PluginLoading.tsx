/**
 * The screen POM shows while a plugin screen is still opening: the same puzzle
 * of pieces the Chat draws when the network serves no model (`MissingPiece`),
 * with its fourth piece hovering over the slot - the plugin's piece is on its
 * way. It replaces both the empty gap of a module still being imported and the
 * advertising card of a plugin that has not answered yet.
 */
import { useI18n } from "../i18n/provider";
import { pluginDisplayName } from "../lib/pluginUi";
import { MissingPiece } from "./MissingPiece";

export function PluginLoading({ code }: { code?: string }) {
  const { t, locale } = useI18n();
  return (
    <main className="flex min-h-full items-center justify-center bg-ink px-5 py-10">
      <MissingPiece
        variant="loading"
        title={code ? t("plugins.loadingNamed", { name: pluginDisplayName(code, locale) }) : t("plugins.loading")}
        detail={t("plugins.loadingHint")}
      />
    </main>
  );
}