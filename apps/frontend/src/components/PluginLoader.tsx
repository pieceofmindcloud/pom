/**
 * POM's official loader and installer over a plugin screen, in the state the
 * plugin asked for (`lib/pluginLoader.ts`): the generic pieces while it opens
 * (`PluginLoading`), a bar with the percentage and what is happening while it
 * installs, its error with "Try again", and the puzzle completing on success.
 * The plugin only reports; POM draws every one of them the same way.
 */
import { RotateCcw } from "lucide-react";
import { useI18n } from "../i18n/provider";
import { publishLoaderRetry } from "../lib/pluginEvents";
import { applyLoaderCommand, type LoaderState } from "../lib/pluginLoader";
import { pluginDisplayName } from "../lib/pluginUi";
import { MissingPiece } from "./MissingPiece";
import { PluginLoading } from "./PluginLoading";
import { Button } from "./ui";

export function PluginLoader({ code, state }: { code: string; state: LoaderState | null }) {
  const { t, locale } = useI18n();
  if (!state || state.kind === "generic") return <PluginLoading code={code} title={state?.title} detail={state?.message} />;
  const name = pluginDisplayName(code, locale);
  const retry = () => {
    // Back to the generic pieces until the plugin reports again.
    applyLoaderCommand(code, "loader.show", { kind: "generic" });
    publishLoaderRetry(code);
  };
  return (
    <main className="flex min-h-full items-center justify-center bg-ink px-5 py-10">
      {state.kind === "progress" ? (
        <MissingPiece variant="loading" title={state.title ?? t("plugins.loader.installing", { name })}>
          <div
            className="mx-auto mt-5 h-2 w-72 max-w-full overflow-hidden rounded-full bg-raised"
            role="progressbar"
            aria-valuemin={0}
            aria-valuemax={100}
            aria-valuenow={state.progress}
            aria-label={state.title ?? t("plugins.loader.installing", { name })}
          >
            <div className="h-full rounded-full bg-gradient-to-r from-aqua to-sky transition-[width] duration-500" style={{ width: `${state.progress}%` }} />
          </div>
          <p className="mt-2 font-mono text-xs text-muted">{state.progress}%</p>
          <p className="mt-1.5 text-sm leading-relaxed text-muted" aria-live="polite">{state.message ?? ""}</p>
        </MissingPiece>
      ) : state.kind === "error" ? (
        <MissingPiece variant="failed" title={state.title ?? t("plugins.loader.failed", { name })}>
          <p className="mt-1.5 text-sm leading-relaxed text-rose" role="alert">{state.message}</p>
          {state.retry ? (
            <Button variant="primary" className="mt-5" onClick={retry} icon={<RotateCcw className="h-4 w-4" />}>
              {t("plugins.loader.retry")}
            </Button>
          ) : null}
        </MissingPiece>
      ) : (
        <MissingPiece variant="done" title={state.title ?? t("plugins.loader.installed", { name })} detail={state.message} />
      )}
    </main>
  );
}
