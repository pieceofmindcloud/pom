/**
 * One broken screen must not take the whole window with it.
 *
 * React unmounts the entire tree on an uncaught render error, so without a
 * boundary a single malformed payload leaves the operator staring at a blank
 * window: no sidebar, no menu, no way back except reloading and landing on the
 * same screen again. Here the shell survives, the failure is named, and
 * navigating away is enough to clear it.
 *
 * It is deliberately placed around the routed page rather than around the whole
 * app: the sidebar and the topbar are what make the error recoverable.
 */
import { Component, type ErrorInfo, type ReactNode } from "react";
import { RefreshCw, RotateCw, Unplug } from "lucide-react";
import { Button, Empty, Panel } from "./ui";
import { formatMessage, getActiveLocale } from "../i18n/locales";

interface Props {
  /** Changing this clears the error — the route path, in practice. */
  resetKey?: string;
  /**
   * What to show instead of the full panel. The chrome uses it: a broken
   * toolbar should shrink to a line, not push the app down the page.
   */
  fallback?: ReactNode;
  children: ReactNode;
}

export class ErrorBoundary extends Component<Props, { error: Error | null }> {
  state: { error: Error | null } = { error: null };

  static getDerivedStateFromError(error: Error) {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    // The stack is the only thing that says which screen died; keep it.
    console.error("A tela quebrou:", error, info.componentStack);
  }

  componentDidUpdate(previous: Props) {
    if (this.state.error && previous.resetKey !== this.props.resetKey) {
      this.setState({ error: null });
    }
  }

  render() {
    const { error } = this.state;
    const t = (key: string) => formatMessage(getActiveLocale(), key);
    if (!error) return this.props.children;
    if (this.props.fallback !== undefined) return this.props.fallback;

    return (
      <Panel>
        <Empty
          icon={<Unplug className="h-5 w-5" />}
          title={t("errorBoundary.title")}
          action={
            <div className="flex flex-wrap justify-center gap-2">
              <Button
                variant="primary"
                icon={<RotateCw className="h-3.5 w-3.5" />}
                onClick={() => this.setState({ error: null })}
              >
                {t("errorBoundary.retry")}
              </Button>
              {/* Rendering again cannot fix a screen whose code failed to load:
                  `lazy` keeps the rejected import and throws it again. Only a
                  fresh page fetches the module anew - and the macOS window has
                  no browser reload button to do it. */}
              <Button
                icon={<RefreshCw className="h-3.5 w-3.5" />}
                onClick={() => window.location.reload()}
              >
                {t("errorBoundary.reload")}
              </Button>
            </div>
          }
        >
          {t("errorBoundary.detail")}
        </Empty>
        <pre className="overflow-x-auto border-t border-line px-5 py-3 font-mono text-xs leading-relaxed text-faint">
          {error.message || String(error)}
        </pre>
      </Panel>
    );
  }
}
